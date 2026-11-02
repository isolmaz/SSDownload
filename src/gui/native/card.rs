//! Small bottom-right cards: one download's mini view, the panel of running
//! downloads and the completion card with its open actions. Cards stack above the
//! taskbar, follow the colour scheme (the system colours in high contrast) and can
//! be pinned on top. What a card offers to press is a push button of the card whose
//! face the card draws, so the keyboard and screen readers reach it like any other.

use super::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetActiveWindow, SetActiveWindow};

pub(super) const CLASS_CARD: &str = "SSDownload.Card";
const TIMER_CARD: usize = 7;
/// A completion card leaves by itself after this long unless the user is working in it.
const DONE_LIFETIME: Duration = Duration::from_secs(12);
const CARD_WIDTH: i32 = 340;
const HEADER: i32 = 34;
const ROW: i32 = 54;
const FOOTER: i32 = 40;
const PANEL_ROWS: usize = 5;
const FIRST_CONTROL_ID: i32 = 100;

#[derive(Clone)]
pub(super) enum CardKind {
    /// The downloads a job view was minimized from.
    Jobs(Vec<String>),
    /// Every running or waiting download.
    Panel,
    /// One completed download.
    Done(String),
}

#[derive(Clone, PartialEq)]
enum CardAction {
    Toggle(String),
    PauseAll,
    OpenFile(String),
    OpenFolder(String),
    Expand,
    Pin,
    Close,
}

/// What the face of a control shows.
#[derive(Clone, PartialEq)]
enum Look {
    /// A line icon, in the colour that marks something on when the flag is set.
    Glyph(Glyph, bool),
    Label(String),
}

/// Something on a card the user can press.
#[derive(Clone)]
struct Control {
    rect: RECT,
    action: CardAction,
    /// The text of the button: what a screen reader announces.
    name: String,
    look: Look,
}

/// A control and the push button that stands for it.
struct Shown {
    control: Control,
    hwnd: HWND,
    id: i32,
}

/// What one painting of a card found: its title and the controls it has room for.
struct Painted {
    title: String,
    controls: Vec<Control>,
}

/// The colours a card is drawn in.
#[derive(Clone, Copy, PartialEq)]
struct Colours {
    surface: u32,
    text: u32,
    muted: u32,
    track: u32,
    accent: u32,
    line: u32,
    face: u32,
    face_text: u32,
    /// The glyphs that mark something on: a held pin, pause and resume.
    mark: u32,
    contrast: bool,
}

struct CardUi {
    app: App,
    owner: HWND,
    hwnd: HWND,
    kind: CardKind,
    dpi: u32,
    font: HFONT,
    bold: HFONT,
    pinned: bool,
    /// A completion card the user pinned stays until it is closed.
    keep: bool,
    opened: Instant,
    colours: Option<Colours>,
    title: String,
    controls: Vec<Shown>,
    next_id: i32,
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
    create_card(owner, app, kind, primary_dpi());
}

