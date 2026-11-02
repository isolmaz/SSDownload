//! Small bottom-right cards: one download's mini view, the panel of running
//! downloads and the completion card with its open actions. Cards stack above the
//! taskbar, follow the colour scheme and can be pinned on top.

use super::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

pub(super) const CLASS_CARD: &str = "SSDownload.Card";
const TIMER_CARD: usize = 7;
/// A completion card leaves by itself after this long unless the cursor is on it.
const DONE_LIFETIME: Duration = Duration::from_secs(12);
const CARD_WIDTH: i32 = 340;
const HEADER: i32 = 34;
const ROW: i32 = 54;
const FOOTER: i32 = 40;
const PANEL_ROWS: usize = 5;
/// Modern's card, in epx: its width, the padding at its sides, the header, one
/// download's row and the footer under the rows, and the size of a target the
/// pointer hits (a header glyph, a pause button, a footer button).
const MODERN_WIDTH: i32 = 360;
const MODERN_PAD: i32 = 16;
const MODERN_HEADER: i32 = 48;
const MODERN_ROW: i32 = 66;
const MODERN_FOOTER: i32 = 48;
const MODERN_TARGET: i32 = 32;

fn card_width(modern: bool) -> i32 {
    if modern {
        MODERN_WIDTH
    } else {
        CARD_WIDTH
    }
}

/// The height of a Modern card that lists `rows` downloads.
fn modern_height(rows: i32, dpi: u32) -> i32 {
    scale(MODERN_HEADER + MODERN_ROW * rows + MODERN_FOOTER, dpi)
}

#[derive(Clone)]
pub(super) enum CardKind {
    /// The downloads a job view was minimized from.
    Jobs(Vec<String>),
    /// Every running or waiting download.
    Panel,
    /// One completed download.
    Done(String),
}

#[derive(Clone)]
enum CardAction {
    Toggle(String),
    PauseAll,
    OpenFile(String),
    OpenFolder(String),
    Expand,
    Pin,
    Close,
}

struct CardUi {
    app: App,
    owner: HWND,
    hwnd: HWND,
    kind: CardKind,
    dpi: u32,
    font: HFONT,
    bold: HFONT,
    /// Modern's Caption font; null in Classic.
    small: HFONT,
    pinned: bool,
    /// A completion card the user pinned stays until it is closed.
    keep: bool,
    opened: Instant,
    hover: bool,
    hits: Vec<(RECT, CardAction)>,
    /// Modern: the hit under the pointer and the one it is pressed on.
    hot: Option<usize>,
    down: Option<usize>,
}

thread_local! {
    static CARDS: std::cell::RefCell<Vec<HWND>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) unsafe fn register_card_class(instance: HINSTANCE, icon: HICON) -> bool {
    let name = wide(CLASS_CARD);
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
        lpfnWndProc: Some(card_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: icon,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: name.as_ptr(),
        hIconSm: icon,
    };
    RegisterClassExW(&class) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS
}

/// Whether an open card already shows what `kind` would. A card minimized from one download
/// shows that download's completion itself, so it stands in for its completion card.
fn shows(open: &CardKind, kind: &CardKind) -> bool {
    match (open, kind) {
        (CardKind::Panel, CardKind::Panel) => true,
        (CardKind::Jobs(a), CardKind::Jobs(b)) => a == b,
        (CardKind::Done(a), CardKind::Done(b)) => a == b,
        (CardKind::Jobs(jobs), CardKind::Done(job)) => {
            matches!(jobs.as_slice(), [only] if only == job)
        }
        _ => false,
    }
}

/// Opens a card; a card that already shows the same downloads is raised instead of
/// duplicated.
pub(super) unsafe fn open_card(owner: HWND, app: App, kind: CardKind) {
    let existing = CARDS.with(|cards| cards.borrow().clone());
    for hwnd in existing {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardUi;
        if raw.is_null() {
            continue;
        }
        if shows(&(*raw).kind, &kind) {
            (*raw).opened = Instant::now();
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            InvalidateRect(hwnd, null(), 1);
            return;
        }
    }
    let dpi = primary_dpi();
    let pinned = matches!(kind, CardKind::Done(_));
    let (font, bold, small) = if modern() {
        (
            ramp_font(dpi, BODY_PX, false),
            ramp_font_weight(dpi, BODY_PX, false, FW_SEMIBOLD as i32),
            ramp_font(dpi, CAPTION_PX, true),
        )
    } else {
        (
            make_font(dpi, 9, FW_NORMAL as i32),
            make_font(dpi, 9, FW_SEMIBOLD as i32),
            null_mut(),
        )
    };
    let raw = Box::into_raw(Box::new(CardUi {
        app,
        owner,
        hwnd: null_mut(),
        kind,
        dpi,
        font,
        bold,
        small,
        pinned,
        keep: false,
        opened: Instant::now(),
        hover: false,
        hits: Vec::new(),
        hot: None,
        down: None,
    }));
    let height = card_height(&*raw);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | if pinned { WS_EX_TOPMOST } else { 0 },
        wide(CLASS_CARD).as_ptr(),
        wide("SSDownload").as_ptr(),
        WS_POPUP,
        0,
        0,
        scale(card_width(modern()), dpi),
        height,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        raw.cast(),
    );
    if hwnd.is_null() {
        delete_card_fonts(&*raw);
        drop(Box::from_raw(raw));
        return;
    }
    // Windows 11 rounds the corners; older builds ignore the attribute.
    let corner: u32 = 2;
    DwmSetWindowAttribute(
        hwnd,
        33,
        (&corner as *const u32).cast(),
        size_of::<u32>() as u32,
    );
    CARDS.with(|cards| cards.borrow_mut().push(hwnd));
    trim_done_cards(hwnd);
    stack_cards();
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, TIMER_CARD, 500, None);
}

/// DPI of the monitor the cards are stacked on (the primary one). The window
/// that opened a card may sit on another monitor with another scale, which
/// would draw the card too large or too small where it actually appears.
unsafe fn primary_dpi() -> u32 {
    let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
    let (mut x, mut y) = (0u32, 0u32);
    if !monitor.is_null() && GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) == 0 {
        x.max(96)
    } else {
        GetDpiForSystem().max(96)
    }
}

/// The downloads a card shows right now.
fn card_jobs<'a>(kind: &CardKind, snapshot: &'a AppSnapshot) -> Vec<&'a Job> {
    match kind {
        CardKind::Jobs(ids) => ids
            .iter()
            .filter_map(|id| snapshot.jobs.iter().find(|job| &job.id == id))
            .collect(),
        CardKind::Done(id) => snapshot.jobs.iter().filter(|job| &job.id == id).collect(),
        CardKind::Panel => snapshot
            .jobs
            .iter()
            .filter(|job| in_panel(job))
            .take(PANEL_ROWS)
            .collect(),
    }
}

