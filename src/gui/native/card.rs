//! Small bottom-right cards: one download's mini view, the panel of running
//! downloads and the completion card with its open actions. Cards stack above the
//! taskbar, follow the colour scheme (the system colours in high contrast) and can
//! be pinned on top. What a card offers to press is a push button of the card whose
//! face the card draws, so the keyboard and screen readers reach it like any other.
//! The Modern theme draws the same cards as flyouts.

use super::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetActiveWindow, SetCapture,
};

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
    /// Modern's palette; `None` in Classic.
    modern: Option<CardColours>,
}

/// The colours a Modern card paints with.
#[derive(Clone, Copy, PartialEq)]
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
    let (font, bold, small) = card_fonts(dpi);
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

/// The fonts of a card at `dpi`: the body, its bold and, in Modern, the caption.
unsafe fn card_fonts(dpi: u32) -> (HFONT, HFONT, HFONT) {
    if modern() {
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
    }
}

unsafe fn delete_card_fonts(card: &CardUi) {
    for font in [card.font, card.bold, card.small] {
        if !font.is_null() {
            DeleteObject(font as HGDIOBJ);
        }
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

/// The colours of a card as COLORREF (BGR). In high contrast they are the system's: window,
/// its text, highlight and button face.
unsafe fn colours(dark: bool, contrast: bool) -> Colours {
    let modern = modern().then(|| card_colours(dark, scheme_palette(dark), contrast));
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
            modern,
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
        modern,
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

/// The glyph buttons at the end of a card's header, rightmost first: what each shows, does
/// and is called, and whether it marks something on (a held pin).
fn header_glyphs(done: bool, held: bool) -> Vec<(Glyph, CardAction, bool, String)> {
    let mut glyphs = vec![
        (
            Glyph::Close,
            CardAction::Close,
            false,
            crate::i18n::ui("Kapat", "Close").to_string(),
        ),
        (
            if held { Glyph::Pinned } else { Glyph::Pin },
            CardAction::Pin,
            held,
            if held {
                crate::i18n::ui("Sabitlemeyi kaldır", "Unpin")
            } else {
                crate::i18n::ui("Sabitle", "Pin")
            }
            .to_string(),
        ),
    ];
    if !done {
        glyphs.push((
            Glyph::Expand,
            CardAction::Expand,
            false,
            crate::i18n::ui("Genişlet", "Expand").to_string(),
        ));
    }
    glyphs
}

/// The pause or resume button of a download's row: its glyph and its name.
fn toggle_look(job: &Job) -> (Glyph, String) {
    let resume = matches!(
        job.state,
        JobState::Paused | JobState::Failed | JobState::Cancelled
    );
    if resume {
        (
            Glyph::Play,
            crate::i18n::ui_format!("Sürdür: {0}", "Resume: {0}", job.name),
        )
    } else {
        (
            Glyph::Pause,
            crate::i18n::ui_format!("Duraklat: {0}", "Pause: {0}", job.name),
        )
    }
}

/// Modern's colours of a card in the scheme `dark` names, from `palette`. High
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
    body: HFONT,
    strong: HFONT,
    small: HFONT,
}

/// What the face of a Modern card's button shows besides what it is.
#[derive(Clone, Copy, Default)]
struct Face {
    hot: bool,
    pressed: bool,
    focused: bool,
}

/// A Modern card with everything but its buttons: a flyout surface with a thin
/// border, a header with the title, a row for each download (its name, a progress
/// bar, its state). Returns the buttons the card has room for, which draw
/// themselves with `paint_modern_control`.
unsafe fn paint_modern(dc: HDC, client: RECT, card: &ModernCard) -> Vec<Control> {
    let d = card.dpi;
    let s = |value: i32| scale(value, d);
    let colours = &card.colours;
    let palette = colours.palette;
    let surface = palette.flyout;
    fill_colour(dc, &client, surface);
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
    let mut controls: Vec<Control> = Vec::new();
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
    let mut right = client.right - s(8);
    for (glyph, action, marked, name) in header_glyphs(done, card.held) {
        let rect = RECT {
            left: right - target,
            top: s(8),
            right,
            bottom: s(8) + target,
        };
        controls.push(Control {
            rect,
            action,
            name,
            look: Look::Glyph(glyph, marked),
        });
        right -= target;
    }
    // The controls are listed in reading order, which is the order Tab visits them in.
    controls.reverse();
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
            let (glyph, name) = toggle_look(job);
            controls.push(Control {
                rect: RECT {
                    left: client.right - pad + s(8) - target,
                    top: y + s(12) - target / 2,
                    right: client.right - pad + s(8),
                    bottom: y + s(12) - target / 2 + target,
                },
                action: CardAction::Toggle(job.id.clone()),
                name,
                look: Look::Glyph(glyph, true),
            });
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

    // The footer: the actions, the one that opens the file in the accent colour. A
    // button's window reaches its focus ring past the face.
    let ring = s(FOCUS_MARGIN);
    let mut x = pad;
    for (label, action) in card_buttons(card.kind, &jobs) {
        let previous = SelectObject(dc, card.body as HGDIOBJ);
        let text_width = label_width(dc, &label);
        SelectObject(dc, previous);
        let face = RECT {
            left: x,
            top: y,
            right: x + (text_width + 2 * s(16)).max(s(80)),
            bottom: y + target,
        };
        x = face.right + s(8);
        controls.push(Control {
            rect: inflate(face, ring, ring),
            action,
            name: label.clone(),
            look: Look::Label(label),
        });
    }
    controls
}

/// Paints the face of one of a Modern card's buttons into `area` of `dc`: a glyph on the
/// plate the pointer earns, or a footer button, the one that opens the file in the accent
/// colour.
unsafe fn paint_modern_control(
    dc: HDC,
    area: RECT,
    control: &Control,
    face: Face,
    colours: &CardColours,
    font: HFONT,
    dpi: u32,
) {
    let palette = colours.palette;
    match &control.look {
        Look::Glyph(glyph, marked) => {
            fill_colour(dc, &area, palette.flyout);
            let plate = if face.pressed {
                Some(colours.plate.1)
            } else if face.hot {
                Some(colours.plate.0)
            } else {
                None
            };
            if face.focused {
                focus_plate(
                    dc,
                    &area,
                    scale(CORNER, dpi),
                    dpi,
                    &palette,
                    plate.unwrap_or(palette.flyout),
                );
            } else if let Some(plate) = plate {
                round_rect(dc, &area, scale(CORNER, dpi), plate, None);
            }
            let colour = match glyph {
                Glyph::Pause | Glyph::Play => palette.text,
                Glyph::Pin | Glyph::Pinned if *marked => link_colour(&colours.controls),
                _ => palette.secondary,
            };
            let colour = colours.on_plate.filter(|_| face.hot).unwrap_or(colour);
            draw_glyph(dc, *glyph, area, colour, dpi);
        }
        Look::Label(label) => {
            let characters: Vec<u16> = label.encode_utf16().collect();
            paint_button(
                dc,
                area,
                dpi,
                &colours.controls,
                font,
                &characters,
                None,
                ButtonState {
                    primary: matches!(control.action, CardAction::OpenFile(_)),
                    disabled: false,
                    pressed: face.pressed && face.hot,
                    hot: face.hot,
                    focused: face.focused,
                    underline: false,
                },
            );
        }
    }
}

/// Paints everything of a card except its controls, which are buttons of their own, and
/// returns the controls the card has room for.
unsafe fn paint_card(card: &CardUi, dc: HDC, snapshot: &AppSnapshot, colours: &Colours) -> Painted {
    let d = card.dpi;
    let mut client: RECT = zeroed();
    GetClientRect(card.hwnd, &mut client);
    if let Some(modern) = colours.modern {
        let controls = paint_modern(
            dc,
            client,
            &ModernCard {
                kind: &card.kind,
                snapshot,
                dpi: d,
                colours: modern,
                // A completion card is always on top; its pin means "stay until closed".
                held: if matches!(card.kind, CardKind::Done(_)) {
                    card.keep
                } else {
                    card.pinned
                },
                body: card.font,
                strong: card.bold,
                small: card.small,
            },
        );
        return Painted {
            title: card_title(&card.kind, snapshot),
            controls,
        };
    }
    fill(dc, client, colours.surface);
    // One-pixel frame for the light scheme, where the shadow alone is faint.
    let frame = CreateSolidBrush(colours.line);
    FrameRect(dc, &client, frame);
    DeleteObject(frame as HGDIOBJ);
    let mut controls = Vec::new();
    let pad = scale(14, d);
    let jobs = card_jobs(&card.kind, snapshot);
    let title = card_title(&card.kind, snapshot);
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
    let glyphs = header_glyphs(matches!(card.kind, CardKind::Done(_)), held);
    for (symbol, action, marked, name) in glyphs {
        controls.push(Control {
            rect: RECT {
                left: x,
                top: scale(5, d),
                right: x + glyph,
                bottom: scale(5, d) + glyph,
            },
            action,
            name,
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
        y += scale(ROW, d);
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
            let (glyph, name) = toggle_look(job);
            controls.push(Control {
                rect: RECT {
                    left: client.right - pad - scale(26, d),
                    top: y + scale(4, d),
                    right: client.right - pad + scale(2, d),
                    bottom: y + scale(28, d),
                },
                action: CardAction::Toggle(job.id.clone()),
                name,
                look: Look::Glyph(glyph, true),
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
    if let Some(modern) = &colours.modern {
        draw_modern_control(card, draw, control, modern);
        return;
    }
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

/// Draws one control of a Modern card on the face of its push button. The pointer's
/// state is the button's own: it is on it, and it presses it while it is still on it.
unsafe fn draw_modern_control(
    card: &CardUi,
    draw: &NMCUSTOMDRAW,
    control: &Control,
    colours: &CardColours,
) {
    let button = draw.hdr.hwndFrom;
    let hot = !GetPropW(button, hot_prop()).is_null();
    let face = Face {
        hot,
        pressed: hot && !GetPropW(button, pressed_prop()).is_null(),
        focused: SendMessageW(button, BM_GETSTATE, 0, 0) as u32 & BST_FOCUS != 0
            && SendMessageW(button, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0,
    };
    // The glyph buttons paint in several steps; the buffer shows them whole.
    let area = draw.rc;
    let (width, height) = (area.right - area.left, area.bottom - area.top);
    let memory = CreateCompatibleDC(draw.hdc);
    SetLayout(memory, GetLayout(draw.hdc));
    let bitmap = CreateCompatibleBitmap(draw.hdc, width, height);
    let old = SelectObject(memory, bitmap as HGDIOBJ);
    let local = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    paint_modern_control(memory, local, control, face, colours, card.font, card.dpi);
    BitBlt(
        draw.hdc, area.left, area.top, width, height, memory, 0, 0, SRCCOPY,
    );
    SelectObject(memory, old);
    DeleteObject(bitmap as HGDIOBJ);
    DeleteDC(memory);
}

fn same_rect(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// A control's name as the text of its button, which turns the letter after an ampersand
/// into an access key: a download called `AT&T.zip` must be read and typed as it is.
fn button_text(name: &str) -> String {
    name.replace('&', "&&")
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
                    SetWindowTextW(shown.hwnd, wide(&button_text(&control.name)).as_ptr());
                }
                if GetWindow(shown.hwnd, GW_HWNDPREV) != previous
                    || !same_rect(&shown.control.rect, &rect)
                {
                    SetWindowPos(
                        shown.hwnd,
                        previous,
                        rect.left,
                        rect.top,
                        width,
                        height,
                        SWP_NOACTIVATE,
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
                    &button_text(&control.name),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                    0,
                    id,
                );
                subclass(hwnd, card_button_proc);
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

/// Whether the user is working in the card: the cursor is on it, or one of its buttons has
/// the keyboard focus. A card that was only activated is not in use, so it leaves when
/// its time is up like any other.
unsafe fn in_use(hwnd: HWND) -> bool {
    let mut area: RECT = zeroed();
    GetWindowRect(hwnd, &mut area);
    let mut cursor: POINT = zeroed();
    (GetCursorPos(&mut cursor) != 0 && PtInRect(&area, cursor) != 0)
        || GetParent(GetFocus()) == hwnd
}

/// Presses the button as the system does when its click is complete.
unsafe fn click(button: HWND) {
    SendMessageW(
        GetParent(button),
        WM_COMMAND,
        GetDlgCtrlID(button) as usize,
        button as LPARAM,
    );
}

fn pressed_prop() -> *const u16 {
    static NAME: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.CardButtonPressed"));
    NAME.as_ptr()
}

/// A Modern button shows the pointer on it and pressing it, which Classic's does not.
unsafe fn repaint_modern(button: HWND) {
    if modern() {
        InvalidateRect(button, null(), 0);
    }
}

/// A button of a card is pressed by the mouse without taking the focus or activating the
/// card: the user keeps working in the window they were in. The keyboard and screen readers
/// press it through the dialog manager and `BM_CLICK` as for any button.
unsafe extern "system" fn card_button_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE if GetPropW(hwnd, hot_prop()).is_null() => {
            SetPropW(hwnd, hot_prop(), 1usize as HANDLE);
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
            repaint_modern(hwnd);
            call_previous(hwnd, msg, wparam, lparam)
        }
        WM_MOUSELEAVE => {
            RemovePropW(hwnd, hot_prop());
            repaint_modern(hwnd);
            call_previous(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            SetCapture(hwnd);
            SetPropW(hwnd, pressed_prop(), 1usize as HANDLE);
            repaint_modern(hwnd);
            0
        }
        WM_CAPTURECHANGED => {
            RemovePropW(hwnd, pressed_prop());
            repaint_modern(hwnd);
            call_previous(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONUP if GetCapture() == hwnd => {
            ReleaseCapture();
            let mut area: RECT = zeroed();
            GetClientRect(hwnd, &mut area);
            let at = POINT {
                x: (lparam & 0xffff) as i16 as i32,
                y: ((lparam >> 16) & 0xffff) as i16 as i32,
            };
            if PtInRect(&area, at) != 0 {
                click(hwnd);
            }
            0
        }
        BM_CLICK => {
            click(hwnd);
            0
        }
        WM_NCDESTROY => {
            let result = call_previous(hwnd, msg, wparam, lparam);
            RemovePropW(hwnd, subclass_prop());
            RemovePropW(hwnd, hot_prop());
            RemovePropW(hwnd, pressed_prop());
            result
        }
        _ => call_previous(hwnd, msg, wparam, lparam),
    }
}

/// The monitor of the card changed its scale: the fonts and the size are those of the new
/// scale, and the cards are stacked again.
unsafe fn card_dpi_changed(card: &mut CardUi, dpi: u32) {
    delete_card_fonts(card);
    card.dpi = dpi;
    (card.font, card.bold, card.small) = card_fonts(dpi);
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
            let colours = colours(settings_dark(&snapshot.settings), high_contrast());
            let painted = paint_buffered(card, dc, &snapshot, &colours);
            EndPaint(hwnd, &ps);
            sync_controls(card, painted, colours);
            0
        }
        WM_PRINTCLIENT => {
            let snapshot = card.app.snapshot();
            let colours = colours(settings_dark(&snapshot.settings), high_contrast());
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
        // WS_EX_NOACTIVATE keeps a click from activating the card, so a click on the card
        // itself does it: whoever clicks there wants to work in it, and keys reach it only
        // then. The buttons never activate it, and a card never activates itself when it
        // appears.
        WM_LBUTTONDOWN => {
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
    use crate::gui::native::tests::{
        on_private_desktop as on_desktop, register_test_classes, with_common_controls_6, ShiftHeld,
    };
    use crate::model::{Action as ModelAction, AddRequest};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetActiveWindow, SetActiveWindow, VK_ESCAPE, VK_RETURN, VK_SPACE, VK_TAB,
    };

    thread_local! {
        /// The theme the card tests of this thread run in.
        static IN_MODERN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// `on_private_desktop`, in the theme of the test that calls it.
    fn on_private_desktop<T: Send>(body: impl FnOnce() -> T + Send) -> T {
        let in_modern = IN_MODERN.with(std::cell::Cell::get);
        on_desktop(move || {
            set_modern(in_modern);
            body()
        })
    }

    /// Runs `body` in Classic, then in Modern, and fails with what each theme found.
    fn in_each_theme(body: impl Fn(bool)) {
        let mut failed = Vec::new();
        for in_modern in [false, true] {
            IN_MODERN.with(|theme| theme.set(in_modern));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(in_modern)));
            if let Err(panic) = result {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
                    .unwrap_or_default();
                failed.push(format!(
                    "{}: {message}",
                    if in_modern { "modern" } else { "classic" }
                ));
            }
        }
        IN_MODERN.with(|theme| theme.set(false));
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    /// Runs `body` in Modern only.
    fn in_modern(body: impl FnOnce()) {
        IN_MODERN.with(|theme| theme.set(true));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        IN_MODERN.with(|theme| theme.set(false));
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
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
            Self::open_named(kind, dpi, mirrored, None)
        }

        /// The same, for a download saved under `filename`.
        unsafe fn open_named(
            kind: impl FnOnce(&str) -> CardKind,
            dpi: u32,
            mirrored: bool,
            filename: Option<&str>,
        ) -> Self {
            register_test_classes();
            let root =
                std::env::temp_dir().join(format!("ssdownload-card-{}", uuid::Uuid::new_v4()));
            let app = App::open(crate::paths::AppPaths::new(root.clone()).unwrap()).unwrap();
            let added = app
                .dispatch_result(ModelAction::Add {
                    request: AddRequest {
                        url: "https://example.invalid/file.bin".into(),
                        filename: filename.map(str::to_owned),
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
        in_each_theme(|_| {
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
        });
    }

    #[test]
    fn the_controls_of_a_card_are_buttons_named_for_what_they_do_in_reading_order() {
        in_each_theme(|_| {
            with_card(jobs, |card| unsafe {
                let names: Vec<String> =
                    card.buttons().into_iter().map(|button| button.1).collect();
                assert_eq!(
                    names,
                    [
                        "Expand".to_owned(),
                        "Pin".into(),
                        "Close".into(),
                        format!("Pause: {}", card.job_name()),
                        "Open window".into(),
                    ]
                );
            });
            with_card(done, |card| unsafe {
                let names: Vec<String> =
                    card.buttons().into_iter().map(|button| button.1).collect();
                assert_eq!(names, ["Pin", "Close", "Open", "Show in folder"]);
                assert_eq!(
                    window_text(card.hwnd),
                    "Download complete",
                    "the card itself is named for what it shows"
                );
            });
        });
    }

    #[test]
    fn a_card_that_appears_does_not_take_the_focus() {
        in_each_theme(|_| {
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
        });
    }

    /// The mouse key state of a left button message.
    const MK_LBUTTON: usize = 1;

    /// The point `x`, `y` of a window as the position of a mouse message.
    fn at(x: i32, y: i32) -> isize {
        (((y & 0xffff) << 16) | (x & 0xffff)) as isize
    }

    #[test]
    fn a_click_on_the_card_itself_activates_it() {
        in_each_theme(|_| {
            on_private_desktop(|| {
                with_common_controls_6(|| {
                    crate::i18n::with_language("en", || unsafe {
                        let other =
                            control(null_mut(), "STATIC", "other", WS_POPUP | WS_VISIBLE, 0, 0);
                        let card = Fixture::open(jobs, 96, false);
                        SetActiveWindow(other);
                        assert_eq!(GetActiveWindow(), other, "another window is active");
                        SendMessageW(card.hwnd, WM_MOUSEMOVE, 0, at(1, 1));
                        assert_eq!(GetActiveWindow(), other, "no click, no activation");
                        SendMessageW(card.hwnd, WM_LBUTTONDOWN, MK_LBUTTON, at(1, 1));
                        assert_eq!(GetActiveWindow(), card.hwnd, "a click on the card");
                        drop(card);
                        DestroyWindow(other);
                    })
                })
            });
        });
    }

    #[test]
    fn a_click_on_a_button_of_a_card_presses_it_and_leaves_the_active_window_and_the_focus_alone() {
        in_each_theme(|_| {
            on_private_desktop(|| {
                with_common_controls_6(|| {
                    crate::i18n::with_language("en", || unsafe {
                        let other =
                            control(null_mut(), "STATIC", "other", WS_POPUP | WS_VISIBLE, 0, 0);
                        let card = Fixture::open(jobs, 96, false);
                        SetActiveWindow(other);
                        let (active, focus) = (GetActiveWindow(), GetFocus());
                        assert_eq!(active, other, "another window is active");
                        let pin = card.buttons()[1].0;
                        assert!(!(*card.card()).pinned);
                        SendMessageW(pin, WM_LBUTTONDOWN, MK_LBUTTON, at(4, 4));
                        assert_eq!(GetCapture(), pin, "the button holds the mouse");
                        assert!(
                            !(*card.card()).pinned,
                            "nothing is pressed before the button is let go"
                        );
                        SendMessageW(pin, WM_LBUTTONUP, 0, at(4, 4));
                        assert!(GetCapture().is_null(), "the mouse is let go");
                        assert!((*card.card()).pinned, "the click pressed the pin");
                        // A press that is let go outside the button is no click.
                        SendMessageW(pin, WM_LBUTTONDOWN, MK_LBUTTON, at(4, 4));
                        SendMessageW(pin, WM_LBUTTONUP, 0, at(-40, -40));
                        assert!((*card.card()).pinned, "a press let go outside the button");
                        // A double click is a second press.
                        SendMessageW(pin, WM_LBUTTONDBLCLK, MK_LBUTTON, at(4, 4));
                        SendMessageW(pin, WM_LBUTTONUP, 0, at(4, 4));
                        assert!(!(*card.card()).pinned, "a double click");
                        SendMessageW(pin, BM_CLICK, 0, 0);
                        assert!((*card.card()).pinned, "a screen reader presses it too");
                        assert_eq!(GetActiveWindow(), active, "the card was not activated");
                        assert_eq!(GetFocus(), focus, "no button took the focus");
                        drop(card);
                        DestroyWindow(other);
                    })
                })
            });
        });
    }

    #[test]
    fn once_a_card_is_active_tab_enter_space_and_escape_work_in_it() {
        in_each_theme(|_| {
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
        });
    }

    #[test]
    fn a_completion_card_the_user_activated_leaves_unless_a_button_has_the_keyboard() {
        in_each_theme(|_| {
            with_card(done, |card| unsafe {
                let stale = || Instant::now() - DONE_LIFETIME - Duration::from_secs(1);
                SendMessageW(card.hwnd, WM_LBUTTONDOWN, MK_LBUTTON, at(1, 1));
                assert_eq!(GetActiveWindow(), card.hwnd, "the user clicked the card");
                SetFocus(card.buttons()[0].0);
                (*card.card()).opened = stale();
                SendMessageW(card.hwnd, WM_TIMER, TIMER_CARD, 0);
                assert_ne!(
                    IsWindow(card.hwnd),
                    0,
                    "a card with the keyboard in one of its buttons stays"
                );
                SetFocus(card.hwnd);
                assert_eq!(GetActiveWindow(), card.hwnd, "the card is still active");
                (*card.card()).opened = stale();
                SendMessageW(card.hwnd, WM_TIMER, TIMER_CARD, 0);
                assert_eq!(IsWindow(card.hwnd), 0, "an active card nobody uses leaves");
            });
        });
    }

    #[test]
    fn an_ampersand_in_the_name_of_a_download_is_no_access_key_of_its_button() {
        in_each_theme(|_| {
            on_private_desktop(|| {
                with_common_controls_6(|| {
                    crate::i18n::with_language("en", || unsafe {
                        let main = control(null_mut(), "STATIC", "main", WS_POPUP, 0, 0);
                        let card = Fixture::open_named(jobs, 96, false, Some("AT&T-report.zip"));
                        // The engine numbers a name that an earlier run of this test claimed.
                        let name = card.job_name();
                        assert!(name.starts_with("AT&T-report"), "{name}");
                        let buttons = card.buttons();
                        assert_eq!(
                            buttons[3].1,
                            format!("Pause: {}", name.replace('&', "&&")),
                            "the button shows the ampersand"
                        );
                        let state = || card.app.snapshot().jobs[0].state;
                        let before = state();
                        // A letter that no control takes makes Windows beep. This
                        // check box takes it, after the buttons of the card.
                        let taker = control(
                            card.hwnd,
                            "BUTTON",
                            "&t",
                            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                            0,
                            9999,
                        );
                        SendMessageW(card.hwnd, WM_LBUTTONDOWN, MK_LBUTTON, at(1, 1));
                        SetFocus(buttons[0].0);
                        let mut msg: MSG = zeroed();
                        msg.hwnd = GetFocus();
                        msg.message = WM_CHAR;
                        msg.wParam = 't' as usize;
                        dialog_message(main, &msg);
                        assert_eq!(
                            SendMessageW(taker, BM_GETCHECK, 0, 0),
                            BST_CHECKED as isize,
                            "the letter went to the control whose access key it is"
                        );
                        assert_eq!(
                            state(),
                            before,
                            "the letter after the ampersand pauses nothing"
                        );
                        drop(card);
                        DestroyWindow(main);
                    })
                })
            });
        });
    }

    #[test]
    fn the_buttons_of_a_card_follow_the_order_of_the_list_when_it_changes() {
        in_each_theme(|_| {
            with_card(jobs, |card| unsafe {
                let place = RECT {
                    left: 10,
                    top: 40,
                    right: 60,
                    bottom: 60,
                };
                let sync = |names: [&str; 3]| {
                    let controls = names
                        .iter()
                        .map(|name| Control {
                            rect: place,
                            action: CardAction::Toggle((*name).to_owned()),
                            name: (*name).to_owned(),
                            look: Look::Label((*name).to_owned()),
                        })
                        .collect();
                    let painted = Painted {
                        title: "title".to_owned(),
                        controls,
                    };
                    sync_controls(&mut *card.card(), painted, colours(false, false));
                };
                let order = || {
                    card.buttons()
                        .into_iter()
                        .map(|button| button.1)
                        .collect::<Vec<_>>()
                };
                sync(["a", "b", "c"]);
                assert_eq!(order(), ["a", "b", "c"]);
                let windows: Vec<HWND> =
                    card.buttons().into_iter().map(|button| button.0).collect();
                sync(["c", "a", "b"]);
                assert_eq!(
                    order(),
                    ["c", "a", "b"],
                    "Tab visits them in the order of the list"
                );
                let now: Vec<HWND> = card.buttons().into_iter().map(|button| button.0).collect();
                assert_eq!(
                    now,
                    [windows[2], windows[0], windows[1]],
                    "the windows are kept"
                );
            });
        });
    }

    #[test]
    fn the_footer_of_an_empty_panel_stands_below_its_text() {
        in_each_theme(|in_modern| {
            with_card(
                |_| CardKind::Panel,
                |card| unsafe {
                    // The only download is scheduled for later, so the panel lists none.
                    let buttons = card.buttons();
                    let pause_all = buttons
                        .iter()
                        .find(|button| button.1 == "Pause all")
                        .expect("the panel offers Pause all");
                    // A Modern button's window reaches past its face for the focus ring.
                    let (text_end, ring) = if in_modern {
                        (
                            scale(MODERN_HEADER + MODERN_ROW, 96),
                            scale(FOCUS_MARGIN, 96),
                        )
                    } else {
                        (scale(HEADER + ROW, 96), 0)
                    };
                    assert!(
                        pause_all.2.top + ring >= text_end,
                        "the button at {} px is below the text row that ends at {text_end} px",
                        pause_all.2.top + ring,
                    );
                    let mut frame: RECT = zeroed();
                    GetWindowRect(card.hwnd, &mut frame);
                    assert!(
                        pause_all.2.bottom <= frame.bottom - frame.top,
                        "the button is inside the card"
                    );
                },
            );
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
        in_each_theme(|_| {
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
        });
    }

    #[test]
    fn a_mirrored_card_mirrors_its_controls() {
        in_each_theme(|in_modern| {
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
            // A Modern button is as wide as its label, so the card it is compared to
            // reads the same language.
            let plain = with_card_in(if in_modern { "ar" } else { "en" }, jobs, 96, false, places);
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
        });
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
        in_each_theme(|_| {
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
                let page = Dib::new(scale(card_width(modern()), 96), card_height(ui));
                let painted = paint_card(ui, page.dc, &snapshot, &colours);
                sync_controls(ui, painted, colours);

                let after = card.buttons();
                let names: Vec<&str> = after.iter().map(|button| button.1.as_str()).collect();
                assert_eq!(names, ["Expand", "Pin", "Close", "Open", "Show in folder"]);
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
        });
    }

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

    /// An app on a temporary data directory, and that directory.
    fn temp_app() -> (App, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("ssdownload-card-{}", uuid::Uuid::new_v4()));
        let paths = crate::paths::AppPaths::new(root.clone()).unwrap();
        (App::open(paths).unwrap(), root)
    }

    /// A snapshot that lists `jobs`.
    fn snapshot_of(jobs: Vec<Job>) -> AppSnapshot {
        static BASE: std::sync::OnceLock<AppSnapshot> = std::sync::OnceLock::new();
        let base = BASE.get_or_init(|| {
            let (app, root) = temp_app();
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

    /// A Modern card painted on a DIB: its pixels, size, buttons and colours.
    struct Shot {
        pixels: Vec<u32>,
        width: i32,
        height: i32,
        controls: Vec<Control>,
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

        /// The middle of the button `index`.
        fn middle(&self, index: usize) -> POINT {
            let rect = self.controls[index].rect;
            POINT {
                x: (rect.left + rect.right) / 2,
                y: (rect.top + rect.bottom) / 2,
            }
        }

        /// The place of the button `index` that is called `name`: the `nth` of that name.
        fn index_of(&self, name: &str, nth: usize) -> usize {
            self.controls
                .iter()
                .enumerate()
                .filter(|(_, control)| action_name(&control.action) == name)
                .nth(nth)
                .unwrap_or_else(|| panic!("a {name} button, number {nth}"))
                .0
        }

        /// The face of the button `index`: its window reaches the focus ring past it, which
        /// only a button with a label has room for.
        fn face(&self, index: usize) -> RECT {
            let control = &self.controls[index];
            match control.look {
                Look::Label(_) => {
                    inflate(control.rect, -self.s(FOCUS_MARGIN), -self.s(FOCUS_MARGIN))
                }
                Look::Glyph(..) => control.rect,
            }
        }
    }

    /// Where the pointer is on a card and where it pressed.
    type Pointer = (Option<POINT>, Option<POINT>);

    /// `kind` over `jobs` painted in `colours`, its buttons in the state the pointer gives
    /// them as the buttons themselves read it: hot while it is on them, pressed while the
    /// press began on them and the pointer is still there.
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
            let mut controls = Vec::new();
            let pixels = on_white_rect(width, height, |dc| {
                controls = paint_modern(
                    dc,
                    client,
                    &ModernCard {
                        kind,
                        snapshot: &snapshot,
                        dpi,
                        colours,
                        held: false,
                        body: fonts[0],
                        strong: fonts[1],
                        small: fonts[2],
                    },
                );
                for control in &controls {
                    let over =
                        |at: Option<POINT>| at.is_some_and(|at| PtInRect(&control.rect, at) != 0);
                    let hot = over(pointer);
                    let face = Face {
                        hot,
                        pressed: hot && over(press),
                        focused: false,
                    };
                    paint_modern_control(dc, control.rect, control, face, &colours, fonts[0], dpi);
                }
            });
            for font in fonts {
                DeleteObject(font as HGDIOBJ);
            }
            Shot {
                pixels,
                width,
                height,
                controls,
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

    /// A card of each kind with the downloads it lists and the buttons it paints, in the
    /// order Tab visits them.
    fn card_samples() -> Vec<(CardKind, Vec<Job>, Vec<&'static str>)> {
        let long = "an-extremely-long-file-name-for-a-download-that-has-to-be-shortened.tar.gz";
        vec![
            (
                CardKind::Panel,
                vec![
                    running(long),
                    job("paused.iso", JobState::Paused, 3, Some(10)),
                ],
                vec!["expand", "pin", "close", "toggle", "toggle", "pause all"],
            ),
            (
                CardKind::Panel,
                Vec::new(),
                vec!["expand", "pin", "close", "pause all"],
            ),
            (
                CardKind::Done("done.bin".into()),
                vec![job("done.bin", JobState::Completed, 10, Some(10))],
                vec!["pin", "close", "open file", "open folder"],
            ),
            (
                CardKind::Jobs(vec!["a.bin".into(), "b.bin".into()]),
                vec![running("a.bin"), job("b.bin", JobState::Paused, 1, Some(9))],
                vec!["expand", "pin", "close", "toggle", "toggle", "expand"],
            ),
            (
                CardKind::Jobs(vec!["c.bin".into()]),
                vec![job("c.bin", JobState::Completed, 10, Some(10))],
                vec!["expand", "pin", "close", "open file", "open folder"],
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
    fn a_modern_card_fits_its_buttons_in_every_language_scheme_and_scale() {
        for info in crate::i18n::LANGUAGES {
            crate::i18n::with_language(info.code, || {
                for dark in [false, true] {
                    for dpi in [96, 144] {
                        for (kind, jobs, expected) in card_samples() {
                            let shot = shoot(&kind, jobs, dpi, dark, (None, None));
                            let context =
                                format!("{} dark={dark} dpi={dpi} {expected:?}", info.code);
                            let names: Vec<_> = shot
                                .controls
                                .iter()
                                .map(|control| action_name(&control.action))
                                .collect();
                            assert_eq!(names, expected, "{context}");
                            let (target, pad) = (shot.s(MODERN_TARGET), shot.s(MODERN_PAD));
                            for (index, control) in shot.controls.iter().enumerate() {
                                let rect = control.rect;
                                let edges = (rect.left, rect.top, rect.right, rect.bottom);
                                let context =
                                    format!("{context} {} {edges:?}", action_name(&control.action));
                                assert!(
                                    rect.left >= 0
                                        && rect.top >= 0
                                        && rect.right <= shot.width
                                        && rect.bottom <= shot.height,
                                    "{context}: leaves the card"
                                );
                                let face = shot.face(index);
                                assert_eq!(face.bottom - face.top, target, "{context}");
                                match control.look {
                                    Look::Glyph(..) => {
                                        assert_eq!(face.right - face.left, target, "{context}")
                                    }
                                    Look::Label(_) => {
                                        assert!(face.left >= pad, "{context}");
                                        assert!(face.right <= shot.width - pad, "{context}");
                                        assert!(
                                            face.bottom <= shot.height - shot.s(8),
                                            "{context}"
                                        );
                                        assert!(face.right - face.left >= shot.s(80), "{context}");
                                    }
                                }
                                for other in shot.controls.iter().skip(index + 1) {
                                    let (a, b) = (rect, other.rect);
                                    assert!(
                                        a.right <= b.left
                                            || b.right <= a.left
                                            || a.bottom <= b.top
                                            || b.bottom <= a.top,
                                        "{context}: stands over {}",
                                        action_name(&other.action)
                                    );
                                }
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

    /// The pixel a few epx in from a button's left edge: inside its plate, clear of its glyph.
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
    fn a_pointer_on_a_glyph_button_paints_its_plate_and_a_press_a_stronger_one() {
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
                for index in [rest.index_of("close", 0), rest.index_of("toggle", 0)] {
                    let rect = rest.controls[index].rect;
                    let (x, y) = beside_glyph(&rest, &rect);
                    let middle = Some(rest.middle(index));
                    let hot = shoot_at((middle, None));
                    let down = shoot_at((middle, middle));
                    let context = format!("{context} button {index}");
                    assert_eq!(rest.at(x, y), surface, "{context}");
                    assert_eq!(hot.at(x, y), hover, "{context}");
                    assert_eq!(down.at(x, y), pressed, "{context}");
                    assert!(differs_only_in(&rest, &hot, rect), "{context}");
                    assert!(differs_only_in(&rest, &down, rect), "{context}");
                    // A press shows only while the pointer is still on the pressed button.
                    let off = Some(rest.middle(rest.index_of("pin", 0)));
                    assert!(
                        differs_only_in(
                            &rest,
                            &shoot_at((off, middle)),
                            rest.controls[rest.index_of("pin", 0)].rect
                        ),
                        "{context}: a press the pointer left"
                    );
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
                let controls = rest.colours.controls;
                // Open (the primary action) and Show in folder.
                let (open, folder) = (
                    rest.index_of("open file", 0),
                    rest.index_of("open folder", 0),
                );
                let context = format!("dark={dark} dpi={dpi}");
                let (x, y) = beside_glyph(&rest, &rest.face(open));
                assert_eq!(rest.at(x, y), rest.colours.palette.accent, "{context}");
                let on_open = Some(rest.middle(open));
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
                let (x, y) = beside_glyph(&rest, &rest.face(folder));
                assert_eq!(rest.at(x, y), controls.control, "{context}");
                let on_folder = Some(rest.middle(folder));
                let hot = shoot_at((on_folder, None));
                let down = shoot_at((on_folder, on_folder));
                assert_eq!(hot.at(x, y), controls.control_hover, "{context}");
                assert_eq!(down.at(x, y), controls.control_pressed, "{context}");
                let area = rest.controls[folder].rect;
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
                let close = rest.index_of("close", 0);
                let on_close = Some(rest.middle(close));
                for pointer in [(on_close, None), (on_close, on_close)] {
                    let shot = shoot_at(pointer);
                    let rect = shot.controls[close].rect;
                    let (x, y) = beside_glyph(&shot, &rect);
                    assert_eq!(shot.at(x, y), theme.highlight, "{context}: plate");
                    let middle = shot.middle(close);
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

    /// The face of the push button `button`, painted the way it paints itself on the screen.
    unsafe fn face_of(button: HWND, rect: &RECT) -> Dib {
        let face = Dib::new(rect.right - rect.left, rect.bottom - rect.top);
        SendMessageW(
            button,
            WM_PRINTCLIENT,
            face.dc as usize,
            PRF_CLIENT as isize,
        );
        face
    }

    #[test]
    fn a_button_of_a_modern_card_shows_the_pointer_on_it_and_a_press_that_began_on_it() {
        in_modern(|| {
            with_card(jobs, |card| unsafe {
                let modern = (*card.card())
                    .colours
                    .and_then(|colours| colours.modern)
                    .expect("a Modern card is painted in Modern's colours");
                let (pin, _, rect) = card.buttons().remove(1);
                let beside = || {
                    let face = face_of(pin, &rect);
                    face.colour(scale(3, 96), face.height / 2)
                };
                // A change of look has the button repaint itself.
                let repaints = |message: u32, wparam: usize, lparam: isize| {
                    UpdateWindow(pin);
                    SendMessageW(pin, message, wparam, lparam);
                    windows_sys::Win32::Graphics::Gdi::GetUpdateRect(pin, null_mut(), 0) != 0
                };
                assert_eq!(beside(), modern.palette.flyout, "at rest");
                assert!(
                    repaints(WM_MOUSEMOVE, 0, at(10, 10)),
                    "the pointer came onto it"
                );
                assert_eq!(beside(), modern.plate.0, "the pointer is on it");
                assert!(
                    repaints(WM_LBUTTONDOWN, MK_LBUTTON, at(10, 10)),
                    "the press began"
                );
                assert_eq!(beside(), modern.plate.1, "it is pressed");
                assert!(repaints(WM_MOUSELEAVE, 0, 0), "the pointer left it");
                assert_eq!(
                    beside(),
                    modern.palette.flyout,
                    "a press shows only while the pointer is on the button"
                );
                SendMessageW(pin, WM_MOUSEMOVE, 0, at(10, 10));
                assert_eq!(
                    beside(),
                    modern.plate.1,
                    "the pointer came back to the press"
                );
                SendMessageW(pin, WM_LBUTTONUP, 0, at(-40, -40));
                assert!(!(*card.card()).pinned, "a press let go outside is no click");
                assert_eq!(
                    beside(),
                    modern.plate.0,
                    "the press is over, the pointer is still on it"
                );
                SendMessageW(pin, WM_MOUSELEAVE, 0, 0);
                assert_eq!(beside(), modern.palette.flyout, "the pointer left");
            });
        });
    }

    #[test]
    fn a_button_of_a_modern_card_shows_the_keyboard_focus_only_while_the_cues_are_shown() {
        in_modern(|| {
            with_card(jobs, |card| unsafe {
                let modern = (*card.card())
                    .colours
                    .and_then(|colours| colours.modern)
                    .expect("a Modern card is painted in Modern's colours");
                let (pin, _, rect) = card.buttons().remove(1);
                let top = || {
                    let face = face_of(pin, &rect);
                    face.colour(face.width / 2, 1)
                };
                let cues = |shown: bool| {
                    let action = if shown { UIS_CLEAR } else { UIS_SET };
                    SendMessageW(
                        card.hwnd,
                        WM_UPDATEUISTATE,
                        (action | (UISF_HIDEFOCUS << 16)) as usize,
                        0,
                    );
                };
                SetFocus(pin);
                cues(false);
                assert_eq!(top(), modern.palette.flyout, "the cues are hidden");
                cues(true);
                assert_eq!(top(), modern.palette.focus_outer, "the focus ring");
                SetFocus(null_mut());
                assert_eq!(top(), modern.palette.flyout, "no focus, no ring");
            });
        });
    }
}
