//! Dark/light theming: painters for frames, lists, headers, menu strip, tabs, cards and progress bars.

use super::*;

/// The window property `apply_dark_mode` stamps on the window that owns a
/// surface; painters read it instead of the settings snapshot, which arrives
/// after the first frame.
pub(super) fn dark_prop() -> *const u16 {
    static NAME: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.Dark"));
    NAME.as_ptr()
}
/// True when the applied scheme is dark for this window's root. Painting must
/// never depend on the snapshot: the first frame is painted before it lands.
pub(super) unsafe fn scheme_dark(hwnd: HWND) -> bool {
    let root = GetAncestor(hwnd, GA_ROOT);
    let root = if root.is_null() { hwnd } else { root };
    !GetPropW(root, dark_prop()).is_null()
}
/// Cached brush for one palette colour; the palette is closed, so the match is
/// the lookup and no brush is ever created twice.
pub(super) unsafe fn palette_brush(colour: u32) -> HBRUSH {
    static BACKGROUND: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_BACKGROUND) as usize });
    static SURFACE: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_SURFACE) as usize });
    static BORDER: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_BORDER) as usize });
    static SELECTION: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_SELECTION) as usize });
    /// The multi-selection fill, dimmed: only the focused row keeps the accent.
    static DIM_SELECTION: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_SELECTION_DIM) as usize });
    static MUTED: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_MUTED) as usize });
    static TEXT: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_TEXT) as usize });
    static ACCENT: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { CreateSolidBrush(DARK_ACCENT) as usize });
    match colour {
        DARK_SURFACE => *SURFACE as HBRUSH,
        DARK_BORDER => *BORDER as HBRUSH,
        DARK_SELECTION => *SELECTION as HBRUSH,
        DARK_SELECTION_DIM => *DIM_SELECTION as HBRUSH,
        DARK_BACKGROUND => *BACKGROUND as HBRUSH,
        DARK_MUTED => *MUTED as HBRUSH,
        DARK_TEXT => *TEXT as HBRUSH,
        DARK_ACCENT => *ACCENT as HBRUSH,
        _ => *BACKGROUND as HBRUSH,
    }
}
/// The dark surface brush shared by the window paint hook and the tab strip.
pub(super) unsafe fn dark_surface_brush() -> HBRUSH {
    palette_brush(DARK_BACKGROUND)
}
pub(super) unsafe fn dark_text_brush() -> HBRUSH {
    palette_brush(DARK_TEXT)
}
/// Outlines one control's non-client frame in the scheme's border colour.
pub(super) unsafe fn paint_frame(hwnd: HWND) {
    let mut window: RECT = zeroed();
    let mut client: RECT = zeroed();
    if GetWindowRect(hwnd, &mut window) == 0 || GetClientRect(hwnd, &mut client) == 0 {
        return;
    }
    let mut origin = POINT {
        x: client.left,
        y: client.top,
    };
    ClientToScreen(hwnd, &mut origin);
    let left = origin.x - window.left;
    let top = origin.y - window.top;
    let right = client.right - client.left + left;
    let bottom = client.bottom - client.top + top;
    let width = window.right - window.left;
    let height = window.bottom - window.top;
    if left <= 0 && top <= 0 && right >= width && bottom >= height {
        return;
    }
    let dc = GetWindowDC(hwnd);
    if dc.is_null() {
        return;
    }
    let brush = palette_brush(DARK_BORDER);
    for band in [
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: top,
        },
        RECT {
            left: 0,
            top: bottom,
            right: width,
            bottom: height,
        },
        RECT {
            left: 0,
            top,
            right: left,
            bottom,
        },
        RECT {
            left: right,
            top,
            right: width,
            bottom,
        },
    ] {
        if band.right > band.left && band.bottom > band.top {
            FillRect(dc, &band, brush);
        }
    }
    ReleaseDC(hwnd, dc);
}
/// Brushes and text colours the owner-drawn tab strip paints with. The strip
/// reuses the dialog surface so the page never shows a second background.
pub(super) struct TabPalette {
    pub(super) background: HBRUSH,
    pub(super) text: HBRUSH,
    pub(super) text_color: u32,
    pub(super) muted_color: u32,
}
pub(super) unsafe fn tab_palette(dark: bool) -> TabPalette {
    if dark {
        TabPalette {
            background: dark_surface_brush(),
            text: dark_text_brush(),
            text_color: 0xeeeeee,
            muted_color: 0x9a9a9a,
        }
    } else {
        TabPalette {
            background: GetSysColorBrush(COLOR_WINDOW),
            text: GetSysColorBrush(COLOR_WINDOWTEXT),
            text_color: GetSysColor(COLOR_WINDOWTEXT),
            muted_color: GetSysColor(COLOR_GRAYTEXT),
        }
    }
}
/// True when the dialog paints its surface dark.
pub(super) unsafe fn dialog_is_dark(dlg: &DialogUi) -> bool {
    dlg.app.snapshot().settings.dark_mode && !high_contrast()
}
/// Live brushes and colours for the wizard cards, indexed dark * 2 + selected.
pub(super) struct CardFace {
    pub(super) fill: usize,
    pub(super) pen: usize,
    pub(super) glyph: u32,
    pub(super) title: u32,
    pub(super) description: u32,
}
pub(super) static CARD_FACES: std::sync::LazyLock<[CardFace; 4]> = std::sync::LazyLock::new(|| {
    let make = |fill: u32, border: u32, glyph: u32, title: u32, description: u32| unsafe {
        CardFace {
            fill: CreateSolidBrush(fill) as usize,
            pen: CreatePen(PS_SOLID, 1, border) as usize,
            glyph,
            title,
            description,
        }
    };
    [
        // COLORREFs are BGR: the accent reads as RGB(15,108,189) in the light
        // cards and RGB(76,194,255) in the dark ones.
        make(0xffffff, 0xd6d6d6, 0xbd6c0f, 0x1b1b1b, 0x5a5a5a),
        make(0xf7f2f2, 0xbd6c0f, 0xbd6c0f, 0x101010, 0x444444),
        make(0x2b2b2b, 0x3f3f3f, 0xffc24c, 0xf0f0f0, 0xa8a8a8),
        make(0x332a1f, 0xffc24c, 0xffc24c, 0xffffff, 0xc8c8c8),
    ]
});
/// Zero-terminated property names the cards keep their state under.
pub(super) fn card_prop(name: &str) -> *const u16 {
    static CARD_DARK: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.Dark"));
    static PREVIOUS: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.CardProc"));
    static FONT_DPI: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.CardFontDpi"));
    static TITLE: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.CardTitleFont"));
    static DESCRIPTION: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.CardTextFont"));
    match name {
        "dark" => CARD_DARK.as_ptr(),
        "previous" => PREVIOUS.as_ptr(),
        "title-font" => TITLE.as_ptr(),
        "font-dpi" => FONT_DPI.as_ptr(),
        _ => DESCRIPTION.as_ptr(),
    }
}
/// Creates a font from the first face the system really provides.
pub(super) unsafe fn make_face_font(dpi: u32, points: i32, weight: i32, faces: &[&str]) -> HFONT {
    for face in faces {
        let font = CreateFontW(
            -((points * dpi as i32) / 72),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide(face).as_ptr(),
        );
        if font.is_null() {
            continue;
        }
        let dc = GetDC(null_mut());
        let previous = SelectObject(dc, font as HGDIOBJ);
        let mut name = [0u16; 64];
        let len = GetTextFaceW(dc, name.len() as i32, name.as_mut_ptr()).max(0) as usize;
        SelectObject(dc, previous);
        ReleaseDC(null_mut(), dc);
        if String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case(face) {
            return font;
        }
        DeleteObject(font as HGDIOBJ);
    }
    make_font(dpi, points, weight)
}
/// A card font, created once per control and DPI and owned by that control.
pub(super) unsafe fn card_font(
    hwnd: HWND,
    prop: &str,
    points: i32,
    weight: i32,
    faces: &[&str],
) -> HFONT {
    let dpi = GetDpiForWindow(hwnd).max(96);
    let stored = GetPropW(hwnd, card_prop(prop)) as isize;
    if stored != 0 {
        if GetPropW(hwnd, card_prop("font-dpi")) as usize == dpi as usize {
            return stored as HFONT;
        }
        // The window moved to another monitor: the cached font is stale.
        DeleteObject(stored as HGDIOBJ);
        RemovePropW(hwnd, card_prop(prop));
    }
    let font = make_face_font(dpi, points, weight, faces);
    SetPropW(hwnd, card_prop(prop), font as HANDLE);
    SetPropW(hwnd, card_prop("font-dpi"), dpi as usize as HANDLE);
    font
}
/// Vector icon for a card: a play triangle, a folder or a music note. Icon
/// fonts are not used because a font that is missing or substituted renders the
/// private-use codepoint as a tofu box, which no size check can tell apart from
/// a real glyph.
pub(super) unsafe fn draw_card_glyph(dc: HDC, id: i32, origin: POINT, size: i32, colour: u32) {
    let brush = CreateSolidBrush(colour);
    if brush.is_null() {
        return;
    }
    let previous = SelectObject(dc, brush as HGDIOBJ);
    let unit = (size / 16).max(1);
    let (x, y) = (origin.x, origin.y);
    match id {
        ID_WIZARD_FILE => {
            // Folder: the tab first, then the body.
            Rectangle(dc, x, y + 2 * unit, x + 6 * unit, y + 5 * unit);
            Rectangle(dc, x, y + 4 * unit, x + 13 * unit, y + 12 * unit);
        }
        ID_WIZARD_AUDIO => {
            // Note: head, stem and flag.
            Ellipse(dc, x + 2 * unit, y + 8 * unit, x + 8 * unit, y + 13 * unit);
            Rectangle(dc, x + 7 * unit, y + 2 * unit, x + 9 * unit, y + 11 * unit);
            Rectangle(dc, x + 9 * unit, y + 2 * unit, x + 13 * unit, y + 4 * unit);
        }
        _ => {
            let mut triangle = [
                POINT {
                    x: x + 3 * unit,
                    y: y + 2 * unit,
                },
                POINT {
                    x: x + 13 * unit,
                    y: y + 7 * unit,
                },
                POINT {
                    x: x + 3 * unit,
                    y: y + 12 * unit,
                },
            ];
            Polygon(dc, triangle.as_mut_ptr(), triangle.len() as i32);
        }
    }
    SelectObject(dc, previous);
    DeleteObject(brush as HGDIOBJ);
}
/// Splits a card's own caption into its title and its explanation, so the
/// window text automation reads stays the full sentence.
pub(super) fn card_text_parts(text: &str) -> (String, String) {
    match text.split_once(" — ") {
        Some((title, description)) => (title.to_string(), description.to_string()),
        None => (text.to_string(), String::new()),
    }
}
pub(super) unsafe fn draw_card(hwnd: HWND, dc: HDC) {
    let mut rect: RECT = zeroed();
    GetClientRect(hwnd, &mut rect);
    let dpi = GetDpiForWindow(hwnd).max(96);
    let dark = !GetPropW(GetAncestor(hwnd, GA_ROOT), card_prop("dark")).is_null();
    let selected = SendMessageW(hwnd, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED;
    let face = &CARD_FACES[(usize::from(dark) * 2 + usize::from(selected)).min(3)];
    let padded = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    let fill = SelectObject(dc, face.fill as HGDIOBJ);
    let pen = SelectObject(dc, face.pen as HGDIOBJ);
    RoundRect(
        dc,
        padded.left,
        padded.top,
        padded.right,
        padded.bottom,
        scale(10, dpi),
        scale(10, dpi),
    );
    SelectObject(dc, pen);
    SelectObject(dc, fill);
    SetBkMode(dc, TRANSPARENT as i32);
    // Vector glyph on top, centred.
    let glyph_size = scale(32, dpi);
    draw_card_glyph(
        dc,
        GetDlgCtrlID(hwnd),
        POINT {
            x: (padded.left + padded.right - glyph_size) / 2,
            y: padded.top + scale(12, dpi),
        },
        glyph_size,
        face.glyph,
    );
    // Title, then the explanation under it.
    let mut buffer = [0u16; 512];
    let len = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32).max(0) as usize;
    let (title, description) = card_text_parts(&String::from_utf16_lossy(&buffer[..len]));
    let mut title_wide = wide(&title);
    let mut title_area = RECT {
        left: padded.left + scale(8, dpi),
        top: padded.top + scale(52, dpi),
        right: padded.right - scale(8, dpi),
        bottom: padded.bottom,
    };
    let title_font = SelectObject(
        dc,
        card_font(
            hwnd,
            "title-font",
            11,
            600,
            &["Segoe UI Semibold", "Segoe UI"],
        ) as HGDIOBJ,
    );
    SetTextColor(dc, face.title);
    DrawTextW(
        dc,
        title_wide.as_mut_ptr(),
        -1,
        &mut title_area,
        DT_CENTER | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    SelectObject(dc, title_font);
    let mut description_wide = wide(&description);
    let mut description_area = RECT {
        left: padded.left + scale(10, dpi),
        top: padded.top + scale(76, dpi),
        right: padded.right - scale(10, dpi),
        bottom: padded.bottom - scale(8, dpi),
    };
    let description_font = SelectObject(
        dc,
        card_font(hwnd, "text-font", 8, FW_NORMAL as i32, &["Segoe UI"]) as HGDIOBJ,
    );
    SetTextColor(dc, face.description);
    DrawTextW(
        dc,
        description_wide.as_mut_ptr(),
        -1,
        &mut description_area,
        DT_CENTER | DT_TOP | DT_WORDBREAK | DT_EDITCONTROL,
    );
    SelectObject(dc, description_font);
    if GetFocus() == hwnd {
        let mut focus = padded;
        focus.left += scale(3, dpi);
        focus.top += scale(3, dpi);
        focus.right -= scale(3, dpi);
        focus.bottom -= scale(3, dpi);
        DrawFocusRect(dc, &focus);
    }
}
/// Cards are real check boxes, so BM_GETCHECK, BM_CLICK and Space keep working;
/// only their painting is taken over.
pub(super) unsafe extern "system" fn card_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            draw_card(hwnd, dc);
            EndPaint(hwnd, &ps);
            return 0;
        }
        WM_PRINTCLIENT => {
            draw_card(hwnd, wparam as HDC);
            return 0;
        }
        WM_ERASEBKGND => return 1,
        WM_NCDESTROY => {
            for prop in ["title-font", "text-font"] {
                let font = RemovePropW(hwnd, card_prop(prop)) as isize;
                if font != 0 {
                    DeleteObject(font as HGDIOBJ);
                }
            }
            RemovePropW(hwnd, card_prop("previous"));
        }
        _ => {}
    }
    let previous = GetPropW(hwnd, card_prop("previous")) as isize;
    if previous == 0 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
        std::mem::transmute(previous);
    CallWindowProcW(Some(proc), hwnd, msg, wparam, lparam)
}
/// Turns a caption row into a selectable icon card while keeping the check box
/// messages automation depends on.
pub(super) unsafe fn dialog_card(parent: HWND, font: HFONT, label: &str, id: i32) -> HWND {
    let h = control(
        parent,
        "BUTTON",
        label,
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | BS_AUTOCHECKBOX as u32
            | BS_PUSHLIKE as u32
            | BS_MULTILINE as u32,
        0,
        id,
    );
    apply_font(h, font);
    let previous = SetWindowLongPtrW(h, GWLP_WNDPROC, card_proc as *const () as isize);
    SetPropW(h, card_prop("previous"), previous as HANDLE);
    h
}
pub(super) unsafe fn tab_title(tabs: HWND, index: i32) -> [u16; 64] {
    let mut buffer = [0u16; 64];
    let mut item: TCITEMW = zeroed();
    item.mask = TCIF_TEXT;
    item.pszText = buffer.as_mut_ptr();
    item.cchTextMax = buffer.len() as i32;
    SendMessageW(
        tabs,
        TCM_GETITEMW,
        index as usize,
        &mut item as *mut _ as isize,
    );
    buffer
}
pub(super) unsafe fn measure_tab_item(dlg: &DialogUi, item: *mut MEASUREITEMSTRUCT) -> bool {
    if (*item).CtlID != S_TABS as u32 {
        return false;
    }
    (*item).itemWidth = scale(120, dlg.dpi) as u32;
    (*item).itemHeight = scale(30, dlg.dpi) as u32;
    true
}
/// The previous proc of a subclassed control, kept in a window property so the
/// chain survives everything the control does afterwards.
pub(super) fn subclass_prop() -> *const u16 {
    static NAME: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.PreviousProc"));
    NAME.as_ptr()
}
/// Hooks one control's window proc. Returns false when the control is gone.
/// Idempotent: re-applying the scheme must not stack a second hook.
pub(super) unsafe fn subclass(
    hwnd: HWND,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> bool {
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return false;
    }
    if !GetPropW(hwnd, subclass_prop()).is_null() {
        return true;
    }
    let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc as *const () as isize);
    if previous == 0 {
        return false;
    }
    SetPropW(hwnd, subclass_prop(), previous as HANDLE);
    true
}
/// Draws a control's client-edge frame in the scheme's border colour. The theme
/// keeps those frames light in dark mode and there is no message to recolour
/// them, so the non-client area is painted here instead.
pub(super) unsafe extern "system" fn frame_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCPAINT && scheme_dark(hwnd) {
        paint_frame(hwnd);
        return 0;
    }
    call_previous(hwnd, msg, wparam, lparam)
}
pub(super) unsafe fn call_previous(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let previous = GetPropW(hwnd, subclass_prop()) as isize;
    if previous == 0 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
        std::mem::transmute(previous);
    CallWindowProcW(Some(proc), hwnd, msg, wparam, lparam)
}
/// Paints the header cells: comctl32 keeps the themed header light even with
/// the dark subtheme applied, and it answers the list's custom draw itself, so
/// the whole client area is painted here.
pub(super) unsafe fn paint_header(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client) == 0 {
        return;
    }
    let dpi = GetDpiForWindow(hwnd).max(96);
    FillRect(dc, &client, palette_brush(DARK_BACKGROUND));
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, DARK_TEXT);
    for index in 0..SendMessageW(hwnd, HDM_GETITEMCOUNT, 0, 0) as i32 {
        let mut rect: RECT = zeroed();
        if SendMessageW(
            hwnd,
            HDM_GETITEMRECT,
            index as usize,
            &mut rect as *mut _ as isize,
        ) == 0
        {
            continue;
        }
        let mut title = [0u16; 128];
        let mut item: HDITEMW = zeroed();
        item.mask = HDI_TEXT;
        item.pszText = title.as_mut_ptr();
        item.cchTextMax = title.len() as i32;
        SendMessageW(
            hwnd,
            HDM_GETITEMW,
            index as usize,
            &mut item as *mut _ as isize,
        );
        let mut text = rect;
        text.left += scale(8, dpi);
        text.right -= scale(4, dpi);
        DrawTextW(
            dc,
            title.as_mut_ptr(),
            -1,
            &mut text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        let divider = RECT {
            left: rect.right - 1,
            top: rect.top + scale(3, dpi),
            right: rect.right,
            bottom: rect.bottom - scale(3, dpi),
        };
        FillRect(dc, &divider, palette_brush(DARK_BORDER));
    }
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
    // The header's bottom edge is what separates it from the rows.
    let edge = RECT {
        left: client.left,
        top: client.bottom - 1,
        right: client.right,
        bottom: client.bottom,
    };
    FillRect(dc, &edge, palette_brush(DARK_BORDER));
}
pub(super) unsafe extern "system" fn header_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if scheme_dark(hwnd) {
        match msg {
            WM_PAINT => {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                paint_header(hwnd, dc);
                EndPaint(hwnd, &paint);
                return 0;
            }
            WM_PRINTCLIENT => {
                paint_header(hwnd, wparam as HDC);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            _ => {}
        }
    }
    call_previous(hwnd, msg, wparam, lparam)
}
/// One dotted hint around the focused row. DrawFocusRect inverts to white on a
/// dark surface, which the palette forbids, so the dots are drawn by hand.
pub(super) unsafe fn draw_dotted(dc: HDC, rect: &RECT, colour: u32) {
    let brush = palette_brush(colour);
    let mut x = rect.left;
    while x < rect.right {
        let top = RECT {
            left: x,
            top: rect.top,
            right: x + 1,
            bottom: rect.top + 1,
        };
        let bottom = RECT {
            left: x,
            top: rect.bottom - 1,
            right: x + 1,
            bottom: rect.bottom,
        };
        FillRect(dc, &top, brush);
        FillRect(dc, &bottom, brush);
        x += 2;
    }
    let mut y = rect.top;
    while y < rect.bottom {
        let top = RECT {
            left: rect.left,
            top: y,
            right: rect.left + 1,
            bottom: y + 1,
        };
        let bottom = RECT {
            left: rect.right - 1,
            top: y,
            right: rect.right,
            bottom: y + 1,
        };
        FillRect(dc, &top, brush);
        FillRect(dc, &bottom, brush);
        y += 2;
    }
}
/// One cell of one dark row: background, selection, text, our own grid lines
/// and the focus hint. The list keeps its themed cells light even under the
/// dark subtheme, so the list answers its own custom draw here.
pub(super) unsafe fn paint_list_cell(ui: &MainUi, draw: &NMLVCUSTOMDRAW) {
    let list = draw.nmcd.hdr.hwndFrom;
    let dc = draw.nmcd.hdc;
    let rect = draw.nmcd.rc;
    let selected = draw.nmcd.uItemState & CDIS_SELECTED != 0;
    let focused = draw.nmcd.uItemState & CDIS_FOCUS != 0;
    FillRect(
        dc,
        &rect,
        palette_brush(if selected && focused {
            DARK_SELECTION
        } else if selected {
            DARK_SELECTION_DIM
        } else {
            DARK_BACKGROUND
        }),
    );
    let mut text = [0u16; 512];
    let mut item: LVITEMW = zeroed();
    item.iSubItem = draw.iSubItem;
    item.pszText = text.as_mut_ptr();
    item.cchTextMax = text.len() as i32;
    SendMessageW(
        list,
        LVM_GETITEMTEXTW,
        draw.nmcd.dwItemSpec,
        &mut item as *mut _ as isize,
    );
    let length = text
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(text.len()) as i32;
    let font = SendMessageW(list, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(
        dc,
        if selected {
            DARK_TEXT
        } else if draw.iSubItem == 5 {
            // The location column is reference text, not a reading column.
            DARK_MUTED
        } else {
            DARK_TEXT
        },
    );
    let mut area = rect;
    area.left += scale(6, ui.dpi);
    area.right -= scale(3, ui.dpi);
    DrawTextW(
        dc,
        text.as_mut_ptr(),
        length,
        &mut area,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    if draw.iSubItem == 2 {
        paint_row_progress(ui, dc, draw.nmcd.dwItemSpec, rect, 0x3c3a3a);
    }
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
    // Our own grid lines: the themed ones stay light whatever the subtheme.
    let vertical = RECT {
        left: rect.right - 1,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    };
    let horizontal = RECT {
        left: rect.left,
        top: rect.bottom - 1,
        right: rect.right,
        bottom: rect.bottom,
    };
    let line = palette_brush(DARK_BORDER);
    FillRect(dc, &vertical, line);
    FillRect(dc, &horizontal, line);
    if selected && focused {
        let hint = RECT {
            left: rect.left + 1,
            top: rect.top + 1,
            right: rect.right - 1,
            bottom: rect.bottom - 1,
        };
        draw_dotted(dc, &hint, DARK_MUTED);
    }
}
/// The list's dark body. Cells are painted per subitem, and the empty area
/// below the last row is painted here too: dark mode drops LVS_EX_GRIDLINES,
/// so the default pass draws no lines there to leave alone.
/// Colour of a row's progress bar: completed green, failed red, paused grey.
fn progress_colour(job: &Job) -> u32 {
    match job.state {
        JobState::Completed => 0x59c734,
        JobState::Failed | JobState::Cancelled => 0x303bff,
        JobState::Paused | JobState::AwaitingSource | JobState::Scheduled | JobState::Queued => {
            0x938e8e
        }
        _ => 0xe37100,
    }
}

/// A thin progress bar along the bottom of the progress cell of `row`.
pub(super) unsafe fn paint_row_progress(ui: &MainUi, dc: HDC, row: usize, cell: RECT, track: u32) {
    let Some(job) = ui
        .visible_ids
        .get(row)
        .and_then(|id| ui.snapshot.jobs.iter().find(|job| &job.id == id))
    else {
        return;
    };
    let fraction = match (job.state, job.total) {
        (JobState::Completed, _) => 1.0,
        (_, Some(total)) if total > 0 => (job.downloaded as f64 / total as f64).min(1.0),
        _ => return,
    };
    let height = scale(3, ui.dpi);
    let inset = scale(6, ui.dpi);
    let bar = RECT {
        left: cell.left + inset,
        top: cell.bottom - height - scale(2, ui.dpi),
        right: cell.right - inset,
        bottom: cell.bottom - scale(2, ui.dpi),
    };
    if bar.right <= bar.left {
        return;
    }
    fill_solid(dc, &bar, track);
    let filled = RECT {
        right: bar.left + ((bar.right - bar.left) as f64 * fraction) as i32,
        ..bar
    };
    fill_solid(dc, &filled, progress_colour(job));
}

/// Fills `rect` with any colour. `palette_brush` only knows the dark palette and
/// falls back to the background for everything else.
unsafe fn fill_solid(dc: HDC, rect: &RECT, colour: u32) {
    let brush = CreateSolidBrush(colour);
    if brush.is_null() {
        return;
    }
    FillRect(dc, rect, brush);
    DeleteObject(brush as HGDIOBJ);
}

/// Light scheme: the default row painting, plus the progress bar drawn after it.
pub(super) unsafe fn light_list_custom_draw(ui: &MainUi, lparam: LPARAM) -> LRESULT {
    let draw = &*(lparam as *const NMLVCUSTOMDRAW);
    let stage = draw.nmcd.dwDrawStage;
    if stage == CDDS_PREPAINT {
        return CDRF_NOTIFYITEMDRAW as LRESULT;
    }
    if stage == CDDS_ITEMPREPAINT {
        return CDRF_NOTIFYSUBITEMDRAW as LRESULT;
    }
    if stage == CDDS_ITEMPREPAINT | CDDS_SUBITEM {
        return if draw.iSubItem == 2 {
            CDRF_NOTIFYPOSTPAINT as LRESULT
        } else {
            CDRF_DODEFAULT as LRESULT
        };
    }
    if stage == CDDS_ITEMPOSTPAINT | CDDS_SUBITEM && draw.iSubItem == 2 {
        paint_row_progress(
            ui,
            draw.nmcd.hdc,
            draw.nmcd.dwItemSpec,
            draw.nmcd.rc,
            0xeae5e5,
        );
    }
    CDRF_DODEFAULT as LRESULT
}

pub(super) unsafe fn list_custom_draw(ui: &MainUi, lparam: LPARAM) -> LRESULT {
    let draw = &*(lparam as *const NMLVCUSTOMDRAW);
    let stage = draw.nmcd.dwDrawStage;
    if stage == CDDS_PREPAINT {
        return CDRF_NOTIFYITEMDRAW as LRESULT;
    }
    if stage == CDDS_ITEMPREPAINT {
        return CDRF_NOTIFYSUBITEMDRAW as LRESULT;
    }
    if stage == CDDS_ITEMPREPAINT | CDDS_SUBITEM {
        paint_list_cell(ui, draw);
        return CDRF_SKIPDEFAULT as LRESULT;
    }
    if stage == CDDS_POSTPAINT {
        paint_list_tail(ui, draw.nmcd.hdc);
        return CDRF_DODEFAULT as LRESULT;
    }
    CDRF_DODEFAULT as LRESULT
}
/// Repaints the empty area under the last row: background plus the same grid
/// lines and column separators the filled rows carry. The first line sits one
/// row height below the area's top, so on an empty list - where the area starts
/// at the header - it is the bottom of the first row slot: the header and
/// first-data-row boundary itself is the header's own bottom edge, drawn in
/// `paint_header`.
pub(super) unsafe fn paint_list_tail(ui: &MainUi, dc: HDC) {
    let list = ui.list;
    let mut client: RECT = zeroed();
    if GetClientRect(list, &mut client) == 0 {
        return;
    }
    let count = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as i32;
    let mut row_height = scale(22, ui.dpi);
    if count > 0 {
        let mut first = RECT {
            left: LVIR_BOUNDS as i32,
            ..zeroed()
        };
        if SendMessageW(list, LVM_GETITEMRECT, 0, &mut first as *mut _ as isize) != 0
            && first.bottom > first.top
        {
            row_height = first.bottom - first.top;
        }
    }
    let mut tail_top = client.top;
    if count > 0 {
        let mut last = RECT {
            left: LVIR_BOUNDS as i32,
            ..zeroed()
        };
        if SendMessageW(
            list,
            LVM_GETITEMRECT,
            (count - 1) as usize,
            &mut last as *mut _ as isize,
        ) != 0
        {
            tail_top = last.bottom;
        }
    } else {
        let header = SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND;
        let mut header_rect: RECT = zeroed();
        if !header.is_null() && GetClientRect(header, &mut header_rect) != 0 {
            tail_top = header_rect.bottom;
        }
    }
    if tail_top >= client.bottom {
        return;
    }
    let tail = RECT {
        left: client.left,
        top: tail_top,
        right: client.right,
        bottom: client.bottom,
    };
    FillRect(dc, &tail, palette_brush(DARK_BACKGROUND));
    let line = palette_brush(DARK_BORDER);
    let mut y = tail_top + row_height - 1;
    while y < client.bottom {
        let row_line = RECT {
            left: client.left,
            top: y,
            right: client.right,
            bottom: y + 1,
        };
        FillRect(dc, &row_line, line);
        y += row_height;
    }
    let header = SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND;
    let columns = if header.is_null() {
        0
    } else {
        SendMessageW(header, HDM_GETITEMCOUNT, 0, 0) as i32
    };
    let mut edge = client.left;
    for index in 0..columns {
        edge += SendMessageW(list, LVM_GETCOLUMNWIDTH, index as usize, 0) as i32;
        if edge >= client.right {
            break;
        }
        let column_line = RECT {
            left: edge - 1,
            top: tail_top,
            right: edge,
            bottom: client.bottom,
        };
        FillRect(dc, &column_line, line);
    }
}
/// The bar's own geometry: the strip box in window coordinates, the window
/// origin that turns item rects into the same space, and the strip's bottom
/// edge, which is also the underline's clamp.
pub(super) struct MenuStrip {
    pub(super) window_left: i32,
    pub(super) window_top: i32,
    pub(super) bar: RECT,
    pub(super) bottom: i32,
}

/// Measures the menu bar. The first item's rect supplies the strip's top edge
/// and the client area its bottom, so the strip follows whatever height the
/// shell gave the bar. `None` while the bar cannot be measured.
pub(super) unsafe fn menu_strip(ui: &MainUi) -> Option<MenuStrip> {
    let menu = GetMenu(ui.hwnd);
    if menu.is_null() || GetMenuItemCount(menu) <= 0 {
        return None;
    }
    let mut first: RECT = zeroed();
    if GetMenuItemRect(ui.hwnd, menu, 0, &mut first) == 0 {
        return None;
    }
    let mut window: RECT = zeroed();
    let mut client: RECT = zeroed();
    if GetWindowRect(ui.hwnd, &mut window) == 0 || GetClientRect(ui.hwnd, &mut client) == 0 {
        return None;
    }
    let mut origin = POINT {
        x: client.left,
        y: client.top,
    };
    ClientToScreen(ui.hwnd, &mut origin);
    let top = first.top - window.top;
    let bottom = origin.y - window.top;
    if bottom <= top {
        return None;
    }
    Some(MenuStrip {
        window_left: window.left,
        window_top: window.top,
        bar: RECT {
            left: 0,
            top,
            right: window.right - window.left,
            bottom,
        },
        bottom,
    })
}

/// Fills the bar itself in the scheme's colours, with the edge that separates
/// it from the client area. Shared by the strip repaint and the theme's own bar
/// drawing, so both land on the same pixels.
pub(super) unsafe fn fill_menu_bar(dc: HDC, strip: &MenuStrip) {
    FillRect(dc, &strip.bar, palette_brush(DARK_BACKGROUND));
    let edge = RECT {
        left: strip.bar.left,
        top: strip.bar.bottom - 1,
        right: strip.bar.right,
        bottom: strip.bar.bottom,
    };
    FillRect(dc, &edge, palette_brush(DARK_BORDER));
}

/// The font the bar's captions are drawn with: the application font, which is
/// also the font the menu carries, so the captions and the item boxes the shell
/// measured agree.
pub(super) unsafe fn menu_font(ui: &MainUi) -> HFONT {
    if ui.font.is_null() {
        SendMessageW(ui.hwnd, WM_GETFONT, 0, 0) as HFONT
    } else {
        ui.font
    }
}

/// Paints the menu bar strip in the scheme's colours, with the hot item
/// highlighted, into the window's own context. The system draws the strip light
/// and repaints a hot item on its own during menu tracking, so this runs on
/// every frame paint and is invoked again whenever the selection changes.
pub(super) unsafe fn paint_menu_strip(ui: &MainUi) {
    let dc = GetWindowDC(ui.hwnd);
    if dc.is_null() {
        return;
    }
    paint_menu_strip_into(ui, dc);
    ReleaseDC(ui.hwnd, dc);
}

/// Paints the strip into a caller-supplied context, in the same window
/// coordinates `menu_strip` measures. The theme's own bar requests hand over the
/// context they present; painting those anywhere else leaves the presented one
/// light, which is what a fast hover across the bar shows.
pub(super) unsafe fn paint_menu_strip_into(ui: &MainUi, dc: HDC) {
    if dc.is_null() {
        return;
    }
    let menu = GetMenu(ui.hwnd);
    if menu.is_null() {
        return;
    }
    let count = GetMenuItemCount(menu);
    if count <= 0 {
        return;
    }
    let Some(strip) = menu_strip(ui) else {
        return;
    };
    fill_menu_bar(dc, &strip);
    for index in 0..count {
        let mut rect: RECT = zeroed();
        if GetMenuItemRect(ui.hwnd, menu, index as u32, &mut rect) == 0 {
            continue;
        }
        let item = RECT {
            left: rect.left - strip.window_left,
            top: rect.top - strip.window_top,
            right: rect.right - strip.window_left,
            bottom: rect.bottom - strip.window_top,
        };
        draw_menu_item(
            dc,
            ui,
            menu,
            index as usize,
            item,
            ui.menu_hot == index,
            strip.bottom,
        );
    }
}

/// Draws one bar item's box: the highlight a hot item gets and the caption with
/// its keyboard hint. The item rect is the bar's own box for that caption,
/// padding included, so the caption is drawn inside all of it and centred like
/// the system does: insetting it clipped the last letter of every title.
pub(super) unsafe fn draw_menu_item(
    dc: HDC,
    ui: &MainUi,
    menu: HMENU,
    index: usize,
    item: RECT,
    hot: bool,
    strip_bottom: i32,
) {
    let mut label = [0u16; 128];
    let length = GetMenuStringW(
        menu,
        index as u32,
        label.as_mut_ptr(),
        label.len() as i32,
        MF_BYPOSITION,
    );
    if length <= 0 {
        return;
    }
    if hot {
        FillRect(dc, &item, palette_brush(DARK_SURFACE));
    }
    SetBkMode(dc, TRANSPARENT as i32);
    let font = menu_font(ui);
    let previous = if font.is_null() {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    let mut title = String::from_utf16_lossy(&label[..length as usize]);
    let mnemonic = match title.find('&') {
        Some(at) => {
            title.remove(at);
            Some(at)
        }
        None => None,
    };
    let state = GetMenuState(menu, index as u32, MF_BYPOSITION);
    let enabled = state & (MF_DISABLED | MF_GRAYED) == 0;
    SetTextColor(dc, if enabled { DARK_TEXT } else { DARK_MUTED });
    let mut text = item;
    let mut wide_title = wide(&title);
    let mut title_size: SIZE = zeroed();
    GetTextExtentPoint32W(
        dc,
        wide_title.as_mut_ptr(),
        title.chars().count() as i32,
        &mut title_size,
    );
    let text_left = item.left + ((item.right - item.left) - title_size.cx).max(0) / 2;
    DrawTextW(
        dc,
        wide_title.as_mut_ptr(),
        -1,
        &mut text,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    // The keyboard hint: one hairline at the glyph baseline under exactly the
    // mnemonic letter, the way Windows draws it, and only while the bar is in
    // keyboard mode.
    if ui.menu_hint {
        if let Some(at) = mnemonic {
            if at < title.chars().count() {
                let prefix: String = title.chars().take(at).collect();
                let letter: String = title.chars().skip(at).take(1).collect();
                let mut measure = wide(&prefix);
                let mut size: SIZE = zeroed();
                GetTextExtentPoint32W(
                    dc,
                    measure.as_mut_ptr(),
                    prefix.chars().count() as i32,
                    &mut size,
                );
                let mut letter_wide = wide(&letter);
                let mut letter_size: SIZE = zeroed();
                GetTextExtentPoint32W(
                    dc,
                    letter_wide.as_mut_ptr(),
                    letter.chars().count() as i32,
                    &mut letter_size,
                );
                let mut metrics: TEXTMETRICW = zeroed();
                GetTextMetricsW(dc, &mut metrics);
                let text_top = text.top + (text.bottom - text.top - metrics.tmHeight).max(0) / 2;
                let thickness = scale(1, ui.dpi).max(1);
                let top = (text_top + metrics.tmAscent).min(strip_bottom - 1 - thickness);
                let underline = RECT {
                    left: text_left + size.cx,
                    top,
                    right: text_left + size.cx + letter_size.cx.max(scale(4, ui.dpi)),
                    bottom: top + thickness,
                };
                FillRect(dc, &underline, palette_brush(DARK_TEXT));
            }
        }
    }
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
}

/// The theme's bar-drawing request: the menu and the device context to draw in.
/// The flag word after it is not read. The per-item request puts a
/// `DRAWITEMSTRUCT` (which carries the same context) at its head.
#[repr(C)]
pub(super) struct UahMenu {
    pub(super) menu: HMENU,
    pub(super) dc: HDC,
    pub(super) _flags: u32,
}

/// Bar item under a screen point, -1 for the gaps between items and for a
/// disabled caption. The shell skips its own highlight on those, so the strip
/// does too.
pub(super) unsafe fn menu_item_at(hwnd: HWND, menu: HMENU, point: POINT) -> i32 {
    for index in 0..GetMenuItemCount(menu) {
        let mut rect: RECT = zeroed();
        if GetMenuItemRect(hwnd, menu, index as u32, &mut rect) == 0 {
            break;
        }
        if point.x < rect.left
            || point.x >= rect.right
            || point.y < rect.top
            || point.y >= rect.bottom
        {
            continue;
        }
        let state = GetMenuState(menu, index as u32, MF_BYPOSITION);
        return if state & (MF_DISABLED | MF_GRAYED) == 0 {
            index
        } else {
            -1
        };
    }
    -1
}

/// Asks for the non-client leave notification that clears the hover highlight
/// once the cursor leaves the bar. The request is one-shot, so every move over
/// the bar renews it.
pub(super) unsafe fn track_nc_leave(hwnd: HWND) {
    let mut track = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE | TME_NONCLIENT,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    TrackMouseEvent(&mut track);
}

/// The status bar keeps the system face colour, offers no text-colour message
/// and drops the dark subtheme for the text, so dark mode paints its parts.
pub(super) unsafe fn paint_status(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client) == 0 {
        return;
    }
    let dpi = GetDpiForWindow(hwnd).max(96);
    FillRect(dc, &client, palette_brush(DARK_SURFACE));
    let edge = RECT {
        left: client.left,
        top: client.top,
        right: client.right,
        bottom: client.top + 1,
    };
    FillRect(dc, &edge, palette_brush(DARK_BORDER));
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, DARK_TEXT);
    let parts = SendMessageW(hwnd, SB_GETPARTS, 0, 0).max(1);
    for index in 0..parts {
        let mut rect: RECT = zeroed();
        if SendMessageW(
            hwnd,
            SB_GETRECT,
            index as usize,
            &mut rect as *mut _ as isize,
        ) == 0
        {
            continue;
        }
        let length = SendMessageW(hwnd, SB_GETTEXTLENGTHW, index as usize, 0) & 0xFFFF;
        if length > 0 {
            let mut text = vec![0u16; length as usize + 1];
            SendMessageW(
                hwnd,
                SB_GETTEXTW,
                index as usize,
                text.as_mut_ptr() as isize,
            );
            let mut area = rect;
            area.left += scale(6, dpi);
            area.right -= scale(4, dpi);
            DrawTextW(
                dc,
                text.as_mut_ptr(),
                length as i32,
                &mut area,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
        if index + 1 < parts {
            let divider = RECT {
                left: rect.right - 1,
                top: rect.top + scale(2, dpi),
                right: rect.right,
                bottom: rect.bottom - scale(2, dpi),
            };
            FillRect(dc, &divider, palette_brush(DARK_BORDER));
        }
    }
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
    // The grip the control would have drawn in its own colours.
    if GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & SBARS_SIZEGRIP != 0 {
        let brush = palette_brush(DARK_MUTED);
        for row in 0..3 {
            for column in 0..3 - row {
                let dot = RECT {
                    left: client.right - scale(6, dpi) - column * scale(4, dpi),
                    top: client.bottom - scale(6, dpi) - row * scale(4, dpi),
                    right: client.right - scale(4, dpi) - column * scale(4, dpi),
                    bottom: client.bottom - scale(4, dpi) - row * scale(4, dpi),
                };
                FillRect(dc, &dot, brush);
            }
        }
    }
}
pub(super) unsafe extern "system" fn status_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if scheme_dark(hwnd) {
        match msg {
            WM_PAINT => {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                paint_status(hwnd, dc);
                EndPaint(hwnd, &paint);
                return 0;
            }
            WM_PRINTCLIENT => {
                paint_status(hwnd, wparam as HDC);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            _ => {}
        }
    }
    call_previous(hwnd, msg, wparam, lparam)
}
pub(super) unsafe fn paint_theme(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if GetPropW(hwnd, dark_prop()).is_null() {
        return None;
    }
    let brush = dark_surface_brush();
    match msg {
        WM_ERASEBKGND => {
            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect);
            FillRect(wparam as HDC, &rect, brush);
            Some(1)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
            let error = GetDlgCtrlID(lparam as HWND) == D_ERROR;
            SetTextColor(wparam as HDC, if error { 0xaaaaff } else { DARK_TEXT });
            SetBkColor(wparam as HDC, DARK_BACKGROUND);
            Some(brush as LRESULT)
        }
        _ => None,
    }
}
pub(super) unsafe fn high_contrast() -> bool {
    let mut value: HIGHCONTRASTW = zeroed();
    value.cbSize = size_of::<HIGHCONTRASTW>() as u32;
    SystemParametersInfoW(
        SPI_GETHIGHCONTRAST,
        value.cbSize,
        (&mut value as *mut HIGHCONTRASTW).cast(),
        0,
    ) != 0
        && value.dwFlags & HCF_HIGHCONTRASTON != 0
}
/// uxtheme's dark-mode entry points, resolved by ordinal the way the shell
/// exposes them. Windows 11 ships all three; when a build lacks one the lookup
/// returns None and the app keeps painting what it can itself.
pub(super) type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
pub(super) type FlushMenuThemes = unsafe extern "system" fn();
pub(super) type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;
pub(super) unsafe fn uxtheme_export(ordinal: usize) -> usize {
    let module = GetModuleHandleW(wide("uxtheme.dll").as_ptr());
    if module.is_null() {
        return 0;
    }
    match GetProcAddress(module, ordinal as *const u8) {
        Some(address) => address as usize,
        None => 0,
    }
}
pub(super) unsafe fn set_preferred_app_mode() -> Option<SetPreferredAppMode> {
    static ADDRESS: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { uxtheme_export(135) });
    (*ADDRESS != 0).then(|| std::mem::transmute::<usize, SetPreferredAppMode>(*ADDRESS))
}
pub(super) unsafe fn flush_menu_themes() -> Option<FlushMenuThemes> {
    static ADDRESS: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { uxtheme_export(136) });
    (*ADDRESS != 0).then(|| std::mem::transmute::<usize, FlushMenuThemes>(*ADDRESS))
}
pub(super) unsafe fn allow_dark_mode_for_window() -> Option<AllowDarkModeForWindow> {
    static ADDRESS: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| unsafe { uxtheme_export(133) });
    (*ADDRESS != 0).then(|| std::mem::transmute::<usize, AllowDarkModeForWindow>(*ADDRESS))
}
/// Opts the process into the shell colour scheme so comctl32 draws menus,
/// combo boxes and list headers dark too. AllowDark follows the app setting and
/// ForceLight keeps a light dialog light even on a dark desktop.
pub(super) unsafe fn dark_app_mode(enabled: bool) {
    if let Some(set_mode) = set_preferred_app_mode() {
        set_mode(if enabled { 1 } else { 3 });
    }
    if let Some(flush) = flush_menu_themes() {
        flush();
    }
}
pub(super) unsafe fn apply_dark_mode(hwnd: HWND, enabled: bool) {
    let enabled = enabled && !high_contrast();
    dark_app_mode(enabled);
    if let Some(allow) = allow_dark_mode_for_window() {
        allow(hwnd, enabled as BOOL);
    }
    SetPropW(
        hwnd,
        wide("SSDownload.Dark").as_ptr(),
        if enabled {
            1usize as HANDLE
        } else {
            null_mut()
        },
    );
    let value: BOOL = enabled as BOOL;
    DwmSetWindowAttribute(
        hwnd,
        20,
        &value as *const _ as *const c_void,
        size_of::<BOOL>() as u32,
    );
    let theme = wide("DarkMode_Explorer");
    SetWindowTheme(hwnd, if enabled { theme.as_ptr() } else { null() }, null());
    EnumChildWindows(hwnd, Some(theme_child), enabled as LPARAM);
    let list = GetDlgItem(hwnd, ID_JOB_LIST);
    if !list.is_null() {
        let bg = if enabled {
            0x202020
        } else {
            GetSysColor(COLOR_WINDOW)
        };
        let fg = if enabled {
            0xeeeeee
        } else {
            GetSysColor(COLOR_WINDOWTEXT)
        };
        SendMessageW(list, LVM_SETBKCOLOR, 0, bg as isize);
        SendMessageW(list, LVM_SETTEXTBKCOLOR, 0, bg as isize);
        SendMessageW(list, LVM_SETTEXTCOLOR, 0, fg as isize);
        // The themed grid lines stay light whatever the subtheme, and there is
        // no message to recolour them: the dark table drops them, light keeps
        // them.
        let styles = SendMessageW(list, LVM_GETEXTENDEDLISTVIEWSTYLE, 0, 0) as u32;
        let wanted = if enabled {
            styles & !LVS_EX_GRIDLINES
        } else {
            styles | LVS_EX_GRIDLINES
        };
        if wanted != styles {
            SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, wanted as isize);
        }
    }
    // Children keep their previous painting until they are invalidated, so a
    // theme switch has to redraw the whole descendant tree.
    RedrawWindow(
        hwnd,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}