/// Whether the mini panel lists a download: running or waiting, not finished.
fn in_panel(job: &Job) -> bool {
    matches!(
        job.state,
        JobState::Connecting
            | JobState::Downloading
            | JobState::Processing
            | JobState::Queued
            | JobState::Paused
    )
}

unsafe fn card_height(card: &CardUi) -> i32 {
    let snapshot = card.app.snapshot();
    let rows = card_jobs(&card.kind, &snapshot).len().max(1) as i32;
    if modern() {
        modern_height(rows, card.dpi)
    } else {
        scale(HEADER + ROW * rows + FOOTER, card.dpi)
    }
}

unsafe fn delete_card_fonts(card: &CardUi) {
    for font in [card.font, card.bold, card.small] {
        if !font.is_null() {
            DeleteObject(font as HGDIOBJ);
        }
    }
}

/// A burst of finished downloads would stack cards past the top of the screen,
/// out of reach. The oldest completion cards the user did not keep go first, so
/// the newest stay on screen; `newest` is never closed here.
unsafe fn trim_done_cards(newest: HWND) {
    let mut area: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut area as *mut RECT).cast(), 0);
    let available = area.bottom - area.top;
    loop {
        let cards = CARDS.with(|cards| cards.borrow().clone());
        let mut used = 0;
        let mut oldest = None;
        for hwnd in cards {
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardUi;
            if raw.is_null() {
                continue;
            }
            used += card_height(&*raw) + scale(12, (*raw).dpi);
            if oldest.is_none()
                && hwnd != newest
                && !(*raw).keep
                && matches!((*raw).kind, CardKind::Done(_))
            {
                oldest = Some(hwnd);
            }
        }
        match oldest {
            Some(hwnd) if used > available => close_card(hwnd),
            _ => break,
        }
    }
}

/// Stacks every open card upward from the bottom-right corner of the work area.
unsafe fn stack_cards() {
    let mut area: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut area as *mut RECT).cast(), 0);
    let cards = CARDS.with(|cards| cards.borrow().clone());
    let mut bottom = area.bottom;
    for hwnd in cards {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardUi;
        if raw.is_null() {
            continue;
        }
        let gap = scale(12, (*raw).dpi);
        let width = scale(card_width(modern()), (*raw).dpi);
        let height = card_height(&*raw);
        let top = bottom - gap - height;
        SetWindowPos(
            hwnd,
            if (*raw).pinned {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            },
            area.right - gap - width,
            top,
            width,
            height,
            SWP_NOACTIVATE,
        );
        bottom = top;
    }
}

unsafe fn colours(dark: bool) -> (u32, u32, u32, u32, u32, u32) {
    // (surface, text, muted, track, accent, line) as COLORREF (BGR).
    if dark {
        (0x1e1c1c, 0xf7f5f5, 0xa6a1a1, 0x3c3a3a, 0xff9729, 0x3a3838)
    } else {
        (0xffffff, 0x1f1d1d, 0x736e6e, 0xeae5e5, 0xe37100, 0xe0dddd)
    }
}

fn state_colour(job: &Job, accent: u32) -> u32 {
    match job.state {
        JobState::Completed => 0x59c734,
        JobState::Failed | JobState::Cancelled => 0x303bff,
        JobState::Paused | JobState::AwaitingSource => 0x938e8e,
        _ => accent,
    }
}

unsafe fn text_at(dc: HDC, font: HFONT, colour: u32, text: &str, rect: RECT, flags: u32) {
    let previous = SelectObject(dc, font as HGDIOBJ);
    SetTextColor(dc, colour);
    SetBkMode(dc, TRANSPARENT as i32);
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    let mut area = rect;
    DrawTextW(
        dc,
        buffer.as_mut_ptr(),
        buffer.len() as i32,
        &mut area,
        // A one-character glyph is never shortened to an ellipsis.
        flags
            | DT_SINGLELINE
            | DT_NOPREFIX
            | if text.chars().count() > 1 {
                DT_END_ELLIPSIS
            } else {
                DT_NOCLIP
            },
    );
    SelectObject(dc, previous);
}

unsafe fn fill(dc: HDC, rect: RECT, colour: u32) {
    let brush = CreateSolidBrush(colour);
    FillRect(dc, &rect, brush);
    DeleteObject(brush as HGDIOBJ);
}

#[derive(Clone, Copy)]
enum Glyph {
    Close,
    Pin,
    Pinned,
    Expand,
    Pause,
    Play,
}

/// Small line icons drawn as smoothed vector shapes, centred in `rect`; no icon
/// font is needed.
unsafe fn draw_glyph(dc: HDC, glyph: Glyph, rect: RECT, colour: u32, dpi: u32) {
    let r = scale(5, dpi);
    let half = r + 3;
    // The centre lies on a pixel corner, so even strokes keep sharp straight
    // edges. `e` is how far a glyph reaches from it.
    let (c, r) = (half as f32, r as f32);
    let e = r + 1.0;
    let stroke = scale(2, dpi).max(1) as f32;
    let shapes = match glyph {
        Glyph::Close => vec![
            Shape::Line((c - r, c - r), (c + r, c + r), stroke),
            Shape::Line((c + r, c - r), (c - r, c + r), stroke),
        ],
        Glyph::Pin | Glyph::Pinned => {
            let head = [c - e + 0.5, c - e - 2.0, c + e - 0.5, c + e - 3.0];
            vec![
                if matches!(glyph, Glyph::Pinned) {
                    Shape::Ellipse(head)
                } else {
                    Shape::Ring(head, stroke)
                },
                Shape::Rect(
                    [
                        c - stroke / 2.0,
                        head[3] - stroke,
                        c + stroke / 2.0,
                        c + e + 2.0,
                    ],
                    0.0,
                ),
            ]
        }
        Glyph::Expand => vec![Shape::Frame([c - e, c - e, c + e, c + e], 0.0, stroke)],
        Glyph::Pause => {
            let width = scale(3, dpi) as f32;
            vec![
                Shape::Rect([c - e, c - e, c - e + width, c + e], 0.0),
                Shape::Rect([c + e - width, c - e, c + e, c + e], 0.0),
            ]
        }
        Glyph::Play => vec![Shape::Triangle([
            (c - r, c - e),
            (c + e + 1.0, c),
            (c - r, c + e),
        ])],
    };
    paint_shapes(
        dc,
        (rect.left + rect.right) / 2 - half,
        (rect.top + rect.bottom) / 2 - half,
        2 * half,
        &shapes,
        colour,
    );
}

/// The card's title: what it shows.
fn card_title(kind: &CardKind, snapshot: &AppSnapshot) -> String {
    match kind {
        CardKind::Done(_) => crate::i18n::ui("İndirme tamamlandı", "Download complete").to_string(),
        CardKind::Panel => {
            // The rows are capped; the title still counts every listed download.
            let total = snapshot.jobs.iter().filter(|job| in_panel(job)).count();
            crate::i18n::ui_format!("İndirmeler ({total})", "Downloads ({total})", total = total)
        }
        CardKind::Jobs(_) => "SSDownload".to_string(),
    }
}

