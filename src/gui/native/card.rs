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

#[derive(Clone, PartialEq, Eq)]
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
    /// Modern: the target the pointer last moved onto, which tells when to repaint,
    /// and where the pointer pressed with the action it pressed on.
    hot: Option<usize>,
    press: Option<(POINT, CardAction)>,
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
        press: None,
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
    dpi_at(POINT { x: 0, y: 0 })
}

/// DPI of the monitor `point` is on, of the nearest one when it is on none.
pub(super) unsafe fn dpi_at(point: POINT) -> u32 {
    let monitor = MonitorFromPoint(
        point,
        windows_sys::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
    );
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

/// The colours a Modern card paints with.
#[derive(Clone, Copy)]
struct CardColours {
    palette: MainPalette,
    /// The palette of the footer's buttons, which sit on the flyout surface.
    controls: MainPalette,
    border: u32,
    /// The plates under a glyph button the pointer is on and under one it presses.
    plate: (u32, u32),
    /// A glyph's colour on its plate where the plate is the system highlight.
    on_plate: Option<u32>,
    /// The check mark on the completion disc.
    mark: u32,
}

/// The colours of a card in the scheme `dark` names, from `palette`. High
/// contrast keeps the system colours: its border is the text colour, a plate is
/// the highlight and a glyph on it the highlight's text.
fn card_colours(dark: bool, palette: MainPalette, high: bool) -> CardColours {
    let surface = palette.flyout;
    if high {
        return CardColours {
            controls: MainPalette {
                base: surface,
                ..palette
            },
            border: palette.text,
            plate: (palette.hover, palette.control_pressed),
            on_plate: Some(palette.on_accent),
            mark: palette.on_accent,
            palette,
        };
    }
    CardColours {
        controls: on_card(
            MainPalette {
                card: surface,
                ..palette
            },
            dark,
        ),
        border: over(if dark { 0x33000000 } else { 0x0f000000 }, surface),
        plate: flyout_plates(dark, surface),
        on_plate: None,
        mark: if dark { 0x000000 } else { 0xffffff },
        palette,
    }
}

/// What a Modern card paints: the downloads and the state it shows.
struct ModernCard<'a> {
    kind: &'a CardKind,
    snapshot: &'a AppSnapshot,
    dpi: u32,
    colours: CardColours,
    /// The pin is shown as taken.
    held: bool,
    /// Where the pointer is on the card, and where it pressed, in client pixels.
    pointer: Option<POINT>,
    press: Option<POINT>,
    body: HFONT,
    strong: HFONT,
    small: HFONT,
}