/// Applies the colour scheme to every descendant, including the list header
/// which is nested inside the list view rather than owned by the window.
pub(super) unsafe extern "system" fn theme_child(child: HWND, lparam: LPARAM) -> BOOL {
    let enabled = lparam != 0;
    let theme = wide("DarkMode_Explorer");
    let combo_theme = wide("DarkMode_CFD");
    // Combo boxes only follow the colour scheme under their own subtheme.
    let child_theme = if window_class(child).eq_ignore_ascii_case("ComboBox") {
        &combo_theme
    } else {
        &theme
    };
    SetWindowTheme(
        child,
        if enabled {
            child_theme.as_ptr()
        } else {
            null()
        },
        null(),
    );
    // A client edge is a light frame in dark mode with no message to recolour
    // it, so those children paint their own frame.
    if enabled && (GetWindowLongPtrW(child, GWL_EXSTYLE) as u32 & WS_EX_CLIENTEDGE) != 0 {
        subclass(child, frame_proc);
    }
    // A progress bar has no dark subtheme either.
    if enabled && window_class(child).eq_ignore_ascii_case("msctls_progress32") {
        subclass(child, progress_proc);
    }
    SendMessageW(child, WM_THEMECHANGED, 0, 0);
    1
}

pub(super) const D_URLS: i32 = 2001;
pub(super) const D_DIR: i32 = 2002;
pub(super) const D_BROWSE: i32 = 2003;
pub(super) const D_FILENAME: i32 = 2004;
pub(super) const D_KIND: i32 = 2005;
pub(super) const D_CONNECTIONS: i32 = 2006;
pub(super) const D_CHECKSUM: i32 = 2007;
pub(super) const D_SCHEDULE: i32 = 2008;
pub(super) const D_ADD_OK: i32 = 2009;
pub(super) const D_ADD_ANALYZE: i32 = 2010;
pub(super) const D_ADVANCED: i32 = 2011;
pub(super) const D_CANCEL: i32 = 2099;
pub(super) const D_ERROR: i32 = 2098;
/// Shared modal dialog metrics, in 96 dpi logical units before scale().
pub(super) const DIALOG_MARGIN: i32 = 12;
pub(super) const DIALOG_GAP: i32 = 8;
pub(super) const DIALOG_ROW: i32 = 24;
/// Paints the whole tab strip: background, titles and the selected underline.
/// The control's own paint draws a themed band under the items, which shows as
/// a dark line on the light dialog, so the strip is painted here in both
/// themes. Hit testing, selection and the TCM messages stay with comctl32.
pub(super) unsafe fn paint_tabs(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client) == 0 {
        return;
    }
    let dpi = GetDpiForWindow(hwnd).max(96);
    let palette = tab_palette(scheme_dark(hwnd));
    FillRect(dc, &client, palette.background);
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    SetBkMode(dc, TRANSPARENT as i32);
    let selected = SendMessageW(hwnd, TCM_GETCURSEL, 0, 0) as i32;
    for index in 0..SendMessageW(hwnd, TCM_GETITEMCOUNT, 0, 0) as i32 {
        let mut rect: RECT = zeroed();
        if SendMessageW(
            hwnd,
            TCM_GETITEMRECT,
            index as usize,
            &mut rect as *mut _ as isize,
        ) == 0
        {
            continue;
        }
        let mut title = tab_title(hwnd, index);
        SetTextColor(
            dc,
            if index == selected {
                palette.text_color
            } else {
                palette.muted_color
            },
        );
        let mut text = rect;
        text.top += scale(2, dpi);
        DrawTextW(
            dc,
            title.as_mut_ptr(),
            -1,
            &mut text,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        if index == selected {
            let bar = RECT {
                left: rect.left + scale(6, dpi),
                top: rect.bottom - scale(3, dpi),
                right: rect.right - scale(6, dpi),
                bottom: rect.bottom,
            };
            FillRect(dc, &bar, palette.text);
        }
    }
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
}
pub(super) unsafe extern "system" fn tabs_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            paint_tabs(hwnd, dc);
            EndPaint(hwnd, &paint);
            return 0;
        }
        WM_PRINTCLIENT => {
            paint_tabs(hwnd, wparam as HDC);
            return 0;
        }
        WM_ERASEBKGND => return 1,
        _ => {}
    }
    call_previous(hwnd, msg, wparam, lparam)
}
/// Progress bars keep the themed light fill in dark mode; the bar is painted
/// here with the palette's surface and accent, border included.
pub(super) unsafe fn paint_progress(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client) == 0 {
        return;
    }
    FillRect(dc, &client, palette_brush(DARK_SURFACE));
    let mut range = PBRANGE {
        iLow: 0,
        iHigh: 100,
    };
    SendMessageW(hwnd, PBM_GETRANGE, 1, &mut range as *mut _ as isize);
    let position = SendMessageW(hwnd, PBM_GETPOS, 0, 0) as i32;
    let span = (range.iHigh - range.iLow).max(1);
    let width = (client.right - client.left - 2).max(0);
    let filled =
        ((position - range.iLow).clamp(0, span) as i64 * width as i64 / span as i64) as i32;
    if filled > 0 {
        let bar = RECT {
            left: client.left + 1,
            top: client.top + 1,
            right: client.left + 1 + filled,
            bottom: client.bottom - 1,
        };
        FillRect(dc, &bar, palette_brush(DARK_ACCENT));
    }
    let line = palette_brush(DARK_BORDER);
    for frame in [
        RECT {
            left: client.left,
            top: client.top,
            right: client.right,
            bottom: client.top + 1,
        },
        RECT {
            left: client.left,
            top: client.bottom - 1,
            right: client.right,
            bottom: client.bottom,
        },
        RECT {
            left: client.left,
            top: client.top,
            right: client.left + 1,
            bottom: client.bottom,
        },
        RECT {
            left: client.right - 1,
            top: client.top,
            right: client.right,
            bottom: client.bottom,
        },
    ] {
        if frame.right > frame.left && frame.bottom > frame.top {
            FillRect(dc, &frame, line);
        }
    }
}
pub(super) unsafe extern "system" fn progress_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if scheme_dark(hwnd) {
        match msg {
            WM_PAINT => {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                paint_progress(hwnd, dc);
                EndPaint(hwnd, &paint);
                return 0;
            }
            WM_PRINTCLIENT => {
                paint_progress(hwnd, wparam as HDC);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            _ => {}
        }
    }
    call_previous(hwnd, msg, wparam, lparam)
}