/// The line under a download's bar.
fn card_detail(job: &Job) -> String {
    match job.state {
        JobState::Completed => crate::i18n::ui_format!(
            "Tamamlandı · {0}",
            "Completed · {0}",
            format_bytes(job.downloaded)
        ),
        JobState::Downloading | JobState::Processing | JobState::Connecting => format!(
            "{} · {} · {}",
            format_progress(job),
            format_speed(job.speed),
            format_eta(job.eta)
        ),
        _ => state_label(job),
    }
}

/// The colour of the line under a download's bar.
fn detail_colour(palette: &MainPalette, state: JobState) -> u32 {
    match state {
        JobState::Completed => palette.success,
        JobState::Failed | JobState::Cancelled => palette.critical,
        _ => palette.secondary,
    }
}

/// The footer's buttons of a card that lists `jobs`, with what each does.
fn card_buttons(kind: &CardKind, jobs: &[&Job]) -> Vec<(String, CardAction)> {
    match kind {
        CardKind::Done(id) => vec![
            (
                crate::i18n::ui("Aç", "Open").to_string(),
                CardAction::OpenFile(id.clone()),
            ),
            (
                crate::i18n::ui("Klasörde göster", "Show in folder").to_string(),
                CardAction::OpenFolder(id.clone()),
            ),
        ],
        CardKind::Panel => vec![(
            crate::i18n::ui("Tümünü duraklat", "Pause all").to_string(),
            CardAction::PauseAll,
        )],
        CardKind::Jobs(ids) => {
            let done = jobs
                .iter()
                .find(|job| job.state == JobState::Completed)
                .map(|job| job.id.clone());
            match done {
                Some(id) if ids.len() == 1 => vec![
                    (
                        crate::i18n::ui("Aç", "Open").to_string(),
                        CardAction::OpenFile(id.clone()),
                    ),
                    (
                        crate::i18n::ui("Klasörde göster", "Show in folder").to_string(),
                        CardAction::OpenFolder(id),
                    ),
                ],
                _ => vec![(
                    crate::i18n::ui("Pencereyi aç", "Open window").to_string(),
                    CardAction::Expand,
                )],
            }
        }
    }
}

/// What a Modern card paints: the downloads and the state it shows.
struct ModernCard<'a> {
    kind: &'a CardKind,
    snapshot: &'a AppSnapshot,
    dpi: u32,
    dark: bool,
    /// The pin is shown as taken.
    held: bool,
    hot: Option<usize>,
    down: Option<usize>,
    body: HFONT,
    strong: HFONT,
    small: HFONT,
}

/// The plate under a glyph button the pointer is on or presses.
unsafe fn paint_plate(
    dc: HDC,
    rect: &RECT,
    (hot, down): (bool, bool),
    palette: &MainPalette,
    dpi: u32,
) {
    let colour = if down {
        palette.row_hover
    } else if hot {
        palette.hover
    } else {
        return;
    };
    round_rect(dc, rect, scale(CORNER, dpi), colour, None);
}

