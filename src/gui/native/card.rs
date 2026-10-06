//! Small bottom-right cards: one download's mini view, the panel of running
//! downloads and the completion card with its open actions. Cards stack above the
//! taskbar, follow the colour scheme and can be pinned on top.

use super::*;

pub(super) const CLASS_CARD: &str = "SSDownload.Card";
const TIMER_CARD: usize = 7;
/// A completion card leaves by itself after this long unless the cursor is on it.
const DONE_LIFETIME: Duration = Duration::from_secs(12);
const CARD_WIDTH: i32 = 340;
const HEADER: i32 = 34;
const ROW: i32 = 54;
const FOOTER: i32 = 40;
const PANEL_ROWS: usize = 5;

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
    pinned: bool,
    /// A completion card the user pinned stays until it is closed.
    keep: bool,
    opened: Instant,
    hover: bool,
    hits: Vec<(RECT, CardAction)>,
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

/// Opens a card; a panel or a job card for the same downloads is raised instead of
/// duplicated.
pub(super) unsafe fn open_card(owner: HWND, app: App, kind: CardKind) {
    let existing = CARDS.with(|cards| cards.borrow().clone());
    for hwnd in existing {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardUi;
        if raw.is_null() {
            continue;
        }
        let same = match (&(*raw).kind, &kind) {
            (CardKind::Panel, CardKind::Panel) => true,
            (CardKind::Jobs(a), CardKind::Jobs(b)) => a == b,
            (CardKind::Done(a), CardKind::Done(b)) => a == b,
            _ => false,
        };
        if same {
            (*raw).opened = Instant::now();
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            InvalidateRect(hwnd, null(), 1);
            return;
        }
    }
    let dpi = GetDpiForWindow(owner).max(96);
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
        hover: false,
        hits: Vec::new(),
    }));
    let height = card_height(&*raw);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | if pinned { WS_EX_TOPMOST } else { 0 },
        wide(CLASS_CARD).as_ptr(),
        wide("SSDownload").as_ptr(),
        WS_POPUP,
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
    stack_cards();
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, TIMER_CARD, 500, None);
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
            .filter(|job| {
                matches!(
                    job.state,
                    JobState::Connecting
                        | JobState::Downloading
                        | JobState::Processing
                        | JobState::Queued
                        | JobState::Paused
                )
            })
            .take(PANEL_ROWS)
            .collect(),
    }
}

unsafe fn card_height(card: &CardUi) -> i32 {
    let snapshot = card.app.snapshot();
    let rows = card_jobs(&card.kind, &snapshot).len().max(1) as i32;
    scale(HEADER + ROW * rows + FOOTER, card.dpi)
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

unsafe fn paint_card(card: &mut CardUi, dc: HDC) {
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
    let title = match &card.kind {
        CardKind::Done(_) => crate::i18n::ui("İndirme tamamlandı", "Download complete").to_string(),
        CardKind::Panel => crate::i18n::ui_owned!(
            format!("İndirmeler ({})", jobs.len()),
            format!("Downloads ({})", jobs.len())
        ),
        CardKind::Jobs(_) => "SSDownload".to_string(),
    };
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
        list.push((
            if card.pinned {
                Glyph::Pinned
            } else {
                Glyph::Pin
            },
            CardAction::Pin,
            if card.pinned { accent } else { muted },
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
        let fraction = match (job.state, job.total) {
            (JobState::Completed, _) => 1.0,
            (_, Some(total)) if total > 0 => (job.downloaded as f64 / total as f64).min(1.0),
            _ => 0.0,
        };
        let filled = RECT {
            right: bar.left + ((bar.right - bar.left) as f64 * fraction) as i32,
            ..bar
        };
        fill(dc, filled, state_colour(job, accent));
        let detail = match job.state {
            JobState::Completed => crate::i18n::ui_owned!(
                format!("Tamamlandı · {}", format_bytes(job.downloaded)),
                format!("Completed · {}", format_bytes(job.downloaded))
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

unsafe fn close_card(hwnd: HWND) {
    KillTimer(hwnd, TIMER_CARD);
    DestroyWindow(hwnd);
}

unsafe fn run_action(card: &mut CardUi, action: CardAction) {
    match action {
        CardAction::Close => close_card(card.hwnd),
        CardAction::Pin => {
            card.pinned = !card.pinned;
            card.keep = card.pinned;
            stack_cards();
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
            0
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            card.hover = false;
            card.opened = Instant::now();
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
            let point = POINT {
                x: (lparam & 0xffff) as i16 as i32,
                y: ((lparam >> 16) & 0xffff) as i16 as i32,
            };
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
            DeleteObject(card.font as HGDIOBJ);
            DeleteObject(card.bold as HGDIOBJ);
            drop(Box::from_raw(raw));
            stack_cards();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