/// The plate under a glyph button the pointer is on or presses.
unsafe fn paint_plate(
    dc: HDC,
    rect: &RECT,
    (hot, down): (bool, bool),
    colours: &CardColours,
    dpi: u32,
) {
    let colour = if down {
        colours.plate.1
    } else if hot {
        colours.plate.0
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
    let colours = &card.colours;
    let palette = colours.palette;
    let controls = colours.controls;
    let surface = palette.flyout;
    fill_colour(dc, &client, surface);
    // The pointer is on a target, and presses it while it is still on it.
    let state = |rect: &RECT| {
        let hot = card.pointer.is_some_and(|point| PtInRect(rect, point) != 0);
        let down = hot && card.press.is_some_and(|point| PtInRect(rect, point) != 0);
        (hot, down)
    };
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
        fill_colour(dc, &edge, colours.border);
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
            colours.mark,
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
        let (hot, down) = state(&rect);
        paint_plate(dc, &rect, (hot, down), colours, d);
        let colour = colours.on_plate.filter(|_| hot).unwrap_or(colour);
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
            let (hot, down) = state(&rect);
            paint_plate(dc, &rect, (hot, down), colours, d);
            draw_glyph(
                dc,
                if resume { Glyph::Play } else { Glyph::Pause },
                rect,
                colours.on_plate.filter(|_| hot).unwrap_or(palette.text),
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
        let (hot, down) = state(&face);
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
                pressed: down,
                hot,
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
    // The pointer is read as the card paints: a row that left moves what is under it.
    let pointer = card.hover.then(|| {
        let mut point: POINT = zeroed();
        GetCursorPos(&mut point);
        ScreenToClient(card.hwnd, &mut point);
        point
    });
    let dark = snapshot.settings.dark_mode;
    let hits = paint_modern(
        dc,
        client,
        &ModernCard {
            kind: &card.kind,
            snapshot: &snapshot,
            dpi: card.dpi,
            colours: card_colours(dark, scheme_palette(dark), high_contrast()),
            // A completion card is always on top; its pin means "stay until closed".
            held: if matches!(card.kind, CardKind::Done(_)) {
                card.keep
            } else {
                card.pinned
            },
            pointer,
            press: card.press.as_ref().map(|(point, _)| *point),
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

/// The target a release at `up` completes a click on: the one under it, when
/// the press began on the same action.
fn clicked(hits: &[(RECT, CardAction)], pressed: Option<&CardAction>, up: POINT) -> Option<usize> {
    hit_at(hits, up).filter(|&index| pressed == Some(&hits[index].1))
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
            if modern() {
                card.hot = None;
                card.press = None;
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_LBUTTONDOWN if modern() => {
            let point = point_of(lparam);
            card.press = hit_at(&card.hits, point).map(|index| (point, card.hits[index].1.clone()));
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
            let pressed = card.press.take();
            // Modern clicks what the press began on; Classic acts on the release alone.
            let target = if modern() {
                InvalidateRect(hwnd, null(), 0);
                clicked(
                    &card.hits,
                    pressed.as_ref().map(|(_, action)| action),
                    point,
                )
            } else {
                hit_at(&card.hits, point)
            };
            if let Some(index) = target {
                let action = card.hits[index].1.clone();
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

    /// An app on a temporary data directory, and that directory.
    fn temp_app() -> (crate::app::App, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("ssdownload-card-{}", uuid::Uuid::new_v4()));
        let paths = crate::paths::AppPaths::new(root.clone()).unwrap();
        (crate::app::App::open(paths).unwrap(), root)
    }

    /// Stops `app` and removes its data directory.
    fn drop_app(app: crate::app::App, root: &std::path::Path) {
        app.shutdown();
        for _ in 0..10 {
            if std::fs::remove_dir_all(root).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A snapshot that lists `jobs`.
    fn snapshot_of(jobs: Vec<Job>) -> AppSnapshot {
        static BASE: std::sync::OnceLock<AppSnapshot> = std::sync::OnceLock::new();
        let base = BASE.get_or_init(|| {
            let (app, root) = temp_app();
            let snapshot = app.snapshot();
            drop_app(app, &root);
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

    /// A Modern card painted on a DIB: its pixels, size, targets and colours.
    struct Shot {
        pixels: Vec<u32>,
        width: i32,
        height: i32,
        hits: Vec<(RECT, CardAction)>,
        colours: CardColours,
        dpi: u32,
    }

    impl Shot {
        fn at(&self, x: i32, y: i32) -> u32 {
            colorref(self.pixels[(y * self.width + x) as usize])
        }

        fn s(&self, epx: i32) -> i32 {
            scale(epx, self.dpi)
        }

        /// The middle of the target `index`.
        fn middle(&self, index: usize) -> POINT {
            let rect = self.hits[index].0;
            POINT {
                x: (rect.left + rect.right) / 2,
                y: (rect.top + rect.bottom) / 2,
            }
        }
    }

    /// Where the pointer is on a card and where it pressed.
    type Pointer = (Option<POINT>, Option<POINT>);

    /// `kind` over `jobs` painted in `colours`.
    fn shoot_in(
        kind: &CardKind,
        jobs: Vec<Job>,
        dpi: u32,
        colours: CardColours,
        (pointer, press): Pointer,
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
                        colours,
                        held: false,
                        pointer,
                        press,
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
                colours,
                dpi,
            }
        }
    }

    /// `shoot_in` in the colours of the scheme `dark` names.
    fn shoot(kind: &CardKind, jobs: Vec<Job>, dpi: u32, dark: bool, pointer: Pointer) -> Shot {
        let colours = card_colours(dark, unsafe { scheme_palette(dark) }, false);
        shoot_in(kind, jobs, dpi, colours, pointer)
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

    /// The colours of four high contrast themes, as COLORREFs (0x00BBGGRR): white on
    /// black, black on white, and a light and a dark one with a coloured highlight.
    fn high_contrast_themes() -> [SystemColours; 4] {
        [
            SystemColours {
                window: 0x000000,
                text: 0xffffff,
                face: 0x000000,
                button_text: 0xffffff,
                highlight: 0xffeb1a,
                highlight_text: 0x000000,
                gray_text: 0x3ff23f,
            },
            SystemColours {
                window: 0xffffff,
                text: 0x000000,
                face: 0xffffff,
                button_text: 0x000000,
                highlight: 0x6e0037,
                highlight_text: 0xffffff,
                gray_text: 0x000060,
            },
            SystemColours {
                window: 0x202020,
                text: 0xffffff,
                face: 0x202020,
                button_text: 0xffffff,
                highlight: 0xefe38e,
                highlight_text: 0x000000,
                gray_text: 0x3ff23f,
            },
            SystemColours {
                window: 0xffffff,
                text: 0x000000,
                face: 0xffffff,
                button_text: 0x000000,
                highlight: 0x527f03,
                highlight_text: 0xffffff,
                gray_text: 0x000060,
            },
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
                                assert_eq!(
                                    hit_at(&shot.hits, shot.middle(index)),
                                    Some(index),
                                    "{context}"
                                );
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
                let surface = shot.colours.palette.flyout;
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
                let (hover, pressed) = rest.colours.plate;
                let surface = rest.colours.palette.flyout;
                let context = format!("dark={dark} dpi={dpi}");
                // Each plate is a visible step off the flyout it lies on, a press a stronger one.
                assert!(contrast(hover, surface) >= 1.05, "{context}: hover");
                assert!(contrast(pressed, surface) >= 1.1, "{context}: press");
                assert!(
                    contrast(pressed, surface) > contrast(hover, surface),
                    "{context}"
                );
                // The close button, and the first row's pause button.
                for index in [0, 3] {
                    let rect = rest.hits[index].0;
                    let (x, y) = beside_glyph(&rest, &rect);
                    let middle = Some(rest.middle(index));
                    let hot = shoot_at((middle, None));
                    let down = shoot_at((middle, middle));
                    let context = format!("{context} target {index}");
                    assert_eq!(rest.at(x, y), surface, "{context}");
                    assert_eq!(hot.at(x, y), hover, "{context}");
                    assert_eq!(down.at(x, y), pressed, "{context}");
                    assert!(differs_only_in(&rest, &hot, rect), "{context}");
                    assert!(differs_only_in(&rest, &down, rect), "{context}");
                    // A press shows only while the pointer is still on the pressed target.
                    let off = Some(rest.middle(2));
                    assert!(differs_only_in(
                        &rest,
                        &shoot_at((off, middle)),
                        rest.hits[2].0
                    ));
                }
            }
        }
    }

    #[test]
    fn the_plate_follows_the_pointer_when_a_row_leaves_the_card() {
        let (kind, jobs, _) = card_samples().remove(0);
        let leaving = running("leaving.bin");
        let both = vec![leaving, jobs[1].clone()];
        for dark in [false, true] {
            let with_both = shoot(&kind, both.clone(), 96, dark, (None, None));
            // The pointer is on the second row's pause button; that row moves up.
            let pointer = Some(with_both.middle(4));
            let after = shoot(&kind, vec![jobs[1].clone()], 96, dark, (pointer, None));
            let rest = shoot(&kind, vec![jobs[1].clone()], 96, dark, (None, None));
            // Nothing of the card is under the pointer now, so no plate is painted anywhere.
            assert!(
                differs_only_in(
                    &rest,
                    &after,
                    RECT {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0
                    }
                ),
                "dark={dark}"
            );
            // And the pointer on the target that moved under it paints that target's plate.
            let moved = shoot(
                &kind,
                vec![jobs[1].clone()],
                96,
                dark,
                (Some(rest.middle(3)), None),
            );
            let (x, y) = beside_glyph(&rest, &rest.hits[3].0);
            assert_eq!(moved.at(x, y), rest.colours.plate.0, "dark={dark}");
        }
    }

    #[test]
    fn a_footer_button_takes_its_hover_and_pressed_face_and_the_primary_one_the_accent() {
        let (kind, jobs, _) = card_samples().remove(2);
        for dark in [false, true] {
            for dpi in [96, 144] {
                let shoot_at = |pointer| shoot(&kind, jobs.clone(), dpi, dark, pointer);
                let rest = shoot_at((None, None));
                let controls = rest.colours.controls;
                let ring = rest.s(FOCUS_MARGIN);
                // Open (the primary action) and Show in folder.
                let [open, folder] = [2, 3].map(|index| rest.hits[index].0);
                let context = format!("dark={dark} dpi={dpi}");
                let (x, y) = beside_glyph(&rest, &open);
                assert_eq!(rest.at(x, y), rest.colours.palette.accent, "{context}");
                let on_open = Some(rest.middle(2));
                assert_eq!(
                    shoot_at((on_open, None)).at(x, y),
                    controls.accent_hover,
                    "{context}"
                );
                assert_eq!(
                    shoot_at((on_open, on_open)).at(x, y),
                    controls.accent_pressed,
                    "{context}"
                );
                let (x, y) = beside_glyph(&rest, &folder);
                assert_eq!(rest.at(x, y), controls.control, "{context}");
                let on_folder = Some(rest.middle(3));
                let hot = shoot_at((on_folder, None));
                let down = shoot_at((on_folder, on_folder));
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
                    let success = shot.colours.palette.success;
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
                    (at(0.25), at(0.75), shot.colours.palette)
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

    #[test]
    fn the_check_mark_reads_on_the_success_disc_in_both_schemes() {
        for dark in [false, true] {
            let colours = card_colours(dark, unsafe { scheme_palette(dark) }, false);
            assert!(
                contrast(colours.mark, colours.palette.success) >= 4.5,
                "dark={dark}"
            );
        }
    }

    #[test]
    fn high_contrast_keeps_the_system_colours_on_every_part_of_a_card() {
        for theme in high_contrast_themes() {
            for dark in [false, true] {
                let colours = card_colours(dark, system_palette(&theme), true);
                let context = format!(
                    "dark={dark} window={:06x} highlight={:06x}",
                    theme.window, theme.highlight
                );
                // The buttons keep the face the system gives them, which its button text reads on.
                assert_eq!(colours.controls.base, theme.window, "{context}");
                assert_eq!(colours.controls.control, theme.face, "{context}");
                assert!(
                    contrast(theme.button_text, colours.controls.control) >= 4.5,
                    "{context}"
                );
                // The border is the text colour, which the window reads against.
                assert_eq!(colours.border, theme.text, "{context}");
                assert!(contrast(colours.border, theme.window) >= 4.5, "{context}");
                // A plate is the highlight, under a glyph in the highlight's text.
                let on_plate = colours
                    .on_plate
                    .expect("a glyph on a plate has its own colour");
                for plate in [colours.plate.0, colours.plate.1] {
                    assert_eq!(plate, theme.highlight, "{context}");
                    assert!(contrast(plate, theme.window) >= 3.0, "{context}");
                    assert!(contrast(on_plate, plate) >= 4.5, "{context}");
                }
                // The check mark reads on the disc the highlight fills.
                assert!(
                    contrast(colours.mark, colours.palette.success) >= 4.5,
                    "{context}"
                );
            }
        }
    }

    #[test]
    fn a_card_in_high_contrast_paints_its_border_plate_glyph_and_mark_in_those_colours() {
        let (kind, jobs, _) = card_samples().remove(2);
        for theme in high_contrast_themes() {
            for dpi in [96, 144] {
                let colours = card_colours(false, system_palette(&theme), true);
                let shoot_at = |pointer| shoot_in(&kind, jobs.clone(), dpi, colours, pointer);
                let rest = shoot_at((None, None));
                let context = format!(
                    "dpi={dpi} window={:06x} highlight={:06x}",
                    theme.window, theme.highlight
                );
                assert_eq!(rest.at(0, rest.height / 2), theme.text, "{context}: border");
                assert_eq!(
                    rest.at(1, rest.height / 2),
                    theme.window,
                    "{context}: surface"
                );
                // The disc is the highlight and the tick on it the highlight's text.
                let unit = rest.s(20) as f32 / 20.0;
                let pad = rest.s(MODERN_PAD);
                let disc = (pad + (10.0 * unit) as i32, rest.s(14) + (3.0 * unit) as i32);
                let tick = (pad + (8.8 * unit) as i32, rest.s(14) + (13.8 * unit) as i32);
                assert_eq!(rest.at(disc.0, disc.1), theme.highlight, "{context}: disc");
                assert_eq!(
                    rest.at(tick.0, tick.1),
                    theme.highlight_text,
                    "{context}: tick"
                );
                // The pointer on the close button fills its plate with the highlight and draws
                // the glyph in the highlight's text; a press keeps that look.
                let on_close = Some(rest.middle(0));
                for pointer in [(on_close, None), (on_close, on_close)] {
                    let shot = shoot_at(pointer);
                    let rect = shot.hits[0].0;
                    let (x, y) = beside_glyph(&shot, &rect);
                    assert_eq!(shot.at(x, y), theme.highlight, "{context}: plate");
                    let middle = shot.middle(0);
                    let reach = shot.s(6);
                    let inside = |colour: u32| {
                        (middle.y - reach..middle.y + reach)
                            .flat_map(|y| (middle.x - reach..middle.x + reach).map(move |x| (x, y)))
                            .filter(|&(x, y)| shot.at(x, y) == colour)
                            .count()
                    };
                    assert!(inside(theme.highlight_text) > 0, "{context}: glyph");
                    assert_eq!(inside(theme.text), 0, "{context}: glyph in the text colour");
                }
            }
        }
    }

    #[test]
    fn a_release_completes_a_click_only_on_the_action_the_press_began_on() {
        let (kind, jobs, _) = card_samples().remove(3);
        let shot = shoot(&kind, jobs, 96, false, (None, None));
        let [close, pin, header_expand, first, second, footer_expand] =
            [0, 1, 2, 3, 4, 5].map(|index| shot.middle(index));
        let pressed = |index: usize| shot.hits[index].1.clone();
        let nowhere = POINT {
            x: shot.width / 2,
            y: shot.s(MODERN_HEADER) + 4,
        };
        // Released where it was pressed.
        assert_eq!(clicked(&shot.hits, Some(&pressed(1)), pin), Some(1));
        assert_eq!(clicked(&shot.hits, Some(&pressed(3)), first), Some(3));
        // Released on another target, on none, or without a press on any.
        assert_eq!(clicked(&shot.hits, Some(&pressed(1)), close), None);
        assert_eq!(clicked(&shot.hits, Some(&pressed(3)), second), None);
        assert_eq!(clicked(&shot.hits, Some(&pressed(1)), nowhere), None);
        assert_eq!(clicked(&shot.hits, None, pin), None);
        // Two targets that do the same thing complete each other's click.
        assert_eq!(
            clicked(&shot.hits, Some(&pressed(2)), footer_expand),
            Some(5)
        );
        assert_eq!(
            clicked(&shot.hits, Some(&pressed(5)), header_expand),
            Some(2)
        );
        // A row that left and a pause button of another one took its place: the press
        // began on the first one's action, which the target under the release is not.
        let moved = shoot(&kind, vec![running("b.bin")], 96, false, (None, None));
        let under = moved.middle(3);
        assert_eq!(
            clicked(
                &moved.hits,
                Some(&CardAction::Toggle("a.bin".into())),
                under
            ),
            None
        );
        assert_eq!(
            clicked(
                &moved.hits,
                Some(&CardAction::Toggle("b.bin".into())),
                under
            ),
            Some(3)
        );
    }

    #[test]
    fn a_modern_card_clicks_what_the_press_began_on_and_clears_its_pressed_look_on_release() {
        use windows_sys::Win32::Graphics::Gdi::{GetUpdateRect, ValidateRect};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DestroyWindow, IsWindow, SendMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_PAINT,
        };

        super::super::tests::on_private_desktop(|| unsafe {
            super::super::set_modern(true);
            assert!(register_card_class(GetModuleHandleW(null()), null_mut()));
            let (app, root) = temp_app();
            open_card(null_mut(), app.clone(), CardKind::Panel);
            let hwnd = CARDS.with(|cards| *cards.borrow().last().expect("the card opened"));
            let card = || &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardUi);
            SendMessageW(hwnd, WM_PAINT, 0, 0);
            let at = |action: &str| {
                let hits = &card().hits;
                let index = hits
                    .iter()
                    .position(|(_, a)| action_name(a) == action)
                    .unwrap();
                let rect = hits[index].0;
                ((rect.left + rect.right) / 2) | (((rect.top + rect.bottom) / 2) << 16)
            };
            let (pin, close, expand) = (at("pin"), at("close"), at("expand"));
            let mouse = |message: u32, point: i32| {
                SendMessageW(hwnd, message, 0, point as isize);
            };
            let invalid = || {
                let mut area: RECT = zeroed();
                GetUpdateRect(hwnd, &mut area, 0) != 0
            };

            // Pressed on the pin and released on the close button, nothing runs.
            mouse(WM_LBUTTONDOWN, pin);
            assert!(card().press.is_some());
            ValidateRect(hwnd, null());
            mouse(WM_LBUTTONUP, close);
            assert_ne!(
                IsWindow(hwnd),
                0,
                "the release on another target closed the card"
            );
            assert!(card().press.is_none());
            assert!(!card().pinned);
            assert!(invalid(), "the pressed plate is painted away");

            // Released off every target, nothing runs and the pressed look goes too.
            mouse(WM_LBUTTONDOWN, pin);
            ValidateRect(hwnd, null());
            mouse(WM_LBUTTONUP, 4 | (4 << 16));
            assert!(card().press.is_none() && !card().pinned);
            assert!(invalid());

            // Pressed and released on the pin, the card is pinned.
            mouse(WM_LBUTTONDOWN, pin);
            mouse(WM_LBUTTONUP, pin);
            assert!(card().pinned && card().press.is_none());

            // A release without a press on the target does nothing in Modern.
            mouse(WM_LBUTTONUP, pin);
            assert!(card().pinned);
            mouse(WM_LBUTTONDOWN, expand);
            mouse(WM_LBUTTONUP, close);
            assert_ne!(IsWindow(hwnd), 0);

            DestroyWindow(hwnd);
            drop_app(app, &root);
        });
    }
}