/// Paints a card in the Modern look into `client` of `dc`: a flyout surface with
/// a thin border, a header with the title and the pin, expand and close
/// buttons, a row for each download (its name, a progress bar, its state) and
/// the footer's buttons. Returns the targets the pointer hits.
unsafe fn paint_modern(dc: HDC, client: RECT, card: &ModernCard) -> Vec<(RECT, CardAction)> {
    let d = card.dpi;
    let s = |value: i32| scale(value, d);
    let palette = scheme_palette(card.dark);
    let surface = palette.flyout;
    let controls = on_card(
        MainPalette {
            card: surface,
            ..palette
        },
        card.dark,
    );
    fill_colour(dc, &client, surface);
    let border = over(if card.dark { 0x33000000 } else { 0x0f000000 }, surface);
    for edge in [
        RECT {
            bottom: client.top + 1,
            ..client
        },
        RECT {
            top: client.bottom - 1,
            ..client
        },
        RECT {
            right: client.left + 1,
            ..client
        },
        RECT {
            left: client.right - 1,
            ..client
        },
    ] {
        fill_colour(dc, &edge, border);
    }
    let mut hits: Vec<(RECT, CardAction)> = Vec::new();
    let pad = s(MODERN_PAD);
    let target = s(MODERN_TARGET);
    let jobs = card_jobs(card.kind, card.snapshot);
    let done = matches!(card.kind, CardKind::Done(_));

    // The header: a completion's mark, the title and the buttons at the end.
    let mut left = pad;
    if done {
        let size = s(20);
        let unit = size as f32 / 20.0;
        let stroke = (1.6 * unit).max(1.5);
        let on_success = if card.dark { 0x000000 } else { 0xffffff };
        paint_shapes(
            dc,
            pad,
            s(14),
            size,
            &[Shape::Ellipse([0.0, 0.0, size as f32, size as f32])],
            palette.success,
        );
        paint_shapes(
            dc,
            pad,
            s(14),
            size,
            &[
                Shape::Line((5.5 * unit, 10.5 * unit), (8.8 * unit, 13.8 * unit), stroke),
                Shape::Line((8.8 * unit, 13.8 * unit), (14.5 * unit, 6.8 * unit), stroke),
            ],
            on_success,
        );
        left = pad + size + s(8);
    }
    let mut glyphs = vec![(Glyph::Close, CardAction::Close, palette.secondary)];
    glyphs.push((
        if card.held { Glyph::Pinned } else { Glyph::Pin },
        CardAction::Pin,
        if card.held {
            link_colour(&controls)
        } else {
            palette.secondary
        },
    ));
    if !done {
        glyphs.push((Glyph::Expand, CardAction::Expand, palette.secondary));
    }
    let mut right = client.right - s(8);
    for (symbol, action, colour) in glyphs {
        let rect = RECT {
            left: right - target,
            top: s(8),
            right,
            bottom: s(8) + target,
        };
        let index = hits.len();
        let state = (card.hot == Some(index), card.down == Some(index));
        paint_plate(dc, &rect, state, &palette, d);
        draw_glyph(dc, symbol, rect, colour, d);
        hits.push((rect, action));
        right -= target;
    }
    let title = RECT {
        left,
        top: s(8),
        right: right - s(4),
        bottom: s(8) + target,
    };
    text_at(
        dc,
        card.strong,
        palette.text,
        &card_title(card.kind, card.snapshot),
        title,
        DT_LEFT | DT_VCENTER,
    );

    // One row for each download.
    let mut y = s(MODERN_HEADER);
    if jobs.is_empty() {
        let message = RECT {
            left: pad,
            top: y,
            right: client.right - pad,
            bottom: y + s(24),
        };
        text_at(
            dc,
            card.body,
            palette.secondary,
            crate::i18n::ui("Etkin indirme yok.", "No active downloads."),
            message,
            DT_LEFT | DT_VCENTER,
        );
        y += s(MODERN_ROW);
    }
    let width = client.right - pad - pad;
    for job in &jobs {
        let toggles = !done && can_pause_or_resume(job);
        let name = RECT {
            left: pad,
            top: y,
            right: client.right - pad - if toggles { s(24) } else { 0 },
            bottom: y + s(24),
        };
        text_at(
            dc,
            card.strong,
            palette.text,
            &job.name,
            name,
            DT_LEFT | DT_VCENTER,
        );
        if toggles {
            // The glyph ends at the padding; the target reaches past it.
            let rect = RECT {
                left: client.right - pad + s(8) - target,
                top: y + s(12) - target / 2,
                right: client.right - pad + s(8),
                bottom: y + s(12) - target / 2 + target,
            };
            let resume = matches!(
                job.state,
                JobState::Paused | JobState::Failed | JobState::Cancelled
            );
            let index = hits.len();
            let state = (card.hot == Some(index), card.down == Some(index));
            paint_plate(dc, &rect, state, &palette, d);
            draw_glyph(
                dc,
                if resume { Glyph::Play } else { Glyph::Pause },
                rect,
                palette.text,
                d,
            );
            hits.push((rect, CardAction::Toggle(job.id.clone())));
        }
        // A 1 px track with a 3 px rounded fill on it, as the list's bar.
        let line = s(1).max(1);
        let thickness = s(3).max(3);
        let middle = y + s(32);
        fill_colour(
            dc,
            &RECT {
                left: pad,
                top: middle - line / 2,
                right: pad + width,
                bottom: middle - line / 2 + line,
            },
            palette.strong_stroke,
        );
        let (start, end) = if job.state == JobState::Processing {
            processing_sweep(sweep_clock(), width)
        } else {
            let fraction = progress_fraction(job.state, job.downloaded, job.total).unwrap_or(0.0);
            (0, (width as f64 * fraction).round() as i32)
        };
        if end > start {
            round_rect(
                dc,
                &RECT {
                    left: pad + start,
                    top: middle - thickness / 2,
                    right: pad + end,
                    bottom: middle - thickness / 2 + thickness,
                },
                thickness / 2,
                state_fill(&palette, job.state),
                None,
            );
        }
        let colour = detail_colour(&palette, job.state);
        let detail = RECT {
            left: pad,
            top: y + s(38),
            right: client.right - pad,
            bottom: y + s(54),
        };
        text_at(
            dc,
            card.small,
            colour,
            &card_detail(job),
            detail,
            DT_LEFT | DT_VCENTER,
        );
        y += s(MODERN_ROW);
    }

    // The footer: the actions, the one that opens the file in the accent colour.
    let ring = s(FOCUS_MARGIN);
    let mut x = pad;
    for (label, action) in card_buttons(card.kind, &jobs) {
        let characters: Vec<u16> = label.encode_utf16().collect();
        let previous = SelectObject(dc, card.body as HGDIOBJ);
        let text_width = label_width(dc, &label);
        SelectObject(dc, previous);
        let face = RECT {
            left: x,
            top: y,
            right: x + (text_width + 2 * s(16)).max(s(80)),
            bottom: y + target,
        };
        let index = hits.len();
        paint_button(
            dc,
            inflate(face, ring, ring),
            d,
            &controls,
            card.body,
            &characters,
            None,
            ButtonState {
                primary: matches!(action, CardAction::OpenFile(_)),
                disabled: false,
                pressed: card.down == Some(index),
                hot: card.hot == Some(index),
                focused: false,
                underline: false,
            },
        );
        hits.push((face, action));
        x = face.right + s(8);
    }
    hits
}

unsafe fn paint_modern_card(card: &mut CardUi, dc: HDC) {
    let snapshot = card.app.snapshot();
    let mut client: RECT = zeroed();
    GetClientRect(card.hwnd, &mut client);
    let hits = paint_modern(
        dc,
        client,
        &ModernCard {
            kind: &card.kind,
            snapshot: &snapshot,
            dpi: card.dpi,
            dark: snapshot.settings.dark_mode,
            // A completion card is always on top; its pin means "stay until closed".
            held: if matches!(card.kind, CardKind::Done(_)) {
                card.keep
            } else {
                card.pinned
            },
            hot: card.hot,
            down: card.down,
            body: card.font,
            strong: card.bold,
            small: card.small,
        },
    );
    card.hits = hits;
}