/// Creates a card for `dpi` and shows it without activating it; null when the window
/// cannot be created.
unsafe fn create_card(owner: HWND, app: App, kind: CardKind, dpi: u32) -> HWND {
    let pinned = matches!(kind, CardKind::Done(_));
    let raw = Box::into_raw(Box::new(CardUi {
        app,
        owner,
        hwnd: null_mut(),
        kind,
        dpi,
        font: make_font(dpi, 9, FW_NORMAL as i32),
        bold: make_font(dpi, 9, FW_SEMIBOLD as i32),
        pinned,
        keep: false,
        opened: Instant::now(),
        colours: None,
        title: String::new(),
        controls: Vec::new(),
        next_id: FIRST_CONTROL_ID,
    }));
    let height = card_height(&*raw);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | if pinned { WS_EX_TOPMOST } else { 0 },
        wide(CLASS_CARD).as_ptr(),
        wide("SSDownload").as_ptr(),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        scale(CARD_WIDTH, dpi),
        height,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        raw.cast(),
    );
    if hwnd.is_null() {
        DeleteObject((*raw).font as HGDIOBJ);
        DeleteObject((*raw).bold as HGDIOBJ);
        drop(Box::from_raw(raw));
        return null_mut();
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
    hwnd
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
    scale(HEADER + ROW * rows + FOOTER, card.dpi)
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
        let width = scale(CARD_WIDTH, (*raw).dpi);
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

/// The colours of a card as COLORREF (BGR). In high contrast they are the system's: window,
/// its text, highlight and button face.
unsafe fn colours(dark: bool, contrast: bool) -> Colours {
    if contrast {
        let text = GetSysColor(COLOR_WINDOWTEXT);
        return Colours {
            surface: GetSysColor(COLOR_WINDOW),
            text,
            muted: text,
            track: GetSysColor(COLOR_WINDOW),
            accent: GetSysColor(COLOR_HIGHLIGHT),
            line: text,
            face: GetSysColor(COLOR_BTNFACE),
            face_text: GetSysColor(COLOR_BTNTEXT),
            mark: text,
            contrast,
        };
    }
    let (surface, text, muted, track, accent, line) = if dark {
        (0x1e1c1c, 0xf7f5f5, 0xa6a1a1, 0x3c3a3a, 0xff9729, 0x3a3838)
    } else {
        (0xffffff, 0x1f1d1d, 0x736e6e, 0xeae5e5, 0xe37100, 0xe0dddd)
    };
    Colours {
        surface,
        text,
        muted,
        track,
        accent,
        line,
        face: track,
        face_text: text,
        mark: accent,
        contrast,
    }
}

fn state_colour(job: &Job, colours: &Colours) -> u32 {
    let stopped = matches!(
        job.state,
        JobState::Paused | JobState::AwaitingSource | JobState::Failed | JobState::Cancelled
    );
    if colours.contrast {
        return if stopped {
            colours.text
        } else {
            colours.accent
        };
    }
    match job.state {
        JobState::Completed => 0x59c734,
        JobState::Failed | JobState::Cancelled => 0x303bff,
        JobState::Paused | JobState::AwaitingSource => 0x938e8e,
        _ => colours.accent,
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

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Close,
    Pin,
    Pinned,
    Expand,
    Pause,
    Play,
}

/// Small line icons drawn with GDI, centred in `rect`; no icon font is needed.
unsafe fn draw_glyph(dc: HDC, glyph: Glyph, rect: RECT, colour: u32, dpi: u32) {
    let cx = (rect.left + rect.right) / 2;
    let cy = (rect.top + rect.bottom) / 2;
    let r = scale(5, dpi);
    let pen = CreatePen(PS_SOLID, scale(2, dpi).max(1), colour);
    let brush = CreateSolidBrush(colour);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);
    let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
    match glyph {
        Glyph::Close => {
            MoveToEx(dc, cx - r, cy - r, null_mut());
            LineTo(dc, cx + r + 1, cy + r + 1);
            MoveToEx(dc, cx + r, cy - r, null_mut());
            LineTo(dc, cx - r - 1, cy + r + 1);
        }
        Glyph::Pin | Glyph::Pinned => {
            if matches!(glyph, Glyph::Pinned) {
                SelectObject(dc, brush as HGDIOBJ);
            }
            Ellipse(dc, cx - r + 1, cy - r - 2, cx + r, cy + r - 3);
            MoveToEx(dc, cx, cy + r - 3, null_mut());
            LineTo(dc, cx, cy + r + 3);
        }
        Glyph::Expand => {
            Rectangle(dc, cx - r, cy - r, cx + r + 1, cy + r + 1);
        }
        Glyph::Pause => {
            let width = scale(3, dpi);
            let left = RECT {
                left: cx - r,
                top: cy - r,
                right: cx - r + width,
                bottom: cy + r + 1,
            };
            FillRect(dc, &left, brush);
            let right = RECT {
                left: cx + r + 1 - width,
                top: cy - r,
                right: cx + r + 1,
                bottom: cy + r + 1,
            };
            FillRect(dc, &right, brush);
        }
        Glyph::Play => {
            SelectObject(dc, brush as HGDIOBJ);
            let points = [
                POINT {
                    x: cx - r + 1,
                    y: cy - r,
                },
                POINT {
                    x: cx + r + 1,
                    y: cy,
                },
                POINT {
                    x: cx - r + 1,
                    y: cy + r,
                },
            ];
            Polygon(dc, points.as_ptr(), 3);
        }
    }
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    DeleteObject(pen as HGDIOBJ);
    DeleteObject(brush as HGDIOBJ);
}

/// Paints everything of a card except its controls, which are buttons of their own, and
/// returns the controls the card has room for.
unsafe fn paint_card(card: &CardUi, dc: HDC, snapshot: &AppSnapshot, colours: &Colours) -> Painted {
    let d = card.dpi;
    let mut client: RECT = zeroed();
    GetClientRect(card.hwnd, &mut client);
    fill(dc, client, colours.surface);
    // One-pixel frame for the light scheme, where the shadow alone is faint.
    let frame = CreateSolidBrush(colours.line);
    FrameRect(dc, &client, frame);
    DeleteObject(frame as HGDIOBJ);
    let mut controls = Vec::new();
    let pad = scale(14, d);
    let jobs = card_jobs(&card.kind, snapshot);
    let title = match &card.kind {
        CardKind::Done(_) => crate::i18n::ui("İndirme tamamlandı", "Download complete").to_string(),
        CardKind::Panel => {
            // The rows are capped; the title still counts every listed download.
            let total = snapshot.jobs.iter().filter(|job| in_panel(job)).count();
            crate::i18n::ui_format!("İndirmeler ({total})", "Downloads ({total})", total = total)
        }
        CardKind::Jobs(_) => "SSDownload".to_string(),
    };
    let header = RECT {
        left: pad,
        top: 0,
        right: client.right - pad - scale(72, d),
        bottom: scale(HEADER, d),
    };
    text_at(
        dc,
        card.bold,
        colours.text,
        &title,
        header,
        DT_LEFT | DT_VCENTER,
    );
    // Header glyphs: close, pin and expand (job and panel cards), right to left.
    let glyph = scale(24, d);
    let mut x = client.right - pad + scale(6, d) - glyph;
    // A completion card is always on top; its pin means "stay until closed".
    let held = if matches!(card.kind, CardKind::Done(_)) {
        card.keep
    } else {
        card.pinned
    };
    let mut glyphs = vec![
        (
            Glyph::Close,
            CardAction::Close,
            false,
            crate::i18n::ui("Kapat", "Close"),
        ),
        (
            if held { Glyph::Pinned } else { Glyph::Pin },
            CardAction::Pin,
            held,
            if held {
                crate::i18n::ui("Sabitlemeyi kaldır", "Unpin")
            } else {
                crate::i18n::ui("Sabitle", "Pin")
            },
        ),
    ];
    if !matches!(card.kind, CardKind::Done(_)) {
        glyphs.push((
            Glyph::Expand,
            CardAction::Expand,
            false,
            crate::i18n::ui("Pencereyi aç", "Open window"),
        ));
    }
    for (symbol, action, marked, name) in glyphs {
        controls.push(Control {
            rect: RECT {
                left: x,
                top: scale(5, d),
                right: x + glyph,
                bottom: scale(5, d) + glyph,
            },
            action,
            name: name.to_string(),
            look: Look::Glyph(symbol, marked),
        });
        x -= glyph;
    }
    // The controls are listed in reading order, which is the order Tab visits them in.
    controls.reverse();
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
            colours.muted,
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
        text_at(
            dc,
            card.bold,
            colours.text,
            &job.name,
            name,
            DT_LEFT | DT_VCENTER,
        );
        if !matches!(card.kind, CardKind::Done(_)) && can_pause_or_resume(job) {
            let resume = matches!(
                job.state,
                JobState::Paused | JobState::Failed | JobState::Cancelled
            );
            controls.push(Control {
                rect: RECT {
                    left: client.right - pad - scale(26, d),
                    top: y + scale(4, d),
                    right: client.right - pad + scale(2, d),
                    bottom: y + scale(28, d),
                },
                action: CardAction::Toggle(job.id.clone()),
                name: if resume {
                    crate::i18n::ui_format!("Sürdür: {0}", "Resume: {0}", job.name)
                } else {
                    crate::i18n::ui_format!("Duraklat: {0}", "Pause: {0}", job.name)
                },
                look: Look::Glyph(if resume { Glyph::Play } else { Glyph::Pause }, true),
            });
        }
        let bar = RECT {
            left: pad,
            top: y + scale(28, d),
            right: client.right - pad,
            bottom: y + scale(32, d),
        };
        fill(dc, bar, colours.track);
        if colours.contrast {
            // The track is the window's colour; the outline keeps the bar visible.
            let outline = CreateSolidBrush(colours.line);
            FrameRect(dc, &bar, outline);
            DeleteObject(outline as HGDIOBJ);
        }
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
        fill(dc, filled, state_colour(job, colours));
        let detail = match job.state {
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
        };
        let detail_rect = RECT {
            left: pad,
            top: y + scale(34, d),
            right: client.right - pad,
            bottom: y + scale(50, d),
        };
        text_at(
            dc,
            card.font,
            colours.muted,
            &detail,
            detail_rect,
            DT_LEFT | DT_VCENTER,
        );
        y += scale(ROW, d);
    }
    // Footer actions.
    let footer_top = y + scale(4, d);
    let button_h = scale(28, d);
    let buttons: Vec<(String, CardAction)> = match &card.kind {
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
    };
    let mut bx = pad;
    for (label, action) in buttons {
        let width = scale(14, d) + scale(7, d) * label.chars().count() as i32;
        let rect = RECT {
            left: bx,
            top: footer_top,
            right: bx + width,
            bottom: footer_top + button_h,
        };
        bx = rect.right + scale(8, d);
        controls.push(Control {
            rect,
            action,
            name: label.clone(),
            look: Look::Label(label),
        });
    }
    Painted { title, controls }
}

