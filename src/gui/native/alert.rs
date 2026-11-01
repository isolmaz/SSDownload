//! Themed replacement for MessageBoxW (the alert window and its layout/paint).

use super::*;

/// A link under the body of an alert: `label` is what the user reads, activating it
/// passes `target` to `open`; when that fails the alert shows `failure`.
pub(super) struct AlertLink {
    pub(super) label: String,
    pub(super) target: String,
    pub(super) open: fn(&str) -> bool,
    pub(super) failure: String,
}
/// One button of a themed alert, carrying the id MessageBoxW would return.
pub(super) struct AlertButton {
    pub(super) id: i32,
    pub(super) label: &'static str,
    pub(super) handle: HWND,
}
/// State of the themed alert that replaces MessageBoxW for the app's own
/// `message()` helper. The window is created with the size its content needs,
/// so nothing is clipped and no band of empty surface is left over.
/// MessageBoxW keeps its body text in a static with this id; scripts, the
/// automation harness and screen readers all reach the text through it.
pub(super) const ALERT_BODY_ID: i32 = 0xFFFF;
const ALERT_LINK_ID: i32 = 0xFFFE;
pub(super) struct AlertUi {
    pub(super) hwnd: HWND,
    pub(super) font: HFONT,
    pub(super) body: String,
    pub(super) icon: HICON,
    pub(super) icon_handle: HWND,
    pub(super) body_handle: HWND,
    pub(super) buttons: Vec<AlertButton>,
    pub(super) default: i32,
    pub(super) escape: i32,
    pub(super) result: i32,
    pub(super) dpi: u32,
    pub(super) margin: i32,
    pub(super) gap: i32,
    pub(super) row: i32,
    pub(super) icon_column: i32,
    pub(super) text_width: i32,
    pub(super) body_height: i32,
    pub(super) dark: bool,
    /// The body is taller than the screen allows: it lives in a read-only
    /// multiline edit with a scroll bar instead of a static.
    pub(super) scroll: bool,
    pub(super) link: Option<AlertLink>,
    pub(super) link_handle: HWND,
    /// Height of the link row; 0 when the alert has no link.
    pub(super) link_height: i32,
}
/// The buttons a flag set asks for, in the order MessageBoxW shows them, plus
/// the default button and the id Escape returns.
pub(super) unsafe fn alert_plan(flags: MESSAGEBOX_STYLE) -> (Vec<(i32, &'static str)>, i32, i32) {
    let buttons: Vec<(i32, &'static str)> = match flags & MB_TYPEMASK {
        MB_OKCANCEL => vec![
            (IDOK, crate::i18n::ui("Tamam", "OK")),
            (IDCANCEL, crate::i18n::ui("İptal", "Cancel")),
        ],
        MB_ABORTRETRYIGNORE => vec![
            (IDABORT, crate::i18n::ui("Durdur", "Abort")),
            (IDRETRY, crate::i18n::ui("Yeniden dene", "Retry")),
            (IDIGNORE, crate::i18n::ui("Yoksay", "Ignore")),
        ],
        MB_YESNOCANCEL => vec![
            (IDYES, crate::i18n::ui("Evet", "Yes")),
            (IDNO, crate::i18n::ui("Hayır", "No")),
            (IDCANCEL, crate::i18n::ui("İptal", "Cancel")),
        ],
        MB_YESNO => vec![
            (IDYES, crate::i18n::ui("Evet", "Yes")),
            (IDNO, crate::i18n::ui("Hayır", "No")),
        ],
        MB_RETRYCANCEL => vec![
            (IDRETRY, crate::i18n::ui("Yeniden dene", "Retry")),
            (IDCANCEL, crate::i18n::ui("İptal", "Cancel")),
        ],
        MB_CANCELTRYCONTINUE => vec![
            (IDCANCEL, crate::i18n::ui("İptal", "Cancel")),
            (IDTRYAGAIN, crate::i18n::ui("Yeniden dene", "Retry")),
            (IDCONTINUE, crate::i18n::ui("Devam et", "Continue")),
        ],
        _ => vec![(IDOK, crate::i18n::ui("Tamam", "OK"))],
    };
    let index = (((flags & MB_DEFMASK) >> 8) as usize).min(buttons.len() - 1);
    let default = buttons[index].0;
    let escape = alert_escape(&buttons, default);
    (buttons, default, escape)
}
/// What Escape and the close button answer: the cancel button when there is one;
/// otherwise "No" of a yes/no question, so a stray Escape never confirms what
/// the question offers; otherwise the default button.
pub(super) fn alert_escape(buttons: &[(i32, &str)], default: i32) -> i32 {
    [IDCANCEL, IDNO]
        .into_iter()
        .find(|wanted| buttons.iter().any(|(id, _)| id == wanted))
        .unwrap_or(default)
}
pub(super) unsafe fn alert_icon(flags: MESSAGEBOX_STYLE) -> HICON {
    let resource = match flags & MB_ICONMASK {
        MB_ICONERROR => IDI_ERROR,
        MB_ICONQUESTION => IDI_QUESTION,
        MB_ICONWARNING => IDI_WARNING,
        MB_ICONINFORMATION => IDI_INFORMATION,
        _ => return null_mut(),
    };
    LoadIconW(null_mut(), resource)
}
/// Height the body needs when wrapped to `width`, measured with the real font.
pub(super) unsafe fn wrapped_height(font: HFONT, body: &str, width: i32) -> i32 {
    let dc = GetDC(null_mut());
    if dc.is_null() {
        return 0;
    }
    let previous = SelectObject(dc, font as HGDIOBJ);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: 0,
    };
    let mut text = wide(body);
    DrawTextW(
        dc,
        text.as_mut_ptr(),
        -1,
        &mut rect,
        DT_CALCRECT | DT_WORDBREAK | DT_EDITCONTROL | DT_NOPREFIX,
    );
    SelectObject(dc, previous);
    ReleaseDC(null_mut(), dc);
    rect.bottom
}
/// The tallest body an alert may have: 70% of the work area, less the space the
/// frame, the margins and the button row take.
pub(super) fn alert_body_cap(work_height: i32, chrome: i32) -> i32 {
    (work_height * 7 / 10 - chrome).max(1)
}
/// Work-area height of the monitor the alert will appear on (the owner's, else
/// the main window's, else the primary one).
pub(super) unsafe fn alert_work_height(owner: HWND) -> i32 {
    let anchor = if !owner.is_null() && IsWindow(owner) != 0 {
        owner
    } else {
        FindWindowW(wide(CLASS_MAIN).as_ptr(), null())
    };
    let monitor = if anchor.is_null() {
        MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
    } else {
        MonitorFromWindow(anchor, MONITOR_DEFAULTTOPRIMARY)
    };
    let mut info: MONITORINFO = zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
        info.rcWork.bottom - info.rcWork.top
    } else {
        768
    }
}
/// The scheme of the window an alert belongs to; a missing owner falls back to
/// the application's main window so the alert still follows the setting.
pub(super) unsafe fn alert_is_dark(owner: HWND) -> bool {
    if !owner.is_null() && IsWindow(owner) != 0 {
        return scheme_dark(owner);
    }
    let main = FindWindowW(wide(CLASS_MAIN).as_ptr(), null());
    !main.is_null() && scheme_dark(main)
}
pub(super) unsafe fn alert_measure_button(ui: &AlertUi, label: &str) -> i32 {
    let mut text = wide(label);
    let dc = GetDC(ui.hwnd);
    if dc.is_null() {
        return scale(80, ui.dpi);
    }
    let font = SendMessageW(ui.hwnd, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    let mut size: SIZE = zeroed();
    GetTextExtentPoint32W(dc, text.as_mut_ptr(), text.len() as i32, &mut size);
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
    ReleaseDC(ui.hwnd, dc);
    (size.cx + scale(28, ui.dpi)).max(scale(80, ui.dpi))
}
pub(super) unsafe fn alert_layout(ui: &AlertUi) {
    let mut client: RECT = zeroed();
    if GetClientRect(ui.hwnd, &mut client) == 0 {
        return;
    }
    let icon_size = scale(32, ui.dpi);
    if !ui.icon_handle.is_null() {
        let top = ui.margin + (ui.body_height - icon_size) / 2;
        MoveWindow(
            ui.icon_handle,
            ui.margin,
            top.max(ui.margin),
            icon_size,
            icon_size,
            1,
        );
    }
    let link_block = if ui.link_height > 0 {
        ui.gap + ui.link_height
    } else {
        0
    };
    let body_height = (client.bottom - ui.margin - ui.row - ui.gap - ui.margin - link_block).max(1);
    if !ui.body_handle.is_null() {
        MoveWindow(
            ui.body_handle,
            ui.margin + ui.icon_column,
            ui.margin,
            ui.text_width,
            body_height,
            1,
        );
    }
    if !ui.link_handle.is_null() {
        MoveWindow(
            ui.link_handle,
            ui.margin + ui.icon_column,
            ui.margin + body_height + ui.gap,
            ui.text_width,
            ui.link_height,
            1,
        );
    }
    // Buttons sit bottom-right, in MessageBoxW's order, one gap apart.
    let mut x = client.right - ui.margin;
    for button in ui.buttons.iter().rev() {
        let width = alert_measure_button(ui, button.label);
        x -= width;
        MoveWindow(
            button.handle,
            x,
            client.bottom - ui.margin - ui.row,
            width,
            ui.row,
            1,
        );
        x -= ui.gap;
    }
}
pub(super) unsafe fn alert_create(ui: &mut AlertUi) {
    // The static holds and draws the body, the way MessageBoxW's does; the
    // window paint only fills the surface.
    ui.body_handle = if ui.scroll {
        // An edit shows a line break only as CR LF.
        let text = ui.body.replace("\r\n", "\n").replace('\n', "\r\n");
        control(
            ui.hwnd,
            "EDIT",
            &text,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOVSCROLL as u32,
            WS_EX_CLIENTEDGE,
            ALERT_BODY_ID,
        )
    } else {
        control(
            ui.hwnd,
            "STATIC",
            &ui.body,
            // SS_LEFT: left aligned and word wrapped; the body carries file and queue names, so
            // an '&' in them is text, not a mnemonic.
            WS_CHILD | WS_VISIBLE | SS_NOPREFIX,
            0,
            ALERT_BODY_ID,
        )
    };
    apply_font(ui.body_handle, ui.font);
    if !ui.icon.is_null() {
        ui.icon_handle = control(
            ui.hwnd,
            "STATIC",
            "",
            WS_CHILD | WS_VISIBLE | 0x03, // SS_ICON; windows-sys does not export it
            0,
            0,
        );
        SendMessageW(ui.icon_handle, STM_SETICON, ui.icon as usize, 0);
    }
    if let Some(link) = &ui.link {
        // Created before the buttons, so Tab reaches it first, in reading order.
        ui.link_handle = control(
            ui.hwnd,
            "SysLink",
            &format!("<a>{}</a>", link.label),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            0,
            ALERT_LINK_ID,
        );
        apply_font(ui.link_handle, ui.font);
        if ui.dark {
            // The link then takes the colour the window answers WM_CTLCOLORSTATIC with:
            // its own blue is too dark for the dark surface.
            let mut item: LITEM = zeroed();
            item.mask = LIF_ITEMINDEX | LIF_STATE;
            item.state = LIS_DEFAULTCOLORS;
            item.stateMask = LIS_DEFAULTCOLORS;
            SendMessageW(ui.link_handle, LM_SETITEM, 0, &item as *const _ as LPARAM);
        }
    }
    for index in 0..ui.buttons.len() {
        let (id, label) = (ui.buttons[index].id, ui.buttons[index].label);
        ui.buttons[index].handle = dialog_button(ui.hwnd, ui.font, label, id, id == ui.default);
    }
    alert_layout(ui);
}
pub(super) unsafe fn paint_alert(ui: &AlertUi, dc: HDC, client: &RECT) {
    FillRect(
        dc,
        client,
        if ui.dark {
            palette_brush(DARK_BACKGROUND)
        } else {
            GetSysColorBrush(COLOR_WINDOW)
        },
    );
    let font = SendMessageW(ui.hwnd, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
}

pub(super) unsafe extern "system" fn alert_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        let raw = create.lpCreateParams as *mut AlertUi;
        (*raw).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
    }
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AlertUi;
    if raw.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let ui = &mut *raw;
    match msg {
        WM_CREATE => {
            alert_create(ui);
            0
        }
        WM_ERASEBKGND | WM_PAINT | WM_PRINTCLIENT => {
            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect);
            if msg == WM_PAINT {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                paint_alert(ui, dc, &rect);
                EndPaint(hwnd, &paint);
            } else {
                paint_alert(ui, wparam as HDC, &rect);
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT => {
            SetTextColor(
                wparam as HDC,
                if !ui.dark {
                    GetSysColor(COLOR_WINDOWTEXT)
                } else if lparam as HWND == ui.link_handle {
                    DARK_LINK
                } else {
                    DARK_TEXT
                },
            );
            SetBkMode(wparam as HDC, TRANSPARENT as i32);
            if ui.dark {
                palette_brush(DARK_BACKGROUND) as LRESULT
            } else {
                GetSysColorBrush(COLOR_WINDOW) as LRESULT
            }
        }
        // The dialog manager asks which button Enter presses, and again to restore the
        // default when focus leaves the buttons. Answering from `ui.default` keeps that
        // answer fixed; the button style moves with focus and would answer in a circle.
        DM_GETDEFID => ((DC_HASDEFID << 16) | ui.default as u32) as LRESULT,
        WM_NOTIFY => {
            let header = &*(lparam as *const NMHDR);
            if header.idFrom == ALERT_LINK_ID as usize
                && (header.code == NM_CLICK || header.code == NM_RETURN)
            {
                if let Some(link) = &ui.link {
                    if (link.open)(&link.target) {
                        // The page opens in front of the offer instead of behind a topmost box.
                        SetWindowPos(
                            hwnd,
                            HWND_NOTOPMOST,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    } else {
                        message(
                            hwnd,
                            link.failure.trim_start(),
                            crate::i18n::ui("SSDownload güncellemesi", "SSDownload update"),
                            MB_OK | MB_ICONWARNING,
                        );
                    }
                }
            }
            0
        }
        WM_COMMAND => {
            let id = loword(wparam) as i32;
            if ui.buttons.iter().any(|button| button.id == id) {
                ui.result = id;
                DestroyWindow(hwnd);
            } else if id == IDCANCEL {
                // IsDialogMessageW reports Escape this way when the alert has
                // no cancel button of its own.
                ui.result = ui.escape;
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            ui.result = ui.escape;
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            // The window is freed by run_alert, which still needs the result.
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
/// Themed, content-sized replacement for MessageBoxW. Returns None when the
/// window cannot be created, so the caller can fall back to the system box.
pub(super) unsafe fn run_alert(
    owner: HWND,
    body: &str,
    title: &str,
    flags: MESSAGEBOX_STYLE,
) -> Option<i32> {
    run_alert_buttons(owner, body, title, flags, None)
}
/// The themed alert with its own button captions: `buttons` lists `(result id, label)`
/// in display order; the button `flags` name with `MB_DEFBUTTON1..4` (the first one without
/// it) is the default and `IDCANCEL`, when listed, answers Escape.
pub(super) unsafe fn run_alert_buttons(
    owner: HWND,
    body: &str,
    title: &str,
    flags: MESSAGEBOX_STYLE,
    buttons: Option<&[(i32, &'static str)]>,
) -> Option<i32> {
    run_alert_linked(owner, body, title, flags, buttons, None)
}
/// `run_alert_buttons` with a link between the body and the buttons.
pub(super) unsafe fn run_alert_linked(
    owner: HWND,
    body: &str,
    title: &str,
    flags: MESSAGEBOX_STYLE,
    buttons: Option<&[(i32, &'static str)]>,
    link: Option<AlertLink>,
) -> Option<i32> {
    let (plan, default, escape) = match buttons {
        Some(custom) if !custom.is_empty() => {
            let default = custom[(((flags & MB_DEFMASK) >> 8) as usize).min(custom.len() - 1)].0;
            let escape = alert_escape(custom, default);
            (custom.to_vec(), default, escape)
        }
        _ => alert_plan(flags),
    };
    let icon = alert_icon(flags);
    let dark = alert_is_dark(owner);
    let dpi = if owner.is_null() {
        GetDpiForSystem()
    } else {
        GetDpiForWindow(owner)
    }
    .max(96);
    let font = make_font(dpi, 9, FW_NORMAL as i32);
    let margin = scale(12, dpi);
    let gap = scale(8, dpi);
    let row = scale(24, dpi);
    let icon_size = scale(32, dpi);
    let icon_column = if icon.is_null() { 0 } else { icon_size + gap };
    let text_width = scale(340, dpi);
    let wrapped =
        wrapped_height(font, body, text_width).max(if icon.is_null() { 0 } else { icon_size });
    let link_height = link
        .as_ref()
        .map_or(0, |link| wrapped_height(font, &link.label, text_width));
    let link_block = if link_height > 0 {
        gap + link_height
    } else {
        0
    };
    // Caption, borders, the link and the button row come out of the space the body may take.
    let cap = alert_body_cap(
        alert_work_height(owner),
        2 * margin + gap + row + scale(48, dpi) + link_block,
    )
    .max(icon_size);
    let scroll = wrapped > cap;
    let body_height = wrapped.min(cap);
    let text_width = if scroll {
        text_width + GetSystemMetrics(SM_CXVSCROLL) + scale(8, dpi)
    } else {
        text_width
    };
    let client_width = text_width + icon_column + 2 * margin;
    let client_height = margin + body_height + link_block + gap + row + margin;
    let style = WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;
    let ex_style = owned_layout(WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT);
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    AdjustWindowRectEx(&mut frame, style, 0, ex_style);
    let buttons: Vec<AlertButton> = plan
        .iter()
        .map(|(id, label)| AlertButton {
            id: *id,
            label,
            handle: null_mut(),
        })
        .collect();
    let raw = Box::into_raw(Box::new(AlertUi {
        hwnd: null_mut(),
        font,
        body: body.to_string(),
        icon,
        icon_handle: null_mut(),
        body_handle: null_mut(),
        buttons,
        default,
        escape,
        result: escape,
        dpi,
        margin,
        gap,
        row,
        icon_column,
        text_width,
        body_height,
        dark,
        scroll,
        link,
        link_handle: null_mut(),
        link_height,
    }));
    let hwnd = CreateWindowExW(
        ex_style,
        wide(CLASS_ALERT).as_ptr(),
        wide(title).as_ptr(),
        style,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        frame.right - frame.left,
        frame.bottom - frame.top,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        raw.cast(),
    );
    if hwnd.is_null() {
        // The box owns no window now, so the font it holds has no other owner to
        // delete it; the normal path deletes it after the loop.
        if !font.is_null() {
            DeleteObject(font as HGDIOBJ);
        }
        drop(Box::from_raw(raw));
        return None;
    }
    if dark {
        SetPropW(hwnd, dark_prop(), 1usize as HANDLE);
        let theme = wide("DarkMode_Explorer");
        SetWindowTheme(hwnd, theme.as_ptr(), null());
        set_dark_caption(hwnd, true);
        EnumChildWindows(hwnd, Some(theme_child), 1);
    }
    center_window(hwnd, owner);
    if flags & MB_TOPMOST != 0 {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    if !owner.is_null() {
        EnableWindow(owner, 0);
    }
    ShowWindow(hwnd, SW_SHOW);
    if flags & MB_SETFOREGROUND != 0 || owner.is_null() {
        SetForegroundWindow(hwnd);
    }
    let mut msg: MSG = zeroed();
    let mut repost_quit = false;
    while IsWindow(hwnd) != 0 {
        let got = GetMessageW(&mut msg, null_mut(), 0, 0);
        if got <= 0 {
            repost_quit = got == 0;
            break;
        }
        if !dialog_message(hwnd, &msg) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if IsWindow(hwnd) != 0 {
        // Invariant: a surviving HWND must be destroyed before its user data is freed
        // (WM_QUIT or pump failure leaves one here; the normal path destroys it itself).
        DestroyWindow(hwnd);
    }
    if !owner.is_null() {
        EnableWindow(owner, 1);
        SetForegroundWindow(owner);
    }
    if repost_quit {
        PostQuitMessage(0);
    }
    let result = (*raw).result;
    let font = (*raw).font;
    if !font.is_null() {
        DeleteObject(font as HGDIOBJ);
    }
    drop(Box::from_raw(raw));
    Some(result)
}
/// The app's own alerts, themed like every other dialog. The call sites keep
/// their MessageBox semantics and their i32 return contract; a box that cannot
/// be created falls back to the system implementation.
pub(super) unsafe fn message(owner: HWND, body: &str, title: &str, flags: MESSAGEBOX_STYLE) -> i32 {
    match run_alert(owner, body, title, flags) {
        Some(value) => value,
        None => MessageBoxW(owner, wide(body).as_ptr(), wide(title).as_ptr(), flags),
    }
}

#[cfg(test)]
mod tests {
    use super::{alert_body_cap, alert_escape};
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDNO, IDOK, IDYES};

    #[test]
    fn body_cap_is_seventy_percent_of_the_work_area_minus_the_chrome() {
        assert_eq!(alert_body_cap(1000, 100), 600);
        // A tiny work area still leaves a usable (positive) height.
        assert_eq!(alert_body_cap(100, 500), 1);
    }

    #[test]
    fn escape_answers_cancel_when_the_alert_has_one() {
        let buttons = [(IDYES, "Yes"), (IDNO, "No"), (IDCANCEL, "Cancel")];
        assert_eq!(alert_escape(&buttons, IDYES), IDCANCEL);
    }

    #[test]
    fn escape_never_confirms_a_yes_no_question() {
        let buttons = [(IDYES, "Yes"), (IDNO, "No")];
        // Yes is the default button, yet a stray Escape or the close button must
        // not answer it.
        assert_eq!(alert_escape(&buttons, IDYES), IDNO);
    }

    #[test]
    fn escape_falls_back_to_the_default_button_otherwise() {
        assert_eq!(alert_escape(&[(IDOK, "OK")], IDOK), IDOK);
        let custom = [(IDOK, "OK"), (100, "Copy"), (101, "Notes")];
        assert_eq!(alert_escape(&custom, IDOK), IDOK);
    }
}