unsafe fn paint_card(card: &mut CardUi, dc: HDC) {
    if modern() {
        paint_modern_card(card, dc);
        return;
    }
    let snapshot = card.app.snapshot();
    let dark = snapshot.settings.dark_mode;
    let (surface, text, muted, track, accent, line) = colours(dark);
    let d = card.dpi;
    let mut client: RECT = zeroed();
    GetClientRect(card.hwnd, &mut client);
    fill(dc, client, surface);
    // One-pixel frame for the light scheme, where the shadow alone is faint.
    let frame = CreateSolidBrush(line);
    FrameRect(dc, &client, frame);
    DeleteObject(frame as HGDIOBJ);
    card.hits.clear();
    let pad = scale(14, d);
    let jobs = card_jobs(&card.kind, &snapshot);
    let title = card_title(&card.kind, &snapshot);
    let header = RECT {
        left: pad,
        top: 0,
        right: client.right - pad - scale(72, d),
        bottom: scale(HEADER, d),
    };
    text_at(dc, card.bold, text, &title, header, DT_LEFT | DT_VCENTER);
    // Header glyphs: expand (job and panel cards), pin, close.
    let glyph = scale(24, d);
    let mut x = client.right - pad + scale(6, d) - glyph;
    let glyphs: Vec<(Glyph, CardAction, u32)> = {
        let mut list = vec![(Glyph::Close, CardAction::Close, muted)];
        // A completion card is always on top; its pin means "stay until closed".
        let held = if matches!(card.kind, CardKind::Done(_)) {
            card.keep
        } else {
            card.pinned
        };
        list.push((
            if held { Glyph::Pinned } else { Glyph::Pin },
            CardAction::Pin,
            if held { accent } else { muted },
        ));
        if !matches!(card.kind, CardKind::Done(_)) {
            list.push((Glyph::Expand, CardAction::Expand, muted));
        }
        list
    };
    for (symbol, action, colour) in glyphs {
        let rect = RECT {
            left: x,
            top: scale(5, d),
            right: x + glyph,
            bottom: scale(5, d) + glyph,
        };
        draw_glyph(dc, symbol, rect, colour, d);
        card.hits.push((rect, action));
        x -= glyph;
    }
    let mut y = scale(HEADER, d);
    if jobs.is_empty() {
        let rect = RECT {
            left: pad,
            top: y,
            right: client.right - pad,
            bottom: y + scale(ROW, d),
        };
        text_at(
            dc,
            card.font,
            muted,
            crate::i18n::ui("Etkin indirme yok.", "No active downloads."),
            rect,
            DT_LEFT | DT_VCENTER,
        );
    }
    for job in &jobs {
        let name = RECT {
            left: pad,
            top: y + scale(6, d),
            right: client.right - pad - scale(30, d),
            bottom: y + scale(24, d),
        };
        text_at(dc, card.bold, text, &job.name, name, DT_LEFT | DT_VCENTER);
        if !matches!(card.kind, CardKind::Done(_)) && can_pause_or_resume(job) {
            let rect = RECT {
                left: client.right - pad - scale(26, d),
                top: y + scale(4, d),
                right: client.right - pad + scale(2, d),
                bottom: y + scale(28, d),
            };
            let symbol = if matches!(
                job.state,
                JobState::Paused | JobState::Failed | JobState::Cancelled
            ) {
                Glyph::Play
            } else {
                Glyph::Pause
            };
            draw_glyph(dc, symbol, rect, accent, d);
            card.hits.push((rect, CardAction::Toggle(job.id.clone())));
        }
        let bar = RECT {
            left: pad,
            top: y + scale(28, d),
            right: client.right - pad,
            bottom: y + scale(32, d),
        };
        fill(dc, bar, track);
        let filled = if job.state == JobState::Processing {
            let (start, end) = processing_sweep(sweep_clock(), bar.right - bar.left);
            RECT {
                left: bar.left + start,
                right: bar.left + end,
                ..bar
            }
        } else {
            let fraction = match (job.state, job.total) {
                (JobState::Completed, _) => 1.0,
                (_, Some(total)) if total > 0 => (job.downloaded as f64 / total as f64).min(1.0),
                _ => 0.0,
            };
            RECT {
                right: bar.left + ((bar.right - bar.left) as f64 * fraction) as i32,
                ..bar
            }
        };
        fill(dc, filled, state_colour(job, accent));
        let detail = card_detail(job);
        let detail_rect = RECT {
            left: pad,
            top: y + scale(34, d),
            right: client.right - pad,
            bottom: y + scale(50, d),
        };
        text_at(
            dc,
            card.font,
            muted,
            &detail,
            detail_rect,
            DT_LEFT | DT_VCENTER,
        );
        y += scale(ROW, d);
    }
    // Footer actions.
    let footer_top = y + scale(4, d);
    let button_h = scale(28, d);
    let buttons = card_buttons(&card.kind, &jobs);
    let mut bx = pad;
    for (label, action) in buttons {
        let width = scale(14, d) + scale(7, d) * label.chars().count() as i32;
        let rect = RECT {
            left: bx,
            top: footer_top,
            right: bx + width,
            bottom: footer_top + button_h,
        };
        let brush = CreateSolidBrush(track);
        let region = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            scale(14, d),
            scale(14, d),
        );
        FillRgn(dc, region, brush);
        DeleteObject(region as HGDIOBJ);
        DeleteObject(brush as HGDIOBJ);
        text_at(dc, card.font, text, &label, rect, DT_CENTER | DT_VCENTER);
        card.hits.push((rect, action));
        bx = rect.right + scale(8, d);
    }
}

/// The client point a mouse message carries.
fn point_of(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam & 0xffff) as i16 as i32,
        y: ((lparam >> 16) & 0xffff) as i16 as i32,
    }
}

/// The index of the target under `point`.
fn hit_at(hits: &[(RECT, CardAction)], point: POINT) -> Option<usize> {
    hits.iter()
        .position(|(rect, _)| unsafe { PtInRect(rect, point) } != 0)
}

unsafe fn close_card(hwnd: HWND) {
    KillTimer(hwnd, TIMER_CARD);
    DestroyWindow(hwnd);
}

unsafe fn run_action(card: &mut CardUi, action: CardAction) {
    match action {
        CardAction::Close => close_card(card.hwnd),
        CardAction::Pin => {
            if matches!(card.kind, CardKind::Done(_)) {
                // Already on top; the pin only decides whether it stays.
                card.keep = !card.keep;
            } else {
                card.pinned = !card.pinned;
                card.keep = card.pinned;
                stack_cards();
            }
            InvalidateRect(card.hwnd, null(), 0);
        }
        CardAction::PauseAll => {
            let _ = card.app.dispatch(Action::PauseAll);
        }
        CardAction::Toggle(id) => {
            let snapshot = card.app.snapshot();
            if let Some(job) = snapshot.jobs.iter().find(|job| job.id == id) {
                let resume = matches!(
                    job.state,
                    JobState::Paused | JobState::Failed | JobState::Cancelled
                );
                let _ = card.app.dispatch(if resume {
                    Action::Resume { id }
                } else {
                    Action::Pause { id }
                });
            }
        }
        CardAction::OpenFile(id) => {
            let _ = card.app.dispatch(Action::OpenFile { id });
            close_card(card.hwnd);
        }
        CardAction::OpenFolder(id) => {
            let _ = card.app.dispatch(Action::OpenFolder { id: Some(id) });
            close_card(card.hwnd);
        }
        CardAction::Expand => {
            let ids = match &card.kind {
                CardKind::Jobs(ids) => ids.clone(),
                _ => Vec::new(),
            };
            if ids.is_empty() {
                let _ = card.app.dispatch(Action::ShowWindow);
            } else {
                open_job_view(card.owner, card.app.clone(), ids);
            }
            close_card(card.hwnd);
        }
    }
}