/// Draws one control on the face of its push button, when the button asks to be painted.
unsafe fn draw_control(card: &CardUi, draw: &NMCUSTOMDRAW, control: &Control) {
    let Some(colours) = card.colours else {
        return;
    };
    let dc = draw.hdc;
    let area = draw.rc;
    fill(dc, area, colours.surface);
    let mut focus = area;
    match &control.look {
        Look::Glyph(glyph, marked) => {
            let colour = if *marked { colours.mark } else { colours.muted };
            draw_glyph(dc, *glyph, area, colour, card.dpi);
        }
        Look::Label(label) => {
            let radius = scale(14, card.dpi);
            let region =
                CreateRoundRectRgn(area.left, area.top, area.right, area.bottom, radius, radius);
            let face = CreateSolidBrush(colours.face);
            FillRgn(dc, region, face);
            DeleteObject(face as HGDIOBJ);
            if colours.contrast {
                let edge = CreateSolidBrush(colours.line);
                FrameRgn(dc, region, edge, 1, 1);
                DeleteObject(edge as HGDIOBJ);
            }
            DeleteObject(region as HGDIOBJ);
            text_at(
                dc,
                card.font,
                colours.face_text,
                label,
                area,
                DT_CENTER | DT_VCENTER,
            );
            let inset = scale(3, card.dpi);
            focus = RECT {
                left: area.left + inset,
                top: area.top + inset,
                right: area.right - inset,
                bottom: area.bottom - inset,
            };
        }
    }
    let button = draw.hdr.hwndFrom;
    let held = SendMessageW(button, BM_GETSTATE, 0, 0) as u32;
    let cues = SendMessageW(button, WM_QUERYUISTATE, 0, 0) as u32;
    if held & BST_FOCUS != 0 && cues & UISF_HIDEFOCUS == 0 {
        DrawFocusRect(dc, &focus);
    }
}