unsafe extern "system" fn card_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam as *const CREATESTRUCTW);
        let raw = cs.lpCreateParams as *mut CardUi;
        (*raw).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
    }
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardUi;
    if raw.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let card = &mut *raw;
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            // Double-buffered so the half-second refresh never flickers.
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            let memory = CreateCompatibleDC(dc);
            // A mirrored card paints its buffer mirrored too, so the copy keeps text readable.
            SetLayout(memory, GetLayout(dc));
            let bitmap = CreateCompatibleBitmap(dc, client.right, client.bottom);
            let old = SelectObject(memory, bitmap as HGDIOBJ);
            paint_card(card, memory);
            BitBlt(dc, 0, 0, client.right, client.bottom, memory, 0, 0, SRCCOPY);
            SelectObject(memory, old);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteDC(memory);
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER if wparam == TIMER_CARD => {
            let snapshot = card.app.snapshot();
            let jobs = card_jobs(&card.kind, &snapshot);
            let expired = matches!(card.kind, CardKind::Done(_))
                && !card.hover
                && !card.keep
                && card.opened.elapsed() >= DONE_LIFETIME;
            if expired || (matches!(card.kind, CardKind::Jobs(_) | CardKind::Done(_)) && jobs.is_empty()) {
                close_card(hwnd);
                return 0;
            }
            let height = card_height(card);
            let mut rect: RECT = zeroed();
            GetWindowRect(hwnd, &mut rect);
            if rect.bottom - rect.top != height {
                stack_cards();
            }
            InvalidateRect(hwnd, null(), 0);
            // A processing job's bar moves, so the card repaints often while one is shown.
            let processing = jobs.iter().any(|job| job.state == JobState::Processing);
            SetTimer(hwnd, TIMER_CARD, if processing { SWEEP_FRAME_MS } else { 500 }, None);
            0
        }
        WM_MOUSEMOVE => {
            if !card.hover {
                card.hover = true;
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
            }
            if modern() {
                let hot = hit_at(&card.hits, point_of(lparam));
                if hot != card.hot {
                    card.hot = hot;
                    InvalidateRect(hwnd, null(), 0);
                }
            }
            0
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            card.hover = false;
            card.opened = Instant::now();
            if modern() && (card.hot.is_some() || card.down.is_some()) {
                card.hot = None;
                card.down = None;
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_LBUTTONDOWN if modern() => {
            card.down = hit_at(&card.hits, point_of(lparam));
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_SETCURSOR => {
            let mut point: POINT = zeroed();
            GetCursorPos(&mut point);
            ScreenToClient(hwnd, &mut point);
            let over = card.hits.iter().any(|(rect, _)| PtInRect(rect, point) != 0);
            SetCursor(LoadCursorW(null_mut(), if over { IDC_HAND } else { IDC_ARROW }));
            1
        }
        WM_LBUTTONUP => {
            let point = point_of(lparam);
            card.down = None;
            let action = card
                .hits
                .iter()
                .find(|(rect, _)| PtInRect(rect, point) != 0)
                .map(|(_, action)| action.clone());
            if let Some(action) = action {
                run_action(card, action);
            }
            0
        }
        WM_DESTROY => {
            CARDS.with(|cards| cards.borrow_mut().retain(|value| *value != hwnd));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            delete_card_fonts(card);
            drop(Box::from_raw(raw));
            stack_cards();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_glyphs_smooth_their_slopes_and_curves_and_keep_straight_edges_sharp() {
        let counts = |glyph, dpi| {
            let pixels = unsafe {
                on_white(24, |dc| {
                    let rect = RECT {
                        left: 0,
                        top: 0,
                        right: 24,
                        bottom: 24,
                    };
                    draw_glyph(dc, glyph, rect, 0, dpi)
                })
            };
            let solid = pixels.iter().filter(|&&pixel| pixel == 0).count();
            let blended = pixels
                .iter()
                .filter(|&&pixel| pixel != 0 && pixel != 0xffffff)
                .count();
            (solid, blended)
        };
        for dpi in [96, 144] {
            for glyph in [Glyph::Close, Glyph::Pin, Glyph::Pinned, Glyph::Play] {
                let (solid, blended) = counts(glyph, dpi);
                assert!(
                    solid > 0 && blended > 0,
                    "{dpi}: {solid} solid, {blended} blended"
                );
            }
            for glyph in [Glyph::Expand, Glyph::Pause] {
                let (solid, blended) = counts(glyph, dpi);
                assert!(
                    solid > 0 && blended == 0,
                    "{dpi}: {solid} solid, {blended} blended"
                );
            }
        }
    }

    #[test]
    fn a_card_minimized_from_one_download_stands_in_for_its_completion_card() {
        let job = || "job-1".to_owned();
        assert!(shows(&CardKind::Jobs(vec![job()]), &CardKind::Done(job())));
        assert!(!shows(
            &CardKind::Jobs(vec![job()]),
            &CardKind::Done("job-2".into())
        ));
        // A card for several downloads does not show one download's completion.
        assert!(!shows(
            &CardKind::Jobs(vec![job(), "job-2".into()]),
            &CardKind::Done(job())
        ));
        // A completion card does not stand in for a running view.
        assert!(!shows(&CardKind::Done(job()), &CardKind::Jobs(vec![job()])));
        assert!(shows(&CardKind::Done(job()), &CardKind::Done(job())));
        assert!(shows(&CardKind::Panel, &CardKind::Panel));
        assert!(!shows(&CardKind::Panel, &CardKind::Done(job())));
    }

    /// A snapshot that lists `jobs`, taken from an app on a temporary data directory.
    fn snapshot_of(jobs: Vec<Job>) -> AppSnapshot {
        static BASE: std::sync::OnceLock<AppSnapshot> = std::sync::OnceLock::new();
        let base = BASE.get_or_init(|| {
            let root =
                std::env::temp_dir().join(format!("ssdownload-card-{}", uuid::Uuid::new_v4()));
            let paths = crate::paths::AppPaths::new(root.clone()).unwrap();
            let app = crate::app::App::open(paths).unwrap();
            let snapshot = app.snapshot();
            app.shutdown();
            for _ in 0..10 {
                if std::fs::remove_dir_all(&root).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            snapshot
        });
        AppSnapshot {
            jobs: std::sync::Arc::new(jobs),
            ..base.clone()
        }
    }

    fn job(name: &str, state: JobState, downloaded: u64, total: Option<u64>) -> Job {
        Job {
            downloaded,
            total,
            ..super::super::tests::sample_job(name, state)
        }
    }

    fn running(name: &str) -> Job {
        job(name, JobState::Downloading, 5, Some(10))
    }

    /// A DIB pixel (0x00RRGGBB) as the palette writes it (a COLORREF).
    fn colorref(pixel: u32) -> u32 {
        ((pixel & 0xff) << 16) | (pixel & 0xff00) | (pixel >> 16)
    }

    /// A Modern card painted on a DIB: its pixels, size, targets and palette.
    struct Shot {
        pixels: Vec<u32>,
        width: i32,
        height: i32,
        hits: Vec<(RECT, CardAction)>,
        palette: MainPalette,
        dpi: u32,
    }

    impl Shot {
        fn at(&self, x: i32, y: i32) -> u32 {
            colorref(self.pixels[(y * self.width + x) as usize])
        }

        fn s(&self, epx: i32) -> i32 {
            scale(epx, self.dpi)
        }

        /// The palette of the controls on the card's surface.
        fn controls(&self, dark: bool) -> MainPalette {
            on_card(
                MainPalette {
                    card: self.palette.flyout,
                    ..self.palette
                },
                dark,
            )
        }
    }

    /// `kind` over `jobs`; `pointer` is the target the pointer is on and the one it presses.
    fn shoot(
        kind: &CardKind,
        jobs: Vec<Job>,
        dpi: u32,
        dark: bool,
        pointer: (Option<usize>, Option<usize>),
    ) -> Shot {
        unsafe {
            let snapshot = snapshot_of(jobs);
            let rows = card_jobs(kind, &snapshot).len().max(1) as i32;
            let (width, height) = (scale(MODERN_WIDTH, dpi), modern_height(rows, dpi));
            let client = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            let fonts = [
                ramp_font(dpi, BODY_PX, false),
                ramp_font_weight(dpi, BODY_PX, false, FW_SEMIBOLD as i32),
                ramp_font(dpi, CAPTION_PX, true),
            ];
            let mut hits = Vec::new();
            let pixels = on_white_rect(width, height, |dc| {
                hits = paint_modern(
                    dc,
                    client,
                    &ModernCard {
                        kind,
                        snapshot: &snapshot,
                        dpi,
                        dark,
                        held: false,
                        hot: pointer.0,
                        down: pointer.1,
                        body: fonts[0],
                        strong: fonts[1],
                        small: fonts[2],
                    },
                );
            });
            for font in fonts {
                DeleteObject(font as HGDIOBJ);
            }
            Shot {
                pixels,
                width,
                height,
                hits,
                palette: scheme_palette(dark),
                dpi,
            }
        }
    }

    fn action_name(action: &CardAction) -> &'static str {
        match action {
            CardAction::Toggle(_) => "toggle",
            CardAction::PauseAll => "pause all",
            CardAction::OpenFile(_) => "open file",
            CardAction::OpenFolder(_) => "open folder",
            CardAction::Expand => "expand",
            CardAction::Pin => "pin",
            CardAction::Close => "close",
        }
    }

    /// A card of each kind with the downloads it lists and the targets it paints, in order.
    fn card_samples() -> Vec<(CardKind, Vec<Job>, Vec<&'static str>)> {
        let long = "an-extremely-long-file-name-for-a-download-that-has-to-be-shortened.tar.gz";
        vec![
            (
                CardKind::Panel,
                vec![
                    running(long),
                    job("paused.iso", JobState::Paused, 3, Some(10)),
                ],
                vec!["close", "pin", "expand", "toggle", "toggle", "pause all"],
            ),
            (
                CardKind::Panel,
                Vec::new(),
                vec!["close", "pin", "expand", "pause all"],
            ),
            (
                CardKind::Done("done.bin".into()),
                vec![job("done.bin", JobState::Completed, 10, Some(10))],
                vec!["close", "pin", "open file", "open folder"],
            ),
            (
                CardKind::Jobs(vec!["a.bin".into(), "b.bin".into()]),
                vec![running("a.bin"), job("b.bin", JobState::Paused, 1, Some(9))],
                vec!["close", "pin", "expand", "toggle", "toggle", "expand"],
            ),
            (
                CardKind::Jobs(vec!["c.bin".into()]),
                vec![job("c.bin", JobState::Completed, 10, Some(10))],
                vec!["close", "pin", "expand", "open file", "open folder"],
            ),
        ]
    }

    #[test]
    fn a_modern_card_fits_its_targets_in_every_language_scheme_and_scale() {
        for info in crate::i18n::LANGUAGES {
            crate::i18n::with_language(info.code, || {
                for dark in [false, true] {
                    for dpi in [96, 144] {
                        for (kind, jobs, expected) in card_samples() {
                            let shot = shoot(&kind, jobs, dpi, dark, (None, None));
                            let context =
                                format!("{} dark={dark} dpi={dpi} {expected:?}", info.code);
                            let names: Vec<_> = shot
                                .hits
                                .iter()
                                .map(|(_, action)| action_name(action))
                                .collect();
                            assert_eq!(names, expected, "{context}");
                            let (target, pad) = (shot.s(MODERN_TARGET), shot.s(MODERN_PAD));
                            for (index, (rect, action)) in shot.hits.iter().enumerate() {
                                let edges = (rect.left, rect.top, rect.right, rect.bottom);
                                let context =
                                    format!("{context} {} {edges:?}", action_name(action));
                                assert!(
                                    rect.left >= 0
                                        && rect.top >= 0
                                        && rect.right <= shot.width
                                        && rect.bottom <= shot.height,
                                    "{context}: leaves the card"
                                );
                                assert_eq!(rect.bottom - rect.top, target, "{context}");
                                let footer = rect.top >= shot.s(MODERN_HEADER)
                                    && !matches!(action, CardAction::Toggle(_));
                                if footer {
                                    assert!(rect.left >= pad, "{context}");
                                    assert!(rect.right <= shot.width - pad, "{context}");
                                    assert!(rect.bottom <= shot.height - shot.s(8), "{context}");
                                    assert!(rect.right - rect.left >= shot.s(80), "{context}");
                                } else {
                                    assert_eq!(rect.right - rect.left, target, "{context}");
                                }
                                let middle = POINT {
                                    x: (rect.left + rect.right) / 2,
                                    y: (rect.top + rect.bottom) / 2,
                                };
                                assert_eq!(hit_at(&shot.hits, middle), Some(index), "{context}");
                            }
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn a_modern_card_is_the_flyout_surface_inside_a_one_pixel_border() {
        for dark in [false, true] {
            for dpi in [96, 144] {
                let (kind, jobs, _) = card_samples().remove(2);
                let shot = shoot(&kind, jobs, dpi, dark, (None, None));
                let (w, h) = (shot.width, shot.height);
                let surface = shot.palette.flyout;
                let context = format!("dark={dark} dpi={dpi}");
                // Right of the footer's buttons nothing is painted.
                let empty = shot.at(w - shot.s(MODERN_PAD) - 4, h - shot.s(MODERN_FOOTER) / 2);
                assert_eq!(empty, surface, "{context}");
                let border = shot.at(0, h / 2);
                assert_ne!(border, surface, "{context}");
                for (x, y) in [(w - 1, h / 2), (w / 2, 0), (w / 2, h - 1), (0, 0)] {
                    assert_eq!(shot.at(x, y), border, "{context} ({x}, {y})");
                }
                for (x, y) in [(1, h / 2), (w - 2, h / 2), (w / 2, 1), (w / 2, h - 2)] {
                    assert_eq!(shot.at(x, y), surface, "{context} ({x}, {y})");
                }
                // The border is a visible step off the surface.
                assert!(contrast(border, surface) >= 1.05, "{context}");
            }
        }
    }

    /// The pixel a few epx in from a target's left edge: inside its plate, clear of its glyph.
    fn beside_glyph(shot: &Shot, rect: &RECT) -> (i32, i32) {
        (rect.left + shot.s(3), (rect.top + rect.bottom) / 2)
    }

    /// Whether every pixel of `a` and `b` that differs lies inside `area`.
    fn differs_only_in(a: &Shot, b: &Shot, area: RECT) -> bool {
        (0..a.height).all(|y| {
            (0..a.width).all(|x| {
                let inside = x >= area.left && x < area.right && y >= area.top && y < area.bottom;
                inside || a.at(x, y) == b.at(x, y)
            })
        })
    }

    #[test]
    fn a_pointer_on_a_glyph_target_paints_its_plate_and_a_press_a_stronger_one() {
        let (kind, jobs, _) = card_samples().remove(0);
        for dark in [false, true] {
            for dpi in [96, 144] {
                let shoot_at = |pointer| shoot(&kind, jobs.clone(), dpi, dark, pointer);
                let rest = shoot_at((None, None));
                let palette = rest.palette;
                assert!(
                    palette.hover != palette.row_hover && palette.hover != palette.flyout,
                    "the plates differ from each other and from the surface"
                );
                // The close button, and the first row's pause button.
                for index in [0, 3] {
                    let rect = rest.hits[index].0;
                    let (x, y) = beside_glyph(&rest, &rect);
                    let hot = shoot_at((Some(index), None));
                    let down = shoot_at((Some(index), Some(index)));
                    let context = format!("dark={dark} dpi={dpi} target {index}");
                    assert_eq!(rest.at(x, y), palette.flyout, "{context}");
                    assert_eq!(hot.at(x, y), palette.hover, "{context}");
                    assert_eq!(down.at(x, y), palette.row_hover, "{context}");
                    assert!(differs_only_in(&rest, &hot, rect), "{context}");
                    assert!(differs_only_in(&rest, &down, rect), "{context}");
                }
            }
        }
    }

    #[test]
    fn a_footer_button_takes_its_hover_and_pressed_face_and_the_primary_one_the_accent() {
        let (kind, jobs, _) = card_samples().remove(2);
        for dark in [false, true] {
            for dpi in [96, 144] {
                let shoot_at = |pointer| shoot(&kind, jobs.clone(), dpi, dark, pointer);
                let rest = shoot_at((None, None));
                let controls = rest.controls(dark);
                let ring = rest.s(FOCUS_MARGIN);
                // Open (the primary action) and Show in folder.
                let [open, folder] = [2, 3].map(|index| rest.hits[index].0);
                let context = format!("dark={dark} dpi={dpi}");
                let (x, y) = beside_glyph(&rest, &open);
                assert_eq!(rest.at(x, y), rest.palette.accent, "{context}");
                assert_eq!(
                    shoot_at((Some(2), None)).at(x, y),
                    controls.accent_hover,
                    "{context}"
                );
                assert_eq!(
                    shoot_at((Some(2), Some(2))).at(x, y),
                    controls.accent_pressed,
                    "{context}"
                );
                let (x, y) = beside_glyph(&rest, &folder);
                assert_eq!(rest.at(x, y), controls.control, "{context}");
                let hot = shoot_at((Some(3), None));
                let down = shoot_at((Some(3), Some(3)));
                assert_eq!(hot.at(x, y), controls.control_hover, "{context}");
                assert_eq!(down.at(x, y), controls.control_pressed, "{context}");
                let area = inflate(folder, ring, ring);
                assert!(differs_only_in(&rest, &hot, area), "{context}");
                assert!(differs_only_in(&rest, &down, area), "{context}");
            }
        }
    }

    #[test]
    fn only_a_completion_card_carries_the_success_mark() {
        for dark in [false, true] {
            for dpi in [96, 144] {
                let marked = |index: usize| {
                    let (kind, jobs, _) = card_samples().remove(index);
                    let shot = shoot(&kind, jobs, dpi, dark, (None, None));
                    let header = shot.s(MODERN_HEADER);
                    let success = shot.palette.success;
                    (0..header)
                        .flat_map(|y| (0..shot.width).map(move |x| (x, y)))
                        .filter(|&(x, y)| shot.at(x, y) == success)
                        .count()
                };
                let context = format!("dark={dark} dpi={dpi}");
                // The disc is 20 epx across; its tick is cut out of it.
                assert!(marked(2) > 150, "{context}: {}", marked(2));
                for index in [0, 1, 3, 4] {
                    assert_eq!(marked(index), 0, "{context} sample {index}");
                }
            }
        }
    }

    #[test]
    fn a_downloads_bar_fills_to_its_progress_in_its_state_colour() {
        for dark in [false, true] {
            for dpi in [96, 144] {
                let bar = |kind: &CardKind, job: Job| {
                    let shot = shoot(kind, vec![job], dpi, dark, (None, None));
                    let y = shot.s(MODERN_HEADER) + shot.s(32);
                    let width = shot.width - 2 * shot.s(MODERN_PAD);
                    let at = |fraction: f64| {
                        shot.at(shot.s(MODERN_PAD) + (width as f64 * fraction) as i32, y)
                    };
                    (at(0.25), at(0.75), shot.palette)
                };
                let context = format!("dark={dark} dpi={dpi}");
                let (early, late, palette) = bar(&CardKind::Panel, running("half.bin"));
                assert_eq!(
                    early, palette.accent,
                    "{context}: a running bar fills from the left"
                );
                assert_eq!(
                    late, palette.strong_stroke,
                    "{context}: the rest is the track"
                );
                let done = job("full.bin", JobState::Completed, 10, Some(10));
                let (early, late, _) = bar(&CardKind::Done("full.bin".into()), done);
                assert_eq!(
                    (early, late),
                    (palette.success, palette.success),
                    "{context}"
                );
                let failed = job("broken.bin", JobState::Failed, 3, Some(10));
                let (early, late, _) = bar(&CardKind::Jobs(vec!["broken.bin".into()]), failed);
                assert_eq!(
                    (early, late),
                    (palette.critical, palette.critical),
                    "{context}"
                );
                let paused = job("held.bin", JobState::Paused, 5, Some(10));
                let (early, late, _) = bar(&CardKind::Panel, paused);
                assert_eq!(early, palette.caution, "{context}");
                assert_eq!(late, palette.strong_stroke, "{context}");
            }
        }
    }

    #[test]
    fn a_detail_line_reads_on_the_card_surface_in_every_state_and_scheme() {
        let states = [
            JobState::Queued,
            JobState::Scheduled,
            JobState::Connecting,
            JobState::Downloading,
            JobState::Processing,
            JobState::Paused,
            JobState::AwaitingSource,
            JobState::Completed,
            JobState::Failed,
            JobState::Cancelled,
        ];
        for dark in [false, true] {
            let palette = unsafe { scheme_palette(dark) };
            for state in states {
                let colour = detail_colour(&palette, state);
                assert!(
                    contrast(colour, palette.flyout) >= 4.5,
                    "dark={dark} {state:?}: {colour:06x} on {:06x}",
                    palette.flyout
                );
            }
            assert_eq!(
                detail_colour(&palette, JobState::Completed),
                palette.success
            );
            assert_eq!(detail_colour(&palette, JobState::Failed), palette.critical);
            assert_eq!(
                detail_colour(&palette, JobState::Cancelled),
                palette.critical
            );
            assert_eq!(
                detail_colour(&palette, JobState::Downloading),
                palette.secondary
            );
            assert_eq!(detail_colour(&palette, JobState::Paused), palette.secondary);
        }
    }
}