fn same_rect(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// Makes the push buttons of a card the controls a painting found. A control that is still
/// there keeps its window, and with it the keyboard focus; the buttons stay in the order of
/// the list, which is the order Tab visits them in.
unsafe fn sync_controls(card: &mut CardUi, painted: Painted, colours: Colours) {
    if card.title != painted.title {
        SetWindowTextW(card.hwnd, wide(&painted.title).as_ptr());
        card.title = painted.title;
    }
    let restyle = card.colours != Some(colours);
    card.colours = Some(colours);
    let mut old = std::mem::take(&mut card.controls);
    let mut previous = HWND_TOP;
    for control in painted.controls {
        let rect = control.rect;
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        let shown = match old
            .iter()
            .position(|shown| shown.control.action == control.action)
        {
            Some(at) => {
                let mut shown = old.remove(at);
                if shown.control.name != control.name {
                    SetWindowTextW(shown.hwnd, wide(&control.name).as_ptr());
                }
                if !same_rect(&shown.control.rect, &rect) {
                    SetWindowPos(
                        shown.hwnd,
                        null_mut(),
                        rect.left,
                        rect.top,
                        width,
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                if restyle || shown.control.look != control.look {
                    InvalidateRect(shown.hwnd, null(), 0);
                }
                shown.control = control;
                shown
            }
            None => {
                let id = card.next_id;
                card.next_id += 1;
                let hwnd = super::control(
                    card.hwnd,
                    "BUTTON",
                    &control.name,
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                    0,
                    id,
                );
                SetWindowPos(
                    hwnd,
                    previous,
                    rect.left,
                    rect.top,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
                Shown { control, hwnd, id }
            }
        };
        previous = shown.hwnd;
        card.controls.push(shown);
    }
    for gone in old {
        DestroyWindow(gone.hwnd);
    }
}

/// Paints the card into `dc` through a buffer, so the refresh never flickers.
unsafe fn paint_buffered(
    card: &CardUi,
    dc: HDC,
    snapshot: &AppSnapshot,
    colours: &Colours,
) -> Painted {
    let mut client: RECT = zeroed();
    GetClientRect(card.hwnd, &mut client);
    let memory = CreateCompatibleDC(dc);
    // A mirrored card paints its buffer mirrored too, so the copy keeps text readable.
    SetLayout(memory, GetLayout(dc));
    let bitmap = CreateCompatibleBitmap(dc, client.right, client.bottom);
    let old = SelectObject(memory, bitmap as HGDIOBJ);
    let painted = paint_card(card, memory, snapshot, colours);
    BitBlt(dc, 0, 0, client.right, client.bottom, memory, 0, 0, SRCCOPY);
    SelectObject(memory, old);
    DeleteObject(bitmap as HGDIOBJ);
    DeleteDC(memory);
    painted
}

/// Activates a card that was clicked. The click is the last input of the program, so the
/// system lets the card take the foreground; where it does not, the card still becomes the
/// active window of the program.
unsafe fn activate(hwnd: HWND) {
    if SetForegroundWindow(hwnd) == 0 {
        SetActiveWindow(hwnd);
    }
}

/// Whether the user is working in the card: it is the active window, or the cursor is on it.
unsafe fn in_use(hwnd: HWND) -> bool {
    let mut area: RECT = zeroed();
    GetWindowRect(hwnd, &mut area);
    let mut cursor: POINT = zeroed();
    GetActiveWindow() == hwnd || (GetCursorPos(&mut cursor) != 0 && PtInRect(&area, cursor) != 0)
}

/// The monitor of the card changed its scale: the fonts and the size are those of the new
/// scale, and the cards are stacked again.
unsafe fn card_dpi_changed(card: &mut CardUi, dpi: u32) {
    DeleteObject(card.font as HGDIOBJ);
    DeleteObject(card.bold as HGDIOBJ);
    card.dpi = dpi;
    card.font = make_font(dpi, 9, FW_NORMAL as i32);
    card.bold = make_font(dpi, 9, FW_SEMIBOLD as i32);
    stack_cards();
    InvalidateRect(card.hwnd, null(), 0);
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
            let snapshot = card.app.snapshot();
            let colours = colours(snapshot.settings.dark_mode, high_contrast());
            let painted = paint_buffered(card, dc, &snapshot, &colours);
            EndPaint(hwnd, &ps);
            sync_controls(card, painted, colours);
            0
        }
        WM_PRINTCLIENT => {
            let snapshot = card.app.snapshot();
            let colours = colours(snapshot.settings.dark_mode, high_contrast());
            paint_buffered(card, wparam as HDC, &snapshot, &colours);
            0
        }
        WM_ERASEBKGND => 1,
        WM_NOTIFY => {
            let header = &*(lparam as *const NMHDR);
            if header.code != NM_CUSTOMDRAW {
                return 0;
            }
            let draw = &*(lparam as *const NMCUSTOMDRAW);
            if draw.dwDrawStage != CDDS_PREPAINT {
                return CDRF_DODEFAULT as LRESULT;
            }
            match card
                .controls
                .iter()
                .find(|shown| shown.hwnd == header.hwndFrom)
            {
                Some(shown) => {
                    draw_control(card, draw, &shown.control);
                    CDRF_SKIPDEFAULT as LRESULT
                }
                None => CDRF_DODEFAULT as LRESULT,
            }
        }
        WM_COMMAND => {
            let id = loword(wparam) as i32;
            if id == IDCANCEL {
                // The dialog manager reports Escape this way.
                close_card(hwnd);
            } else if let Some(action) = card
                .controls
                .iter()
                .find(|shown| shown.id == id)
                .map(|shown| shown.control.action.clone())
            {
                run_action(card, action);
            }
            0
        }
        // The dialog manager asks which button Enter presses: the one that has the focus.
        // Without an answer it sends IDOK, which a card has no button for.
        DM_GETDEFID => {
            let focus = GetFocus();
            match card.controls.iter().find(|shown| shown.hwnd == focus) {
                Some(shown) => ((DC_HASDEFID << 16) | shown.id as u32) as LRESULT,
                None => 0,
            }
        }
        // WS_EX_NOACTIVATE keeps a click from activating the card, so the click does it:
        // whoever clicks a card wants to work in it, and keys reach it only then. A card
        // never activates itself when it appears.
        WM_LBUTTONDOWN => {
            activate(hwnd);
            0
        }
        WM_PARENTNOTIFY if loword(wparam) as u32 == WM_LBUTTONDOWN => {
            activate(hwnd);
            0
        }
        WM_DPICHANGED => {
            card_dpi_changed(card, loword(wparam) as u32);
            0
        }
        WM_TIMER if wparam == TIMER_CARD => {
            let snapshot = card.app.snapshot();
            let jobs = card_jobs(&card.kind, &snapshot);
            if in_use(hwnd) {
                card.opened = Instant::now();
            }
            let expired = matches!(card.kind, CardKind::Done(_))
                && !card.keep
                && card.opened.elapsed() >= DONE_LIFETIME;
            if expired
                || (matches!(card.kind, CardKind::Jobs(_) | CardKind::Done(_)) && jobs.is_empty())
            {
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
            SetTimer(
                hwnd,
                TIMER_CARD,
                if processing { SWEEP_FRAME_MS } else { 500 },
                None,
            );
            0
        }
        WM_SETCURSOR => {
            // wparam is the window under the cursor: a control of the card or the card.
            let over = wparam as HWND != hwnd;
            SetCursor(LoadCursorW(
                null_mut(),
                if over { IDC_HAND } else { IDC_ARROW },
            ));
            1
        }
        WM_DESTROY => {
            CARDS.with(|cards| cards.borrow_mut().retain(|value| *value != hwnd));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DeleteObject(card.font as HGDIOBJ);
            DeleteObject(card.bold as HGDIOBJ);
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
    use crate::gui::native::tests::{
        on_private_desktop, register_test_classes, with_common_controls_6, ShiftHeld,
    };
    use crate::model::{Action as ModelAction, AddRequest};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SetActiveWindow, VK_ESCAPE, VK_RETURN, VK_SPACE, VK_TAB,
    };

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

    /// A card on a private desktop, over an app with one download scheduled for later, so
    /// the engine does no network I/O for it.
    struct Fixture {
        app: App,
        hwnd: HWND,
        root: std::path::PathBuf,
    }

    impl Fixture {
        /// Opens a card of `kind` for `dpi`. A mirrored card is mirrored before it paints,
        /// the way the process default layout makes it when it is created.
        unsafe fn open(kind: impl FnOnce(&str) -> CardKind, dpi: u32, mirrored: bool) -> Self {
            register_test_classes();
            let root =
                std::env::temp_dir().join(format!("ssdownload-card-{}", uuid::Uuid::new_v4()));
            let app = App::open(crate::paths::AppPaths::new(root.clone()).unwrap()).unwrap();
            let added = app
                .dispatch_result(ModelAction::Add {
                    request: AddRequest {
                        url: "https://example.invalid/file.bin".into(),
                        start_at: Some(chrono::Utc::now().timestamp() + 3_600),
                        ..Default::default()
                    },
                })
                .unwrap();
            let hwnd = create_card(null_mut(), app.clone(), kind(&added.job_ids[0]), dpi);
            assert!(!hwnd.is_null(), "the card is created");
            if mirrored {
                let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYOUTRTL as isize);
            }
            UpdateWindow(hwnd);
            Self { app, hwnd, root }
        }

        unsafe fn card(&self) -> *mut CardUi {
            GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *mut CardUi
        }

        fn job_name(&self) -> String {
            self.app.snapshot().jobs[0].name.clone()
        }

        /// The push buttons of the card in the order Tab visits them: window, text and the
        /// rectangle in the card's window.
        unsafe fn buttons(&self) -> Vec<(HWND, String, RECT)> {
            let mut card: RECT = zeroed();
            GetWindowRect(self.hwnd, &mut card);
            let mut list = Vec::new();
            let mut child = GetWindow(self.hwnd, GW_CHILD);
            while !child.is_null() {
                let mut rect: RECT = zeroed();
                GetWindowRect(child, &mut rect);
                list.push((
                    child,
                    window_text(child),
                    RECT {
                        left: rect.left - card.left,
                        top: rect.top - card.top,
                        right: rect.right - card.left,
                        bottom: rect.bottom - card.top,
                    },
                ));
                child = GetWindow(child, GW_HWNDNEXT);
            }
            list
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe {
                if IsWindow(self.hwnd) != 0 {
                    close_card(self.hwnd);
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    unsafe fn window_text(hwnd: HWND) -> String {
        let mut text = vec![0u16; 256];
        let length = GetWindowTextW(hwnd, text.as_mut_ptr(), 256).max(0) as usize;
        String::from_utf16_lossy(&text[..length])
    }

    fn with_card_in<T: Send>(
        language: &'static str,
        kind: impl FnOnce(&str) -> CardKind + Send,
        dpi: u32,
        mirrored: bool,
        body: impl FnOnce(&Fixture) -> T + Send,
    ) -> T {
        on_private_desktop(|| {
            with_common_controls_6(|| {
                crate::i18n::with_language(language, || unsafe {
                    body(&Fixture::open(kind, dpi, mirrored))
                })
            })
        })
    }

    fn with_card<T: Send>(
        kind: impl FnOnce(&str) -> CardKind + Send,
        body: impl FnOnce(&Fixture) -> T + Send,
    ) -> T {
        with_card_in("en", kind, 96, false, body)
    }

    fn jobs(job: &str) -> CardKind {
        CardKind::Jobs(vec![job.to_owned()])
    }

    fn done(job: &str) -> CardKind {
        CardKind::Done(job.to_owned())
    }

    /// A key press typed into the window that has the focus, or into the card, run through
    /// the dialog manager the way the message loops do, which name the main window; true
    /// when it was consumed.
    unsafe fn press(main: HWND, card: HWND, key: u16) -> bool {
        let mut msg: MSG = zeroed();
        msg.hwnd = if GetFocus().is_null() {
            card
        } else {
            GetFocus()
        };
        msg.message = WM_KEYDOWN;
        msg.wParam = key as usize;
        dialog_message(main, &msg)
    }

    #[test]
    fn every_control_of_a_card_has_a_name_in_every_language() {
        for info in crate::i18n::LANGUAGES.iter() {
            for kind in [jobs as fn(&str) -> CardKind, done, |_| CardKind::Panel] {
                with_card_in(info.code, kind, 96, false, |card| unsafe {
                    let buttons = card.buttons();
                    assert!(
                        buttons.len() >= 3,
                        "{}: a card has its pin, its close button and an action",
                        info.code
                    );
                    for (_, name, _) in &buttons {
                        assert!(
                            !name.trim().is_empty(),
                            "{}: a control without a name",
                            info.code
                        );
                    }
                });
            }
        }
    }

    #[test]
    fn the_controls_of_a_card_are_buttons_named_for_what_they_do_in_reading_order() {
        with_card(jobs, |card| unsafe {
            let names: Vec<String> = card.buttons().into_iter().map(|button| button.1).collect();
            assert_eq!(
                names,
                [
                    "Open window".to_owned(),
                    "Pin".into(),
                    "Close".into(),
                    format!("Pause: {}", card.job_name()),
                    "Open window".into(),
                ]
            );
        });
        with_card(done, |card| unsafe {
            let names: Vec<String> = card.buttons().into_iter().map(|button| button.1).collect();
            assert_eq!(names, ["Pin", "Close", "Open", "Show in folder"]);
            assert_eq!(
                window_text(card.hwnd),
                "Download complete",
                "the card itself is named for what it shows"
            );
        });
    }

    #[test]
    fn a_card_that_appears_does_not_take_the_focus() {
        on_private_desktop(|| {
            with_common_controls_6(|| unsafe {
                let owner = control(null_mut(), "STATIC", "owner", WS_POPUP | WS_VISIBLE, 0, 0);
                SetActiveWindow(owner);
                assert_eq!(GetActiveWindow(), owner, "the window the user works in");
                let card = Fixture::open(jobs, 96, false);
                assert!(IsWindowVisible(card.hwnd) != 0, "the card is shown");
                assert!(!card.buttons().is_empty(), "the card has its buttons");
                assert_eq!(GetActiveWindow(), owner, "the card did not take over");
                let focus = GetFocus();
                assert!(
                    focus != card.hwnd && GetAncestor(focus, GA_ROOT) != card.hwnd,
                    "no control of the card has the focus"
                );
                drop(card);
                DestroyWindow(owner);
            })
        });
    }

    /// The mouse key state of a left button message.
    const MK_LBUTTON: usize = 1;

    #[test]
    fn a_click_on_a_card_or_on_one_of_its_controls_activates_it() {
        on_private_desktop(|| {
            with_common_controls_6(|| {
                crate::i18n::with_language("en", || unsafe {
                    let other = control(null_mut(), "STATIC", "other", WS_POPUP | WS_VISIBLE, 0, 0);
                    let card = Fixture::open(jobs, 96, false);
                    let inside = 1 | (1 << 16);
                    SetActiveWindow(other);
                    assert_eq!(GetActiveWindow(), other, "another window is active");
                    SendMessageW(card.hwnd, WM_LBUTTONDOWN, MK_LBUTTON, inside);
                    assert_eq!(GetActiveWindow(), card.hwnd, "a click on the card");
                    // The system tells a card of the clicks on its controls.
                    SetActiveWindow(other);
                    SendMessageW(card.hwnd, WM_PARENTNOTIFY, WM_LBUTTONDOWN as usize, inside);
                    assert_eq!(GetActiveWindow(), card.hwnd, "a click on a control");
                    SetActiveWindow(other);
                    SendMessageW(card.hwnd, WM_PARENTNOTIFY, WM_MOUSEMOVE as usize, inside);
                    assert_eq!(GetActiveWindow(), other, "no click, no activation");
                    drop(card);
                    DestroyWindow(other);
                })
            })
        });
    }

    #[test]
    fn once_a_card_is_active_tab_enter_space_and_escape_work_in_it() {
        with_card(jobs, |card| unsafe {
            let main = control(null_mut(), "STATIC", "main", WS_POPUP, 0, 0);
            SetActiveWindow(card.hwnd);
            assert_eq!(GetActiveWindow(), card.hwnd, "the card is active");
            let buttons = card.buttons();
            // Tab walks the controls in reading order and wraps around.
            let mut visited = Vec::new();
            for _ in 0..=buttons.len() {
                assert!(
                    press(main, card.hwnd, VK_TAB),
                    "the dialog manager takes Tab"
                );
                visited.push(GetFocus());
            }
            let expected: Vec<HWND> = buttons
                .iter()
                .map(|button| button.0)
                .chain([buttons[0].0])
                .collect();
            assert_eq!(visited, expected, "Tab visits the controls in order");
            // Shift+Tab walks back, around the start.
            {
                let _shift = ShiftHeld::new();
                for back in [buttons.len() - 1, buttons.len() - 2] {
                    assert!(press(main, card.hwnd, VK_TAB), "Shift+Tab is taken");
                    assert_eq!(GetFocus(), buttons[back].0, "Shift+Tab goes back");
                }
            }
            // Enter presses the control that has the focus.
            SetFocus(buttons[1].0);
            assert!(!(*card.card()).pinned);
            assert!(press(main, card.hwnd, VK_RETURN), "Enter is taken");
            assert!((*card.card()).pinned, "Enter pressed the pin");
            // Space presses it too.
            SendMessageW(buttons[1].0, WM_KEYDOWN, VK_SPACE as usize, 0);
            SendMessageW(buttons[1].0, WM_KEYUP, VK_SPACE as usize, 0);
            assert!(!(*card.card()).pinned, "Space pressed the pin again");
            // Escape closes the card.
            assert!(press(main, card.hwnd, VK_ESCAPE), "Escape is taken");
            assert_eq!(IsWindow(card.hwnd), 0, "Escape closed the card");
            DestroyWindow(main);
        });
    }

    #[test]
    fn a_completion_card_does_not_leave_while_the_user_works_in_it() {
        with_card(done, |card| unsafe {
            let stale = || Instant::now() - DONE_LIFETIME - Duration::from_secs(1);
            (*card.card()).opened = stale();
            SetActiveWindow(card.hwnd);
            SendMessageW(card.hwnd, WM_TIMER, TIMER_CARD, 0);
            assert_ne!(IsWindow(card.hwnd), 0, "a card the user works in stays");
            let other = control(null_mut(), "STATIC", "other", WS_POPUP | WS_VISIBLE, 0, 0);
            SetActiveWindow(other);
            (*card.card()).opened = stale();
            SendMessageW(card.hwnd, WM_TIMER, TIMER_CARD, 0);
            assert_eq!(IsWindow(card.hwnd), 0, "an unattended card leaves");
            DestroyWindow(other);
        });
    }

    /// The controls of a card with their places, the size of its window and the height of
    /// its font, to compare two cards.
    type Shape = (Vec<(String, i32, i32, i32, i32)>, (i32, i32), i32);

    unsafe fn shape(card: &Fixture) -> Shape {
        use windows_sys::Win32::Graphics::Gdi::{GetObjectW, LOGFONTW};
        let mut frame: RECT = zeroed();
        GetWindowRect(card.hwnd, &mut frame);
        let controls = card
            .buttons()
            .into_iter()
            .map(|(_, name, rect)| {
                let name = name.replace(&card.job_name(), "<download>");
                (name, rect.left, rect.top, rect.right, rect.bottom)
            })
            .collect();
        let mut face: LOGFONTW = zeroed();
        GetObjectW(
            (*card.card()).font as HGDIOBJ,
            size_of::<LOGFONTW>() as i32,
            &mut face as *mut LOGFONTW as *mut _,
        );
        (
            controls,
            (frame.right - frame.left, frame.bottom - frame.top),
            face.lfHeight,
        )
    }

    #[test]
    fn a_card_moved_to_another_scale_takes_the_fonts_and_sizes_of_that_scale() {
        for (from, to) in [(96, 144), (144, 96), (96, 192)] {
            let made = with_card_in("en", jobs, to, false, |card| unsafe { shape(card) });
            let before = with_card_in("en", jobs, from, false, |card| unsafe { shape(card) });
            let moved = with_card_in("en", jobs, from, false, |card| unsafe {
                SendMessageW(card.hwnd, WM_DPICHANGED, ((to << 16) | to) as usize, 0);
                UpdateWindow(card.hwnd);
                shape(card)
            });
            assert_ne!(before, made, "{from} and {to} dpi make different cards");
            assert_eq!(
                moved, made,
                "{from} -> {to} dpi: the card is not the one made for {to} dpi"
            );
        }
    }

    #[test]
    fn a_mirrored_card_mirrors_its_controls() {
        let places = |card: &Fixture| unsafe {
            let mut frame: RECT = zeroed();
            GetWindowRect(card.hwnd, &mut frame);
            let places: Vec<(bool, RECT)> = card
                .buttons()
                .into_iter()
                .map(|(button, _, rect)| {
                    let own = GetWindowLongPtrW(button, GWL_EXSTYLE) as u32 & WS_EX_LAYOUTRTL;
                    (own != 0, rect)
                })
                .collect();
            (frame.right - frame.left, places)
        };
        let plain = with_card(jobs, places);
        let mirrored = with_card_in("ar", jobs, 96, true, places);
        let width = plain.0;
        assert_eq!(width, mirrored.0, "the card is as wide as before");
        assert_eq!(plain.1.len(), mirrored.1.len(), "the same controls");
        for (left, right) in plain.1.iter().zip(&mirrored.1) {
            assert!(
                !left.0 && right.0,
                "the controls follow the card's direction"
            );
            assert_eq!(
                (left.1.left, left.1.right),
                (width - right.1.right, width - right.1.left),
                "a control stands at the mirror image of its place"
            );
            assert_eq!((left.1.top, left.1.bottom), (right.1.top, right.1.bottom));
        }
    }

    /// A 32-bit bitmap in a device context, to read back what is painted into it.
    struct Dib {
        dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        bits: *const u32,
        width: i32,
        height: i32,
    }

    impl Dib {
        unsafe fn new(width: i32, height: i32) -> Self {
            use windows_sys::Win32::Graphics::Gdi::{
                CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
            };
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits = null_mut();
            let bitmap =
                CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null(), "the bitmap is created");
            let dc = CreateCompatibleDC(null_mut());
            let previous = SelectObject(dc, bitmap as HGDIOBJ);
            Self {
                dc,
                bitmap,
                previous,
                bits: bits as *const u32,
                width,
                height,
            }
        }

        /// The pixel at `x`, `y` as the COLORREF it was painted from.
        unsafe fn colour(&self, x: i32, y: i32) -> u32 {
            windows_sys::Win32::Graphics::Gdi::GdiFlush();
            assert!(
                x < self.width && y < self.height,
                "({x}, {y}) is in the bitmap"
            );
            let rgb = *self.bits.add((y * self.width + x) as usize) & 0x00ff_ffff;
            ((rgb & 0xff) << 16) | (rgb & 0xff00) | (rgb >> 16)
        }
    }

    impl Drop for Dib {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc, self.previous);
                DeleteObject(self.bitmap as HGDIOBJ);
                DeleteDC(self.dc);
            }
        }
    }

    #[test]
    fn in_high_contrast_a_card_is_drawn_in_the_system_colours() {
        with_card(jobs, |card| unsafe {
            let contrast = colours(false, true);
            assert_eq!(contrast.surface, GetSysColor(COLOR_WINDOW));
            assert_eq!(contrast.text, GetSysColor(COLOR_WINDOWTEXT));
            assert_eq!(contrast.accent, GetSysColor(COLOR_HIGHLIGHT));
            assert_eq!(contrast.face, GetSysColor(COLOR_BTNFACE));
            assert_eq!(contrast.face_text, GetSysColor(COLOR_BTNTEXT));

            let ui = &mut *card.card();
            let mut snapshot = card.app.snapshot();
            let job = &mut std::sync::Arc::make_mut(&mut snapshot.jobs)[0];
            job.state = JobState::Downloading;
            job.downloaded = 50;
            job.total = Some(100);
            let page = Dib::new(scale(CARD_WIDTH, 96), card_height(ui));
            let painted = paint_card(ui, page.dc, &snapshot, &contrast);
            let bar = scale(HEADER, 96) + 28;
            assert_eq!(
                page.colour(100, bar + 1),
                GetSysColor(COLOR_HIGHLIGHT),
                "the filled part of the bar"
            );
            assert_eq!(
                page.colour(250, bar + 1),
                GetSysColor(COLOR_WINDOW),
                "the track of the bar"
            );
            assert_eq!(
                page.colour(250, bar),
                GetSysColor(COLOR_WINDOWTEXT),
                "the outline of the bar"
            );
            assert_eq!(page.colour(5, 20), GetSysColor(COLOR_WINDOW), "the card");
            assert_eq!(
                page.colour(0, 20),
                GetSysColor(COLOR_WINDOWTEXT),
                "the frame"
            );

            // The buttons are drawn from the colours the card was last painted with.
            sync_controls(ui, painted, contrast);
            let (button, _, rect) = card.buttons().pop().expect("a card has its action");
            let face = Dib::new(rect.right - rect.left, rect.bottom - rect.top);
            SendMessageW(
                button,
                WM_PRINTCLIENT,
                face.dc as usize,
                PRF_CLIENT as isize,
            );
            assert_eq!(
                face.colour(face.width / 2, 3),
                GetSysColor(COLOR_BTNFACE),
                "the face of a button"
            );
            assert_eq!(
                face.colour(face.width / 2, 0),
                GetSysColor(COLOR_WINDOWTEXT),
                "the outline of a button"
            );
        });
    }

    #[test]
    fn a_card_keeps_the_windows_of_the_controls_that_stay_and_the_focus_of_one_that_goes() {
        with_card(jobs, |card| unsafe {
            SetActiveWindow(card.hwnd);
            let before = card.buttons();
            let (header, toggle) = (&before[..3], before[3].0);
            SetFocus(toggle);
            assert_eq!(GetFocus(), toggle, "the focus is on Pause");

            // The download completes: Pause goes, and the footer offers its file.
            let ui = &mut *card.card();
            let mut snapshot = card.app.snapshot();
            std::sync::Arc::make_mut(&mut snapshot.jobs)[0].state = JobState::Completed;
            let colours = colours(false, false);
            let page = Dib::new(scale(CARD_WIDTH, 96), card_height(ui));
            let painted = paint_card(ui, page.dc, &snapshot, &colours);
            sync_controls(ui, painted, colours);

            let after = card.buttons();
            let names: Vec<&str> = after.iter().map(|button| button.1.as_str()).collect();
            assert_eq!(
                names,
                ["Open window", "Pin", "Close", "Open", "Show in folder"]
            );
            for (kept, now) in header.iter().zip(&after) {
                assert_eq!(kept.0, now.0, "{} keeps its window", kept.1);
            }
            assert_eq!(IsWindow(toggle), 0, "the Pause button is gone");
            assert_eq!(
                GetFocus(),
                card.hwnd,
                "the focus moves to the card, not to nothing"
            );
        });
    }
}
