//! The main window's Windows 11 look: palette, type ramp, command buttons,
//! card surfaces, the striped job list and its progress bars. High contrast
//! replaces every colour here with a system colour.

use super::*;
use crate::i18n::Language;
use std::sync::atomic::AtomicU64;

/// Main window colours as COLORREFs. Each value is the WinUI theme resource
/// named next to it, composited over the layer it sits on, because GDI paints
/// opaque colours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct MainPalette {
    /// SolidBackgroundFillColorBase: the window, the title and menu bars, the command row.
    pub(super) base: u32,
    /// CardBackgroundFillColorDefault: the content layer holding the list and the tab page.
    pub(super) card: u32,
    /// CardStrokeColorDefault: the card outline and the header separators.
    pub(super) stroke: u32,
    /// TextFillColorPrimary / Secondary / Disabled.
    pub(super) text: u32,
    pub(super) secondary: u32,
    pub(super) disabled: u32,
    /// SubtleFillColorSecondary: a hovered menu bar item.
    pub(super) hover: u32,
    /// ControlFillColorDefault / Secondary / Tertiary / Disabled: a standard
    /// button at rest, hovered, pressed and disabled.
    pub(super) control: u32,
    pub(super) control_hover: u32,
    pub(super) control_pressed: u32,
    pub(super) control_disabled: u32,
    /// ControlStrokeColorDefault, and ControlStrokeColorSecondary for the
    /// bottom edge of the elevation border.
    pub(super) control_stroke: u32,
    pub(super) control_stroke_bottom: u32,
    /// AccentFillColorDefault / Secondary / Tertiary: the primary button, the
    /// running progress bar, the selection marks.
    pub(super) accent: u32,
    pub(super) accent_hover: u32,
    pub(super) accent_pressed: u32,
    /// TextOnAccentFillColorPrimary / Secondary.
    pub(super) on_accent: u32,
    pub(super) on_accent_pressed: u32,
    /// ControlStrokeColorOnAccentSecondary: the primary button's bottom edge.
    pub(super) accent_stroke_bottom: u32,
    /// FocusStrokeColorOuter / Inner: the keyboard focus ring.
    pub(super) focus_outer: u32,
    pub(super) focus_inner: u32,
    /// ControlStrongStrokeColorDefault: the progress track.
    pub(super) strong_stroke: u32,
    /// The second row colour of the list: one subtle step off the card.
    pub(super) stripe: u32,
    /// Row backplates: hovered, selected, selected and hovered. Hover is a
    /// stronger step than the stripe, selection is accent tinted, so both stay
    /// visible on either row colour.
    pub(super) row_hover: u32,
    pub(super) row_selected: u32,
    pub(super) row_selected_hover: u32,
    /// SystemFillColorSuccess / Caution / Critical: finished, paused and failed bars.
    pub(super) success: u32,
    pub(super) caution: u32,
    pub(super) critical: u32,
    /// AcrylicInAppFillColorDefault's solid fallback: the open filter list,
    /// the surface the system's menus have too.
    pub(super) flyout: u32,
}

/// The two accent shades WinUI fills with: SystemAccentColorDark1 in the light
/// theme and SystemAccentColorLight2 in the dark one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct AccentShades {
    pub(super) dark1: u32,
    pub(super) light2: u32,
}

/// `argb` (WinUI notation, 0xAARRGGBB) painted over the opaque COLORREF `below`.
pub(super) fn over(argb: u32, below: u32) -> u32 {
    let alpha = argb >> 24;
    let channel = |top: u32, bottom: u32| (top * alpha + bottom * (255 - alpha) + 127) / 255;
    let red = channel((argb >> 16) & 0xff, below & 0xff);
    let green = channel((argb >> 8) & 0xff, (below >> 8) & 0xff);
    let blue = channel(argb & 0xff, (below >> 16) & 0xff);
    red | (green << 8) | (blue << 16)
}
/// An opaque COLORREF with `alpha` (0-255), in the `over` notation.
fn with_alpha(colour: u32, alpha: u32) -> u32 {
    (alpha << 24) | ((colour & 0xff) << 16) | (colour & 0xff00) | ((colour >> 16) & 0xff)
}

/// The WCAG relative luminance of a COLORREF.
fn luminance(colour: u32) -> f64 {
    let channel = |value: u32| {
        let value = value as f64 / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(colour & 0xff)
        + 0.7152 * channel((colour >> 8) & 0xff)
        + 0.0722 * channel((colour >> 16) & 0xff)
}
/// The WCAG contrast ratio of two COLORREFs.
fn contrast(a: u32, b: u32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The Fluent palette of one scheme, from the WinUI light and dark theme dictionaries.
pub(super) fn fluent_palette(dark: bool, accent: AccentShades) -> MainPalette {
    let base = if dark { 0x202020 } else { 0xf3f3f3 };
    let card = over(if dark { 0x0dffffff } else { 0xb3ffffff }, base);
    let pick = |light: u32, dark_value: u32| if dark { dark_value } else { light };
    let on = |light: u32, dark_value: u32, below: u32| over(pick(light, dark_value), below);
    let accent_fill = pick(accent.dark1, accent.light2);
    let accent_pressed = over(with_alpha(accent_fill, 0xcc), base);
    // TextOnAccentFillColor is white in the light theme and black in the dark
    // one; a light or dark custom accent takes whichever of the two reads.
    let on_white = contrast(0xffffff, accent_fill) >= contrast(0x000000, accent_fill);
    let on_accent = if on_white { 0xffffff } else { 0x000000 };
    // The hover fill lets the base show through; where that would leave the
    // text below 4.5:1, it lets the colour opposite the text through instead.
    let accent_hover = [base, on_accent ^ 0xffffff]
        .map(|below| over(with_alpha(accent_fill, 0xe6), below))
        .into_iter()
        .find(|&fill| contrast(on_accent, fill) >= 4.5)
        .unwrap_or(accent_fill);
    let [control, control_hover, control_pressed, control_disabled, control_stroke, control_stroke_bottom] =
        control_layers(dark, base);
    MainPalette {
        base,
        card,
        stroke: on(0x0f000000, 0x19000000, base),
        text: on(0xe4000000, 0xffffffff, card),
        secondary: on(0x9e000000, 0xc5ffffff, card),
        disabled: on(0x5c000000, 0x5dffffff, base),
        hover: on(0x09000000, 0x0fffffff, base),
        control,
        control_hover,
        control_pressed,
        control_disabled,
        control_stroke,
        control_stroke_bottom,
        accent: accent_fill,
        accent_hover,
        accent_pressed,
        on_accent,
        on_accent_pressed: over(
            if on_white { 0xb3ffffff } else { 0x80000000 },
            accent_pressed,
        ),
        accent_stroke_bottom: on(0x66000000, 0x24000000, accent_fill),
        focus_outer: on(0xe4000000, 0xffffffff, base),
        focus_inner: on(0xb3ffffff, 0xb3000000, base),
        strong_stroke: on(0x72000000, 0x8bffffff, card),
        // SubtleFillColorSecondary in the light theme, SubtleFillColorTertiary
        // in the dark one: about the same step off the card in both.
        stripe: on(0x09000000, 0x0affffff, card),
        row_hover: on(0x18000000, 0x14ffffff, card),
        row_selected: over(with_alpha(accent_fill, 0x33), card),
        row_selected_hover: over(with_alpha(accent_fill, 0x47), card),
        success: pick(0x0f7b0f, 0x5fcb6c),
        caution: pick(0x005d9d, 0x00e1fc),
        critical: pick(0x1c2bc4, 0xa499ff),
        flyout: pick(0xf9f9f9, 0x2c2c2c),
    }
}

/// The control fill, hover, pressed and disabled fills (ControlFillColor*)
/// and the stroke and its bottom edge (ControlStrokeColor*), composited over
/// `below`, the layer the controls sit on.
fn control_layers(dark: bool, below: u32) -> [u32; 6] {
    let on = |light: u32, dark_value: u32| over(if dark { dark_value } else { light }, below);
    [
        on(0xb3ffffff, 0x0fffffff),
        on(0x80f9f9f9, 0x15ffffff),
        on(0x4df9f9f9, 0x08ffffff),
        on(0x4df9f9f9, 0x0bffffff),
        on(0x0f000000, 0x12ffffff),
        on(0x29000000, 0x18ffffff),
    ]
}

/// The palette of a control on a dialog's content card: the card is its
/// layer, so its fills and strokes are composited over the card.
pub(super) fn on_card(palette: MainPalette, dark: bool) -> MainPalette {
    let [control, control_hover, control_pressed, control_disabled, control_stroke, control_stroke_bottom] =
        control_layers(dark, palette.card);
    MainPalette {
        base: palette.card,
        control,
        control_hover,
        control_pressed,
        control_disabled,
        control_stroke,
        control_stroke_bottom,
        ..palette
    }
}

/// `on_card` for the scheme `hwnd` is shown in; high contrast keeps the
/// system's colours.
pub(super) unsafe fn card_palette(hwnd: HWND) -> MainPalette {
    let palette = main_palette(hwnd);
    if high_contrast() {
        MainPalette {
            base: palette.card,
            ..palette
        }
    } else {
        on_card(palette, scheme_dark(hwnd))
    }
}

/// Accent shades last read from Windows, packed `dark1 << 32 | light2`; 0 until read.
static ACCENT: AtomicU64 = AtomicU64::new(0);

/// The palette the main window paints with right now: Fluent in either scheme,
/// system colours while high contrast is on.
pub(super) unsafe fn main_palette(hwnd: HWND) -> MainPalette {
    if !high_contrast() {
        let packed = ACCENT.load(Ordering::Relaxed);
        let accent = if packed == 0 {
            read_accent_shades()
        } else {
            AccentShades {
                dark1: (packed >> 32) as u32 & 0x00ff_ffff,
                light2: packed as u32 & 0x00ff_ffff,
            }
        };
        return fluent_palette(scheme_dark(hwnd), accent);
    }
    let window = GetSysColor(COLOR_WINDOW);
    let text = GetSysColor(COLOR_WINDOWTEXT);
    let face = GetSysColor(COLOR_BTNFACE);
    let button_text = GetSysColor(COLOR_BTNTEXT);
    let highlight = GetSysColor(COLOR_HIGHLIGHT);
    let highlight_text = GetSysColor(COLOR_HIGHLIGHTTEXT);
    MainPalette {
        base: window,
        card: window,
        stroke: text,
        text,
        secondary: text,
        disabled: GetSysColor(COLOR_GRAYTEXT),
        hover: highlight,
        control: face,
        control_hover: highlight,
        control_pressed: highlight,
        control_disabled: face,
        control_stroke: button_text,
        control_stroke_bottom: button_text,
        accent: highlight,
        accent_hover: highlight,
        accent_pressed: highlight,
        on_accent: highlight_text,
        on_accent_pressed: highlight_text,
        accent_stroke_bottom: button_text,
        focus_outer: text,
        focus_inner: window,
        strong_stroke: text,
        stripe: window,
        row_hover: window,
        row_selected: window,
        row_selected_hover: window,
        success: highlight,
        caution: highlight,
        critical: highlight,
        flyout: window,
    }
}

/// The accent as DWM reports it: its colorization colour follows the Windows
/// accent. Windows' default blue when DWM does not answer.
unsafe fn dwm_accent() -> u32 {
    let mut argb = 0u32;
    let mut opaque: BOOL = 0;
    if DwmGetColorizationColor(&mut argb, &mut opaque) < 0 {
        return 0xd47800;
    }
    ((argb >> 16) & 0xff) | (argb & 0xff00) | ((argb & 0xff) << 16)
}

/// Reads the accent shades from `UISettings.GetColorValue` (Windows 10 1809 and
/// later) and remembers them for the painters; the DWM accent stands in for both
/// shades where that call is not available.
pub(super) unsafe fn read_accent_shades() -> AccentShades {
    let fallback = dwm_accent();
    let shades = ui_settings_shades().unwrap_or(AccentShades {
        dark1: fallback,
        light2: fallback,
    });
    // The top bit marks the value as read, so a black accent is not read again.
    ACCENT.store(
        ((shades.dark1 as u64) << 32) | shades.light2 as u64 | (1 << 63),
        Ordering::Relaxed,
    );
    shades
}

#[repr(C)]
struct WinRtColor {
    a: u8,
    r: u8,
    g: u8,
    b: u8,
}
const IID_IUISETTINGS3: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x03021be4_5254_4781_8194_5168f7d06d7b);
/// UIColorType.AccentDark1 and UIColorType.AccentLight2.
const ACCENT_DARK1: i32 = 4;
const ACCENT_LIGHT2: i32 = 7;

/// `Windows.UI.ViewManagement.UISettings` through combase's WinRT entry
/// points, resolved at run time the way the uxtheme exports are.
unsafe fn ui_settings_shades() -> Option<AccentShades> {
    type CreateString = unsafe extern "system" fn(*const u16, u32, *mut *mut c_void) -> i32;
    type DeleteString = unsafe extern "system" fn(*mut c_void) -> i32;
    type Activate = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32;
    type QueryInterface = unsafe extern "system" fn(
        *mut c_void,
        *const windows_sys::core::GUID,
        *mut *mut c_void,
    ) -> i32;
    type Release = unsafe extern "system" fn(*mut c_void) -> u32;
    type GetColorValue = unsafe extern "system" fn(*mut c_void, i32, *mut WinRtColor) -> i32;
    let module = GetModuleHandleW(wide("combase.dll").as_ptr());
    if module.is_null() {
        return None;
    }
    let create: CreateString = std::mem::transmute(GetProcAddress(
        module,
        c"WindowsCreateString".as_ptr().cast(),
    )?);
    let delete: DeleteString = std::mem::transmute(GetProcAddress(
        module,
        c"WindowsDeleteString".as_ptr().cast(),
    )?);
    let activate: Activate = std::mem::transmute(GetProcAddress(
        module,
        c"RoActivateInstance".as_ptr().cast(),
    )?);
    let class: Vec<u16> = "Windows.UI.ViewManagement.UISettings"
        .encode_utf16()
        .collect();
    let mut name: *mut c_void = null_mut();
    if create(class.as_ptr(), class.len() as u32, &mut name) < 0 {
        return None;
    }
    let mut instance: *mut c_void = null_mut();
    let activated = activate(name, &mut instance);
    delete(name);
    if activated < 0 || instance.is_null() {
        return None;
    }
    let vtable = *(instance as *const *const usize);
    let query: QueryInterface = std::mem::transmute(*vtable);
    let release: Release = std::mem::transmute(*vtable.add(2));
    let mut settings: *mut c_void = null_mut();
    let found = query(instance, &IID_IUISETTINGS3, &mut settings);
    release(instance);
    if found < 0 || settings.is_null() {
        return None;
    }
    let vtable = *(settings as *const *const usize);
    let get: GetColorValue = std::mem::transmute(*vtable.add(6));
    let release: Release = std::mem::transmute(*vtable.add(2));
    let read = |kind: i32| {
        let mut colour = WinRtColor {
            a: 0,
            r: 0,
            g: 0,
            b: 0,
        };
        (get(settings, kind, &mut colour) >= 0)
            .then_some(colour.r as u32 | (colour.g as u32) << 8 | (colour.b as u32) << 16)
    };
    let shades = read(ACCENT_DARK1).zip(read(ACCENT_LIGHT2));
    release(settings);
    shades.map(|(dark1, light2)| AccentShades { dark1, light2 })
}

/// Faces for the Body and Caption styles of the type ramp. Segoe UI Variable
/// covers Latin, Greek and Cyrillic only, so Arabic stays on Segoe UI; Windows
/// 10 has no Segoe UI Variable and gets Segoe UI through the same fallback.
pub(super) fn ramp_faces(language: Language, caption: bool) -> &'static [&'static str] {
    match (language, caption) {
        (Language::Arabic, _) => &["Segoe UI"],
        (_, false) => &["Segoe UI Variable Text", "Segoe UI"],
        (_, true) => &["Segoe UI Variable Small", "Segoe UI"],
    }
}
/// A type ramp font `px` effective pixels high.
pub(super) unsafe fn ramp_font(dpi: u32, px: i32, caption: bool) -> HFONT {
    face_font(
        -scale(px, dpi),
        FW_NORMAL as i32,
        CLEARTYPE_QUALITY,
        ramp_faces(crate::i18n::language(), caption),
    )
    .unwrap_or_else(|| make_font(dpi, px * 3 / 4, FW_NORMAL as i32))
}
/// Body: commands, rows, tabs and the tab page (14/20 epx).
pub(super) const BODY_PX: i32 = 14;
/// Caption: the status line and the column headers (12/16 epx).
pub(super) const CAPTION_PX: i32 = 12;

/// Icon fonts in preference order: Windows 11, then Windows 10. A substituted
/// font would draw boxes, so without either the commands show text only.
const ICON_FACES: [&str; 2] = ["Segoe Fluent Icons", "Segoe MDL2 Assets"];
pub(super) unsafe fn icon_font(dpi: u32) -> HFONT {
    icon_font_px(dpi, 16)
}
/// Glyphs are smoothed in grey: ClearType gives their thin strokes coloured
/// fringes.
pub(super) unsafe fn icon_font_px(dpi: u32, px: i32) -> HFONT {
    face_font(
        -scale(px, dpi),
        FW_NORMAL as i32,
        ANTIALIASED_QUALITY,
        &ICON_FACES,
    )
    .unwrap_or(null_mut())
}
/// Glyphs of the commands, in `MAIN_BUTTONS` order: Add, Setting, Pause,
/// Delete, OpenFolder.
const COMMAND_GLYPHS: [u16; 5] = [0xe710, 0xe713, 0xe769, 0xe74d, 0xe838];

/// Fills `rect` with `colour` through the DC brush, leaving the selected brush alone.
pub(super) unsafe fn fill_colour(dc: HDC, rect: &RECT, colour: u32) {
    SetDCBrushColor(dc, colour);
    FillRect(dc, rect, GetStockObject(DC_BRUSH) as HBRUSH);
}
/// A rounded rectangle: `fill` inside, outlined in `stroke` unless it is None.
pub(super) unsafe fn round_rect(dc: HDC, rect: &RECT, radius: i32, fill: u32, stroke: Option<u32>) {
    let previous_pen = SelectObject(
        dc,
        GetStockObject(if stroke.is_some() { DC_PEN } else { NULL_PEN }),
    );
    let previous_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
    SetDCBrushColor(dc, fill);
    if let Some(colour) = stroke {
        SetDCPenColor(dc, colour);
    }
    // Without a pen the interior is one pixel short on the right and bottom.
    let extra = if stroke.is_some() { 0 } else { 1 };
    RoundRect(
        dc,
        rect.left,
        rect.top,
        rect.right + extra,
        rect.bottom + extra,
        radius * 2,
        radius * 2,
    );
    SelectObject(dc, previous_brush);
    SelectObject(dc, previous_pen);
}

/// The keyboard focus visual of a button inside `rect`: a 2 epx ring in
/// FocusStrokeColorOuter, a 1 epx ring in FocusStrokeColorInner, and `fill`
/// within them.
pub(super) unsafe fn focus_plate(
    dc: HDC,
    rect: &RECT,
    radius: i32,
    dpi: u32,
    palette: &MainPalette,
    fill: u32,
) {
    let outer = scale(2, dpi).max(2);
    let inner = scale(1, dpi).max(1);
    round_rect(dc, rect, radius, palette.focus_outer, None);
    let ring = inflate(*rect, -outer, -outer);
    round_rect(
        dc,
        &ring,
        (radius - outer).max(0),
        palette.focus_inner,
        None,
    );
    let face = inflate(ring, -inner, -inner);
    round_rect(dc, &face, (radius - outer - inner).max(0), fill, None);
}

/// How much of the pixel whose centre is (`x`, `y`) a rectangle with corners
/// of `radius` covers, 0 to 1: the anti-aliased edge GDI's RoundRect lacks.
pub(super) fn rounded_coverage(x: f32, y: f32, rect: [f32; 4], radius: f32) -> f32 {
    let [left, top, right, bottom] = rect;
    let radius = radius
        .max(0.0)
        .min((right - left) / 2.0)
        .min((bottom - top) / 2.0);
    let qx = (x - (left + right) / 2.0).abs() - ((right - left) / 2.0 - radius);
    let qy = (y - (top + bottom) / 2.0).abs() - ((bottom - top) / 2.0 - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let distance = outside + qx.max(qy).min(0.0) - radius;
    (0.5 - distance).clamp(0.0, 1.0)
}
fn blend(below: u32, above: u32, amount: f32) -> u32 {
    if amount <= 0.0 {
        return below;
    }
    let amount = amount.min(1.0);
    let channel = |shift: u32| {
        let a = ((below >> shift) & 0xff) as f32;
        let b = ((above >> shift) & 0xff) as f32;
        ((a + (b - a) * amount).round() as u32) << shift
    };
    channel(0) | channel(8) | channel(16)
}

/// One button surface: what `paint_button_surface` draws under the content.
pub(super) struct ButtonSurface {
    pub(super) background: u32,
    pub(super) fill: u32,
    pub(super) stroke: u32,
    pub(super) stroke_bottom: u32,
    pub(super) radius: f32,
    /// Outer and inner ring colours while the button shows keyboard focus.
    pub(super) focus: Option<(u32, u32)>,
}
/// Colours of a `width` x `height` button as COLORREFs, row by row: the
/// background, a 1 epx elevation border whose bottom edge is darker, the fill
/// and the two-ring focus visual `margin` pixels outside the border.
pub(super) fn paint_button_surface(
    width: usize,
    height: usize,
    margin: f32,
    surface: &ButtonSurface,
) -> Vec<u32> {
    let body = [
        margin,
        margin,
        width as f32 - margin,
        height as f32 - margin,
    ];
    let grow = |by: f32| [body[0] - by, body[1] - by, body[2] + by, body[3] + by];
    let mut pixels = vec![surface.background; width * height];
    for y in 0..height {
        for x in 0..width {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut colour = surface.background;
            // The rings fill the margin: the outer two thirds of it, then the
            // inner third, 2 + 1 px at 96 dpi.
            if let Some((outer, inner)) = surface.focus {
                let inner_width = margin / 3.0;
                let ring = rounded_coverage(px, py, grow(margin), surface.radius + margin);
                let inside_outer =
                    rounded_coverage(px, py, grow(inner_width), surface.radius + inner_width);
                colour = blend(colour, outer, ring - inside_outer);
                colour = blend(
                    colour,
                    inner,
                    inside_outer - rounded_coverage(px, py, body, surface.radius),
                );
            }
            let edge = rounded_coverage(px, py, body, surface.radius);
            let fill = rounded_coverage(px, py, grow(-1.0), surface.radius - 1.0);
            let stroke = if py > body[3] - surface.radius.max(1.0) {
                surface.stroke_bottom
            } else {
                surface.stroke
            };
            colour = blend(colour, stroke, edge - fill);
            colour = blend(colour, surface.fill, fill);
            pixels[y * width + x] = colour;
        }
    }
    pixels
}

/// A child's rectangle in its parent's client coordinates, left before right
/// also in a mirrored window.
pub(super) unsafe fn child_rect(parent: HWND, child: HWND) -> RECT {
    let mut rect: RECT = zeroed();
    GetWindowRect(child, &mut rect);
    let mut points = [
        POINT {
            x: rect.left,
            y: rect.top,
        },
        POINT {
            x: rect.right,
            y: rect.bottom,
        },
    ];
    MapWindowPoints(null_mut(), parent, points.as_mut_ptr(), 2);
    RECT {
        left: points[0].x.min(points[1].x),
        top: points[0].y,
        right: points[0].x.max(points[1].x),
        bottom: points[1].y,
    }
}
pub(super) fn inflate(rect: RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: rect.left - dx,
        top: rect.top - dy,
        right: rect.right + dx,
        bottom: rect.bottom + dy,
    }
}

/// Space between a card's edge and the list it holds: the 1 epx stroke plus a
/// pixel that keeps the list's square corners inside the rounded outline.
pub(super) const LIST_INSET: i32 = 2;
/// Padding between the tab page card's edge and its text.
pub(super) const PAGE_PAD_X: i32 = 16;
pub(super) const PAGE_PAD_Y: i32 = 12;
/// ControlCornerRadius: buttons, cards and row backplates.
pub(super) const CORNER: i32 = 4;
/// The command row: the space above it, the 32 epx height of a standard
/// button and the space between it and the list.
pub(super) const ROW_TOP: i32 = 16;
pub(super) const ROW_HEIGHT: i32 = 32;
pub(super) const ROW_BELOW: i32 = 16;
/// The split between the list card and the tab strip, which also drags it.
pub(super) const SPLIT_GAP: i32 = 8;
pub(super) const TABS_HEIGHT: i32 = 40;
/// The least the tab page and the list keep when the split moves or the
/// window shrinks: the list keeps its header and three rows, and the page
/// gives way first.
pub(super) const MIN_PAGE_HEIGHT: i32 = 64;
pub(super) const MIN_LIST_HEIGHT: i32 = 136;
/// Room the focus ring takes outside a button, on every side.
pub(super) const FOCUS_MARGIN: i32 = 3;
/// A button's horizontal padding and the gap between its icon and its label.
const BUTTON_PAD: i32 = 12;
const BUTTON_GAP: i32 = 8;

/// Paints the base layer and the two content cards behind the main window's children.
pub(super) unsafe fn paint_main_surfaces(ui: &MainUi, dc: HDC) {
    let palette = main_palette(ui.hwnd);
    let mut client: RECT = zeroed();
    GetClientRect(ui.hwnd, &mut client);
    fill_colour(dc, &client, palette.base);
    if ui.list.is_null() || ui.details.is_null() {
        return;
    }
    let radius = scale(CORNER, ui.dpi);
    let inset = scale(LIST_INSET, ui.dpi);
    for card in [
        inflate(child_rect(ui.hwnd, ui.list), inset, inset),
        ui.page_card.get(),
    ] {
        round_rect(dc, &card, radius, palette.card, Some(palette.stroke));
    }
    if ui.split_hot || ui.splitting.is_some() {
        // The split's grip: a short pill across the middle of the band.
        let band = split_band(ui);
        let (half_w, half_h) = (scale(16, ui.dpi), scale(2, ui.dpi).max(2) / 2);
        let (x, y) = ((band.left + band.right) / 2, (band.top + band.bottom) / 2);
        let grip = RECT {
            left: x - half_w,
            top: y - half_h,
            right: x + half_w,
            bottom: y - half_h + scale(2, ui.dpi).max(2),
        };
        round_rect(dc, &grip, scale(1, ui.dpi), palette.strong_stroke, None);
    }
    let frame = ui.search_frame.get();
    if frame.right > frame.left {
        paint_input_frame(dc, &frame, ui.dpi, &palette, GetFocus() == ui.search);
        if !ui.icon_font.is_null() {
            // The query glyph at the frame's right end, as in a WinUI AutoSuggestBox.
            let mut glyph = RECT {
                left: frame.right - scale(SEARCH_GLYPH, ui.dpi),
                ..frame
            };
            let previous = SelectObject(dc, ui.icon_font as HGDIOBJ);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, palette.secondary);
            DrawTextW(
                dc,
                &0xe721u16,
                1,
                &mut glyph,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(dc, previous);
        }
    }
}

/// The search box: 140 to 240 epx wide, the right end kept for the query glyph.
pub(super) const SEARCH_MIN: i32 = 140;
pub(super) const SEARCH_MAX: i32 = 240;
const SEARCH_GLYPH: i32 = 32;

/// Puts the search edit inside its frame, centred on the frame's height and
/// clear of the glyph.
pub(super) unsafe fn place_search(edit: HWND, frame: RECT, dpi: u32) {
    let font = SendMessageW(edit, WM_GETFONT, 0, 0) as HFONT;
    let height = line_height(edit, font).max(1);
    let left = frame.left + scale(INPUT_PAD_LEFT, dpi);
    MoveWindow(
        edit,
        left,
        (frame.top + frame.bottom - height) / 2,
        frame.right - scale(SEARCH_GLYPH, dpi) - left,
        height,
        1,
    );
}

/// A text field's fill and text: the control fill, or the system's window
/// colours while high contrast is on.
pub(super) unsafe fn input_colours(palette: &MainPalette) -> (u32, u32) {
    if high_contrast() {
        (GetSysColor(COLOR_WINDOW), GetSysColor(COLOR_WINDOWTEXT))
    } else {
        (palette.control, palette.text)
    }
}

/// A WinUI text box frame: control fill, stroke, and a stronger bottom line
/// (TextControlElevationBorder) that turns accent and 2 epx with the focus.
pub(super) unsafe fn paint_input_frame(
    dc: HDC,
    frame: &RECT,
    dpi: u32,
    palette: &MainPalette,
    focused: bool,
) {
    round_rect(
        dc,
        frame,
        scale(CORNER, dpi),
        input_colours(palette).0,
        Some(palette.control_stroke),
    );
    let line = if focused { scale(2, dpi) } else { 1 };
    let inset = scale(CORNER, dpi) / 2;
    fill_colour(
        dc,
        &RECT {
            left: frame.left + inset,
            top: frame.bottom - line,
            right: frame.right - inset,
            bottom: frame.bottom,
        },
        if focused {
            palette.accent
        } else {
            palette.strong_stroke
        },
    );
}

/// Text and background of the tab page text and of the search box.
pub(super) unsafe fn main_control_colour(ui: &MainUi, dc: HDC, control: HWND) -> Option<LRESULT> {
    let palette = main_palette(ui.hwnd);
    let (fill, text) = if control == ui.details {
        (palette.card, palette.text)
    } else if control == ui.search {
        input_colours(&palette)
    } else {
        return None;
    };
    SetTextColor(dc, text);
    SetBkColor(dc, fill);
    SetDCBrushColor(dc, fill);
    Some(GetStockObject(DC_BRUSH) as LRESULT)
}

fn hot_prop() -> *const u16 {
    static NAME: std::sync::LazyLock<Vec<u16>> =
        std::sync::LazyLock::new(|| wide("SSDownload.ButtonHot"));
    NAME.as_ptr()
}
/// Tracks the pointer over a command button for its hover state, and turns a
/// fast second click into a click: an owner-drawn button would read it as a
/// double-click and send no BN_CLICKED.
pub(super) unsafe extern "system" fn button_proc(
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
            InvalidateRect(hwnd, null(), 0);
        }
        WM_MOUSELEAVE => {
            RemovePropW(hwnd, hot_prop());
            InvalidateRect(hwnd, null(), 0);
        }
        WM_LBUTTONDBLCLK => {
            return call_previous(hwnd, WM_LBUTTONDOWN, wparam, lparam);
        }
        WM_ERASEBKGND => return 1,
        WM_NCDESTROY => {
            RemovePropW(hwnd, hot_prop());
        }
        _ => {}
    }
    call_previous(hwnd, msg, wparam, lparam)
}

/// Draws an empty, unfocused edit's cue banner in the secondary text colour
/// (TextFillColorSecondary, as WinUI draws a placeholder): the edit's own cue
/// colour is too faint on the dark field. The edit paints itself otherwise.
pub(super) unsafe extern "system" fn cue_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT if GetWindowTextLengthW(hwnd) == 0 && GetFocus() != hwnd => {
            let mut paint: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            paint_cue(hwnd, dc);
            EndPaint(hwnd, &paint);
            return 0;
        }
        WM_PRINTCLIENT if GetWindowTextLengthW(hwnd) == 0 && GetFocus() != hwnd => {
            paint_cue(hwnd, wparam as HDC);
            return 0;
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let result = call_previous(hwnd, msg, wparam, lparam);
            InvalidateRect(hwnd, null(), 1);
            return result;
        }
        _ => {}
    }
    call_previous(hwnd, msg, wparam, lparam)
}

unsafe fn paint_cue(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    // The field's fill is the parent's answer for the edit.
    let brush =
        SendMessageW(GetParent(hwnd), WM_CTLCOLOREDIT, dc as usize, hwnd as isize) as HBRUSH;
    FillRect(
        dc,
        &client,
        if brush.is_null() {
            GetSysColorBrush(COLOR_WINDOW)
        } else {
            brush
        },
    );
    let mut cue = [0u16; 256];
    SendMessageW(
        hwnd,
        EM_GETCUEBANNER,
        cue.as_mut_ptr() as usize,
        cue.len() as isize,
    );
    let length = cue.iter().position(|&c| c == 0).unwrap_or(0);
    if length == 0 {
        return;
    }
    let mut area: RECT = zeroed();
    SendMessageW(hwnd, EM_GETRECT, 0, &mut area as *mut _ as isize);
    let previous = SelectObject(dc, SendMessageW(hwnd, WM_GETFONT, 0, 0) as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(
        dc,
        if high_contrast() {
            GetSysColor(COLOR_GRAYTEXT)
        } else {
            main_palette(hwnd).secondary
        },
    );
    DrawTextW(
        dc,
        cue.as_mut_ptr(),
        length as i32,
        &mut area,
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    SelectObject(dc, previous);
}

/// Places a drop-down list box so that its closed box is `rect`. The closed
/// box is the selection field plus a frame whose size the theme decides, and
/// moving the box measures the field from the font again, so the field is
/// sized after the move until the measured box has the height.
pub(super) unsafe fn place_combo(combo: HWND, rect: RECT, dpi: u32) {
    let height = rect.bottom - rect.top;
    MoveWindow(
        combo,
        rect.left,
        rect.top,
        rect.right - rect.left,
        height + scale(200, dpi),
        1,
    );
    for _ in 0..3 {
        let mut closed: RECT = zeroed();
        GetWindowRect(combo, &mut closed);
        let field = SendMessageW(combo, CB_GETITEMHEIGHT, usize::MAX, 0);
        let difference = (height - (closed.bottom - closed.top)) as isize;
        if difference == 0 || field + difference < 1 {
            break;
        }
        SendMessageW(combo, CB_SETITEMHEIGHT, usize::MAX, field + difference);
    }
}

/// The width of a combo box's chevron area.
const COMBO_CHEVRON: i32 = 32;
/// The face width a Modern combo box needs for its widest entry: the text
/// 11 epx in and the chevron's 32 epx after it, the sums `paint_combo` takes.
pub(super) unsafe fn combo_face_width(combo: HWND, dpi: u32) -> i32 {
    let dc = GetDC(combo);
    let previous = SelectObject(dc, SendMessageW(combo, WM_GETFONT, 0, 0) as HGDIOBJ);
    let mut widest = 0;
    for index in 0..SendMessageW(combo, CB_GETCOUNT, 0, 0).max(0) as usize {
        let length = SendMessageW(combo, CB_GETLBTEXTLEN, index, 0);
        if length <= 0 {
            continue;
        }
        let mut item = vec![0u16; length as usize + 1];
        SendMessageW(combo, CB_GETLBTEXT, index, item.as_mut_ptr() as isize);
        widest = widest.max(label_width(
            dc,
            &String::from_utf16_lossy(&item[..length as usize]),
        ));
    }
    SelectObject(dc, previous);
    ReleaseDC(combo, dc);
    widest + scale(INPUT_PAD_LEFT, dpi) + scale(COMBO_CHEVRON, dpi)
}

/// The face width a Modern text field needs to show its whole cue banner: the
/// text and the frame's padding on both sides.
pub(super) unsafe fn cue_face_width(edit: HWND, dpi: u32) -> i32 {
    let mut cue = [0u16; 256];
    SendMessageW(
        edit,
        EM_GETCUEBANNER,
        cue.as_mut_ptr() as usize,
        cue.len() as isize,
    );
    let length = cue.iter().position(|&c| c == 0).unwrap_or(0);
    let dc = GetDC(edit);
    let previous = SelectObject(dc, SendMessageW(edit, WM_GETFONT, 0, 0) as HGDIOBJ);
    let width = label_width(dc, &String::from_utf16_lossy(&cue[..length]));
    SelectObject(dc, previous);
    ReleaseDC(edit, dc);
    width + scale(INPUT_PAD_LEFT + INPUT_PAD_RIGHT, dpi)
}

/// Paints a closed drop-down list box as a WinUI ComboBox: the standard
/// button's surface, states and focus ring, the selection 11 epx in from the
/// left, and the chevron at the right. The open list stays the system's.
/// High contrast keeps the system's box.
pub(super) unsafe extern "system" fn combo_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if high_contrast() {
        return call_previous(hwnd, msg, wparam, lparam);
    }
    match msg {
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            paint_combo(hwnd, dc);
            EndPaint(hwnd, &paint);
            return 0;
        }
        WM_PRINTCLIENT => {
            paint_combo(hwnd, wparam as HDC);
            return 0;
        }
        WM_ERASEBKGND => return 1,
        WM_MOUSEMOVE if GetPropW(hwnd, hot_prop()).is_null() => {
            SetPropW(hwnd, hot_prop(), 1usize as HANDLE);
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
            InvalidateRect(hwnd, null(), 0);
        }
        WM_MOUSELEAVE => {
            RemovePropW(hwnd, hot_prop());
            InvalidateRect(hwnd, null(), 0);
        }
        // The states the face shows: focus, keyboard cues, enabled, open.
        WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE | WM_UPDATEUISTATE | WM_LBUTTONDOWN
        | WM_LBUTTONUP | WM_KEYDOWN | CB_SHOWDROPDOWN | CB_SETCURSEL => {
            let result = call_previous(hwnd, msg, wparam, lparam);
            InvalidateRect(hwnd, null(), 0);
            return result;
        }
        WM_NCDESTROY => {
            RemovePropW(hwnd, hot_prop());
        }
        _ => {}
    }
    call_previous(hwnd, msg, wparam, lparam)
}

unsafe fn paint_combo(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    let dpi = GetDpiForWindow(hwnd).max(96);
    // The filter sits on the base layer, a dialog's box on its content card.
    let palette = if GetDlgCtrlID(hwnd) == ID_FILTER {
        main_palette(hwnd)
    } else {
        card_palette(hwnd)
    };
    let disabled = IsWindowEnabled(hwnd) == 0;
    let pressed = !disabled && SendMessageW(hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0;
    let look = ButtonState {
        primary: false,
        disabled,
        pressed,
        hot: !disabled && !GetPropW(hwnd, hot_prop()).is_null(),
        focused: GetFocus() == hwnd
            && SendMessageW(hwnd, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0,
        underline: false,
    };
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0) as HFONT;
    paint_button(dc, client, dpi, &palette, font, &[], None, look);
    let face = inflate(client, -scale(FOCUS_MARGIN, dpi), -scale(FOCUS_MARGIN, dpi));
    let chevron_w = scale(COMBO_CHEVRON, dpi);
    let colour = if disabled {
        palette.disabled
    } else if pressed {
        palette.secondary
    } else {
        palette.text
    };
    let mut left = face.left + scale(INPUT_PAD_LEFT, dpi);
    // The filter shows the chosen entry's glyph, as its open list does.
    if GetDlgCtrlID(hwnd) == ID_FILTER {
        if let Some(&code) = FILTER_GLYPHS.get(combo_index(hwnd).max(0) as usize) {
            let size = scale(16, dpi);
            draw_glyph(
                dc,
                client.left + filter_glyph_x(dpi),
                (face.top + face.bottom - size) / 2,
                dpi,
                code,
                colour,
            );
            left += size + scale(8, dpi);
        }
    }
    let previous = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, colour);
    let mut label: Vec<u16> = text(hwnd).encode_utf16().collect();
    let mut area = RECT {
        left,
        right: face.right - chevron_w,
        ..face
    };
    draw_text(
        dc,
        &mut label,
        &mut area,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    let chevrons = icon_font_px(dpi, 12);
    if !chevrons.is_null() {
        SelectObject(dc, chevrons as HGDIOBJ);
        SetTextColor(
            dc,
            if disabled {
                palette.disabled
            } else {
                palette.secondary
            },
        );
        let mut mark = RECT {
            left: face.right - chevron_w,
            ..face
        };
        DrawTextW(
            dc,
            &0xe70du16,
            1,
            &mut mark,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(dc, font as HGDIOBJ);
        DeleteObject(chevrons as HGDIOBJ);
    }
    SelectObject(dc, previous);
}

/// The tooltip that names a command button while only its icon shows.
pub(super) unsafe fn create_button_tooltip(ui: &mut MainUi) {
    ui.tooltip = CreateWindowExW(
        WS_EX_TOPMOST,
        wide("tooltips_class32").as_ptr(),
        null(),
        WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        ui.hwnd,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    for button in MAIN_BUTTONS {
        let mut tool: TTTOOLINFOW = zeroed();
        tool.cbSize = size_of::<TTTOOLINFOW>() as u32;
        tool.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
        tool.hwnd = ui.hwnd;
        tool.uId = GetDlgItem(ui.hwnd, button.0) as usize;
        // LPSTR_TEXTCALLBACKW: the tooltip asks for the text when it shows.
        tool.lpszText = usize::MAX as *mut u16;
        SendMessageW(ui.tooltip, TTM_ADDTOOLW, 0, &tool as *const _ as isize);
    }
    SendMessageW(ui.tooltip, TTM_ACTIVATE, 0, 0);
}

/// The tooltip asks for a button's name: its label in the current language,
/// and the command's shortcut key where it has one.
pub(super) unsafe fn button_tooltip(lparam: LPARAM) {
    let info = &mut *(lparam as *mut NMTTDISPINFOW);
    let button = info.hdr.idFrom as HWND;
    let (name, _) = split_mnemonic(&text(button));
    let key = match GetDlgCtrlID(button) {
        ID_ADD => Some("Ctrl+N"),
        ID_PAUSE_RESUME => Some(crate::i18n::ui("Boşluk", "Space")),
        ID_REMOVE => Some("Delete"),
        _ => None,
    };
    let tip = match key {
        Some(key) => format!("{name} ({key})"),
        None => name,
    };
    let label: Vec<u16> = tip.encode_utf16().collect();
    let length = label.len().min(info.szText.len() - 1);
    info.szText[..length].copy_from_slice(&label[..length]);
    info.szText[length] = 0;
    info.lpszText = info.szText.as_mut_ptr();
}

/// The primary command: the one accent button of the row.
const PRIMARY: i32 = ID_ADD;

/// Each button's visible width: padding, icon, gap and label, or padding and
/// icon alone while labels are hidden. The primary button keeps its label.
pub(super) unsafe fn button_widths(ui: &MainUi, labels: bool) -> [i32; 5] {
    let mut widths = [0; 5];
    let icon = if ui.icon_font.is_null() {
        0
    } else {
        scale(16, ui.dpi)
    };
    let dc = GetDC(ui.hwnd);
    if dc.is_null() {
        return widths;
    }
    let previous = SelectObject(dc, ui.font as HGDIOBJ);
    for (index, button) in MAIN_BUTTONS.iter().enumerate() {
        let mut width = scale(BUTTON_PAD * 2, ui.dpi) + icon;
        if labels || icon == 0 || button.0 == PRIMARY {
            // The pause button fits each of its labels.
            let label = if button.0 == ID_PAUSE_RESUME {
                PAUSE_VERBS
                    .iter()
                    .map(|verb| label_width(dc, pause_button_label(*verb)))
                    .max()
                    .unwrap_or(0)
            } else {
                label_width(dc, &text(GetDlgItem(ui.hwnd, button.0)))
            };
            width += label
                + if icon == 0 {
                    0
                } else {
                    scale(BUTTON_GAP, ui.dpi)
                };
        } else {
            // An icon-only button is as wide as it is high.
            width = scale(ROW_HEIGHT, ui.dpi);
        }
        widths[index] = width;
    }
    SelectObject(dc, previous);
    ReleaseDC(ui.hwnd, dc);
    widths
}

/// The width of a label in the font selected into `dc`, without the `&`
/// that marks its access key.
pub(super) unsafe fn label_width(dc: HDC, label: &str) -> i32 {
    let mut label: Vec<u16> = label.encode_utf16().collect();
    let mut area: RECT = zeroed();
    draw_text(dc, &mut label, &mut area, DT_CALCRECT | DT_SINGLELINE);
    area.right - area.left
}

/// DrawTextW over `text`. DrawTextW reads its string even for a count of 0,
/// and an empty slice's pointer points at nothing, so empty text is passed as
/// a real empty string.
pub(super) unsafe fn draw_text(dc: HDC, text: &mut [u16], area: &mut RECT, format: u32) -> i32 {
    let count = text.len() as i32;
    let mut empty = [0u16];
    let chars = if text.is_empty() {
        &mut empty[..]
    } else {
        text
    };
    DrawTextW(dc, chars.as_mut_ptr(), count, area, format)
}

/// A button's state for painting.
#[derive(Clone, Copy)]
pub(super) struct ButtonState {
    /// The accent button: the window's or the dialog's primary action.
    pub(super) primary: bool,
    pub(super) disabled: bool,
    pub(super) pressed: bool,
    pub(super) hot: bool,
    pub(super) focused: bool,
    /// The access key is underlined: the keyboard cues are on.
    pub(super) underline: bool,
}

/// Draws one command button into its DRAWITEMSTRUCT with its icon, and the
/// label unless the row hides labels.
pub(super) unsafe fn draw_main_button(ui: &MainUi, item: &DRAWITEMSTRUCT) {
    let id = item.CtlID as i32;
    let Some(index) = MAIN_BUTTONS.iter().position(|(button, _, _)| *button == id) else {
        return;
    };
    let state = item.itemState;
    let disabled = state & ODS_DISABLED != 0;
    let look = ButtonState {
        primary: id == PRIMARY,
        disabled,
        pressed: !disabled && state & ODS_SELECTED != 0,
        hot: !disabled && !GetPropW(item.hwndItem, hot_prop()).is_null(),
        focused: state & ODS_FOCUS != 0 && state & ODS_NOFOCUSRECT == 0,
        underline: state & ODS_NOACCEL == 0,
    };
    let text = super::text(item.hwndItem);
    // A resume shows the play glyph.
    let glyph = if id == ID_PAUSE_RESUME && text == pause_button_label(PauseVerb::Resume) {
        0xe768
    } else {
        COMMAND_GLYPHS[index]
    };
    let label: Vec<u16> = text.encode_utf16().collect();
    let icon = (!ui.icon_font.is_null()).then_some((ui.icon_font, glyph));
    let show_label = icon.is_none() || id == PRIMARY || !ui.labels_hidden.get();
    paint_button(
        item.hDC,
        item.rcItem,
        ui.dpi,
        &main_palette(ui.hwnd),
        ui.font,
        if show_label { &label } else { &[] },
        icon,
        look,
    );
}

/// Paints a WinUI button into `rect` of `dc`: the standard button, or the
/// accent button for a primary action, in its rest, hover, pressed, disabled
/// and keyboard-focus states, with an optional icon before the label. The
/// rectangle reaches FOCUS_MARGIN past the face on every side for the focus
/// ring and is filled with the base colour around the face.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_button(
    dc: HDC,
    rect: RECT,
    dpi: u32,
    palette: &MainPalette,
    font: HFONT,
    label: &[u16],
    icon: Option<(HFONT, u16)>,
    look: ButtonState,
) {
    let contrast = high_contrast();
    let ButtonState {
        disabled,
        pressed,
        hot,
        focused,
        underline,
        ..
    } = look;
    let primary = look.primary && !contrast;
    let (fill, text) = match (primary, disabled, pressed, hot) {
        (_, true, _, _) if primary => (over(0x37000000, palette.base), palette.on_accent),
        (_, true, _, _) => (palette.control_disabled, palette.disabled),
        (true, _, true, _) => (palette.accent_pressed, palette.on_accent_pressed),
        (true, _, _, true) => (palette.accent_hover, palette.on_accent),
        (true, _, _, _) => (palette.accent, palette.on_accent),
        (false, _, true, _) if contrast => {
            (palette.control_pressed, GetSysColor(COLOR_HIGHLIGHTTEXT))
        }
        (false, _, _, true) if contrast => {
            (palette.control_hover, GetSysColor(COLOR_HIGHLIGHTTEXT))
        }
        (false, _, true, _) => (palette.control_pressed, palette.secondary),
        (false, _, _, true) => (palette.control_hover, palette.text),
        _ if contrast => (palette.control, GetSysColor(COLOR_BTNTEXT)),
        _ => (palette.control, palette.text),
    };
    let (stroke, stroke_bottom) = if primary {
        (fill, palette.accent_stroke_bottom)
    } else if pressed || disabled {
        (palette.control_stroke, palette.control_stroke)
    } else {
        (palette.control_stroke, palette.control_stroke_bottom)
    };
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let surface = ButtonSurface {
        background: palette.base,
        fill,
        stroke,
        stroke_bottom: if pressed { stroke } else { stroke_bottom },
        // High contrast keeps the plain, square look.
        radius: if contrast {
            0.0
        } else {
            scale(CORNER, dpi) as f32
        },
        focus: focused.then_some((palette.focus_outer, palette.focus_inner)),
    };
    let pixels = paint_button_surface(
        width as usize,
        height as usize,
        scale(FOCUS_MARGIN, dpi) as f32,
        &surface,
    );
    blit_surface(dc, rect, &pixels, |memory| {
        draw_button_content(
            memory, width, height, dpi, font, text, label, icon, underline,
        )
    });
}

/// Copies `pixels` (COLORREFs, row by row) into `rect` of `dc` through a
/// memory DC, after `draw` has painted over them there with a transparent
/// background.
unsafe fn blit_surface(dc: HDC, rect: RECT, pixels: &[u32], draw: impl FnOnce(HDC)) {
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let memory = CreateCompatibleDC(dc);
    let mut info: BITMAPINFO = zeroed();
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut bits: *mut c_void = null_mut();
    let bitmap = CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if memory.is_null() || bitmap.is_null() || bits.is_null() {
        if !bitmap.is_null() {
            DeleteObject(bitmap as HGDIOBJ);
        }
        if !memory.is_null() {
            DeleteDC(memory);
        }
        return;
    }
    // COLORREF is 0x00BBGGRR, a DIB pixel 0x00RRGGBB.
    let target = std::slice::from_raw_parts_mut(bits as *mut u32, pixels.len());
    for (out, colour) in target.iter_mut().zip(pixels) {
        *out = ((colour & 0xff) << 16) | (colour & 0xff00) | ((colour >> 16) & 0xff);
    }
    let previous_bitmap = SelectObject(memory, bitmap as HGDIOBJ);
    let previous_font = SelectObject(memory, GetStockObject(DEFAULT_GUI_FONT));
    SetBkMode(memory, TRANSPARENT as i32);
    draw(memory);
    BitBlt(
        dc, rect.left, rect.top, width, height, memory, 0, 0, SRCCOPY,
    );
    SelectObject(memory, previous_font);
    SelectObject(memory, previous_bitmap);
    DeleteObject(bitmap as HGDIOBJ);
    DeleteDC(memory);
}

/// A button's icon and label, centred in its `width` x `height` surface.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_button_content(
    memory: HDC,
    width: i32,
    height: i32,
    dpi: u32,
    font: HFONT,
    text: u32,
    label: &[u16],
    icon: Option<(HFONT, u16)>,
    underline: bool,
) {
    SetTextColor(memory, text);
    SelectObject(memory, font as HGDIOBJ);
    let icon_w = if icon.is_some() { scale(16, dpi) } else { 0 };
    let text_w = if label.is_empty() {
        0
    } else {
        label_width(memory, &String::from_utf16_lossy(label))
    };
    let gap = if icon.is_some() && !label.is_empty() {
        scale(BUTTON_GAP, dpi)
    } else {
        0
    };
    let content = icon_w + gap + text_w;
    let mut x = (width - content) / 2;
    if let Some((icon_font, code)) = icon {
        SelectObject(memory, icon_font as HGDIOBJ);
        let mut glyph = [code];
        let mut area = RECT {
            left: x,
            top: 0,
            right: x + icon_w,
            bottom: height,
        };
        DrawTextW(
            memory,
            glyph.as_mut_ptr(),
            1,
            &mut area,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(memory, font as HGDIOBJ);
        x += icon_w + gap;
    }
    if !label.is_empty() {
        let mut text_wide = label.to_vec();
        let mut area = RECT {
            left: x,
            top: 0,
            right: width,
            bottom: height,
        };
        DrawTextW(
            memory,
            text_wide.as_mut_ptr(),
            text_wide.len() as i32,
            &mut area,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | if underline { 0 } else { DT_HIDEPREFIX },
        );
    }
}

/// The WinUI CheckBox's box and the space between it and the label, in epx.
const CHECK_BOX: i32 = 20;
const CHECK_GAP: i32 = 8;

/// Gives a new check box room for its box and focus ring. A window that is
/// already shown can paint its controls before the layout sizes them.
pub(super) unsafe fn hold_check_box(control: HWND) {
    let side = scale(
        CHECK_BOX + 2 * FOCUS_MARGIN,
        GetDpiForWindow(control).max(96),
    );
    SetWindowPos(
        control,
        null_mut(),
        0,
        0,
        side,
        side,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}
/// Where a check box's text starts, from the left of its face, in pixels:
/// the sum `paint_check_box` takes.
pub(super) fn check_text_inset(dpi: u32) -> i32 {
    scale(CHECK_BOX, dpi) + scale(CHECK_GAP, dpi)
}

/// Whether `control` is a check box button.
pub(super) unsafe fn is_check_box(control: HWND) -> bool {
    GetWindowLongW(control, GWL_STYLE) as u32 & BS_TYPEMASK as u32 == BS_AUTOCHECKBOX as u32
}

/// Places a Modern dialog control so that its face is `face`: an edit inside
/// its painted frame, a list box inside its 1 px frame with `ROW_HEIGHT`
/// entries, a drop-down list box and a push button FOCUS_MARGIN past the face
/// on every side for the focus ring, a check box that far past it on the left
/// and right, anything else on the face itself.
pub(super) unsafe fn place_modern(control: HWND, show: bool, face: RECT, dpi: u32) {
    if control.is_null() {
        return;
    }
    ShowWindow(control, if show { SW_SHOW } else { SW_HIDE });
    let ring = scale(FOCUS_MARGIN, dpi);
    let class = window_class(control).to_ascii_lowercase();
    let rect = match class.as_str() {
        // A read-only text box `prepare_modern_form` flattened is text on the
        // card, not a field: it fills its face.
        "edit"
            if GetWindowLongW(control, GWL_STYLE) as u32 & (ES_READONLY as u32 | WS_BORDER)
                == ES_READONLY as u32 =>
        {
            MoveWindow(
                control,
                face.left,
                face.top,
                face.right - face.left,
                face.bottom - face.top,
                1,
            );
            if is_multiline(control) {
                sync_edit_scrollbar(control);
            }
            return;
        }
        "edit" => return place_in_frame(control, face, dpi),
        "listbox" => {
            SendMessageW(
                control,
                LB_SETITEMHEIGHT,
                0,
                scale(ROW_HEIGHT, dpi) as isize,
            );
            let inset = scale(LIST_INSET, dpi);
            inflate(face, -inset, -inset)
        }
        "combobox" => return place_combo(control, inflate(face, ring, ring), dpi),
        "button" if is_check_box(control) => inflate(face, ring, 0),
        "button" => inflate(face, ring, ring),
        _ => face,
    };
    MoveWindow(
        control,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        1,
    );
}

/// A WinUI check box's box fill, stroke and mark for its state on the
/// content card.
fn check_box_colours(
    palette: &MainPalette,
    dark: bool,
    checked: bool,
    look: ButtonState,
) -> (u32, u32, u32) {
    let card = palette.card;
    let alt = |light: u32, dark_value: u32| over(if dark { dark_value } else { light }, card);
    // ControlStrongStrokeColorDisabled, also AccentFillColorDisabled.
    let muted = alt(0x37000000, 0x28ffffff);
    // ControlAltFillColor and ControlStrongStrokeColor while clear,
    // AccentFillColor and TextOnAccentFillColor while checked.
    match (checked, look.disabled, look.pressed, look.hot) {
        (true, true, _, _) => (
            muted,
            muted,
            if dark {
                over(0x87ffffff, muted)
            } else {
                0xffffff
            },
        ),
        (true, _, true, _) => (
            palette.accent_pressed,
            palette.accent_pressed,
            palette.on_accent_pressed,
        ),
        (true, _, _, true) => (
            palette.accent_hover,
            palette.accent_hover,
            palette.on_accent,
        ),
        (true, _, _, _) => (palette.accent, palette.accent, palette.on_accent),
        (false, true, _, _) => (card, muted, card),
        (false, _, true, _) => (alt(0x18000000, 0x12ffffff), muted, card),
        (false, _, _, true) => (alt(0x0f000000, 0x0bffffff), palette.strong_stroke, card),
        _ => (alt(0x06000000, 0x19000000), palette.strong_stroke, card),
    }
}

/// Paints a check box on the content card as a WinUI CheckBox: the 20 epx
/// box, filled with the accent and a check mark while checked, the label
/// 8 epx after it (wrapped for a multiline box), and the focus ring around
/// both. The box starts FOCUS_MARGIN in from the left.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_check_box(
    dc: HDC,
    rect: RECT,
    dpi: u32,
    palette: &MainPalette,
    dark: bool,
    font: HFONT,
    control: HWND,
    look: ButtonState,
) {
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let ring = scale(FOCUS_MARGIN, dpi);
    let card = palette.card;
    let mut pixels = paint_button_surface(
        width as usize,
        height as usize,
        ring as f32,
        &ButtonSurface {
            background: card,
            fill: card,
            stroke: card,
            stroke_bottom: card,
            radius: scale(CORNER, dpi) as f32,
            focus: look
                .focused
                .then_some((palette.focus_outer, palette.focus_inner)),
        },
    );
    let checked = SendMessageW(control, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED;
    let (fill, stroke, mark) = check_box_colours(palette, dark, checked, look);
    let size = scale(CHECK_BOX, dpi).min(height);
    let (box_x, box_y) = (ring, (height - size) / 2);
    // The pixels are not mirrored with a right-to-left DC, while the mark and
    // the label drawn through it are: the box goes where the mark lands.
    let pixel_x = if GetLayout(dc) & LAYOUT_RTL != 0 {
        width - ring - size
    } else {
        box_x
    };
    let face = paint_button_surface(
        size as usize,
        size as usize,
        0.0,
        &ButtonSurface {
            background: card,
            fill,
            stroke,
            stroke_bottom: stroke,
            radius: scale(CORNER, dpi) as f32,
            focus: None,
        },
    );
    for y in 0..size {
        for x in 0..size {
            let (tx, ty) = (pixel_x + x, box_y + y);
            if (0..width).contains(&tx) && ty < height {
                pixels[(ty * width + tx) as usize] = face[(y * size + x) as usize];
            }
        }
    }
    let mut label: Vec<u16> = super::text(control).encode_utf16().collect();
    let multiline = GetWindowLongW(control, GWL_STYLE) as u32 & BS_MULTILINE as u32 != 0;
    let prefix = if look.underline { 0 } else { DT_HIDEPREFIX };
    blit_surface(dc, rect, &pixels, |memory| {
        if checked {
            let glyphs = icon_font_px(dpi, 12);
            if !glyphs.is_null() {
                SelectObject(memory, glyphs as HGDIOBJ);
                SetTextColor(memory, mark);
                let mut area = RECT {
                    left: box_x,
                    top: box_y,
                    right: box_x + size,
                    bottom: box_y + size,
                };
                DrawTextW(
                    memory,
                    &0xe73eu16,
                    1,
                    &mut area,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
                SelectObject(memory, font as HGDIOBJ);
                DeleteObject(glyphs as HGDIOBJ);
            }
        }
        SelectObject(memory, font as HGDIOBJ);
        SetTextColor(
            memory,
            if look.disabled {
                palette.disabled
            } else {
                palette.text
            },
        );
        let mut area = RECT {
            left: box_x + size + scale(CHECK_GAP, dpi),
            top: 0,
            right: width - ring,
            bottom: height,
        };
        if multiline {
            let mut measured = area;
            draw_text(
                memory,
                &mut label,
                &mut measured,
                DT_LEFT | DT_WORDBREAK | DT_CALCRECT | prefix,
            );
            area.top = ((height - (measured.bottom - measured.top)) / 2).max(0);
            draw_text(
                memory,
                &mut label,
                &mut area,
                DT_LEFT | DT_WORDBREAK | prefix,
            );
        } else {
            draw_text(
                memory,
                &mut label,
                &mut area,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | prefix,
            );
        }
    });
}

/// One icon font glyph as a `size` x `size` 32-bit premultiplied ARGB bitmap
/// in `colour`: the glyph is drawn white on black with greyscale smoothing and
/// its brightness becomes the alpha. Null when the bitmap cannot be made.
pub(super) unsafe fn glyph_bitmap(
    memory: HDC,
    font: HFONT,
    size: i32,
    code: u16,
    colour: u32,
) -> HBITMAP {
    let (red, green, blue) = (colour & 0xff, (colour >> 8) & 0xff, (colour >> 16) & 0xff);
    let mut info: BITMAPINFO = zeroed();
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = size;
    info.bmiHeader.biHeight = -size;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut bits: *mut c_void = null_mut();
    let bitmap = CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bitmap.is_null() || bits.is_null() {
        return null_mut();
    }
    let previous_bitmap = SelectObject(memory, bitmap as HGDIOBJ);
    let previous_font = SelectObject(memory, font as HGDIOBJ);
    let mut area = RECT {
        left: 0,
        top: 0,
        right: size,
        bottom: size,
    };
    fill_colour(memory, &area, 0);
    SetBkMode(memory, TRANSPARENT as i32);
    SetTextColor(memory, 0xffffff);
    DrawTextW(
        memory,
        &code,
        1,
        &mut area,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    GdiFlush();
    SelectObject(memory, previous_font);
    SelectObject(memory, previous_bitmap);
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
    for pixel in pixels.iter_mut() {
        let alpha = (*pixel >> 8) & 0xff;
        let premultiply = |channel: u32| channel * alpha / 255;
        *pixel = alpha << 24 | premultiply(red) << 16 | premultiply(green) << 8 | premultiply(blue);
    }
    bitmap
}

/// Draws a 16 epx glyph bitmap (`glyph_bitmap`) with its alpha into `dc`,
/// its top left corner at (`x`, `y`).
pub(super) unsafe fn draw_glyph(dc: HDC, x: i32, y: i32, dpi: u32, code: u16, colour: u32) {
    let size = scale(16, dpi);
    let font = icon_font(dpi);
    if font.is_null() {
        return;
    }
    let memory = CreateCompatibleDC(dc);
    let bitmap = glyph_bitmap(memory, font, size, code, colour);
    if !bitmap.is_null() {
        let previous = SelectObject(memory, bitmap as HGDIOBJ);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        GdiAlphaBlend(dc, x, y, size, size, memory, 0, 0, size, size, blend);
        SelectObject(memory, previous);
        DeleteObject(bitmap as HGDIOBJ);
    }
    DeleteDC(memory);
    DeleteObject(font as HGDIOBJ);
}

/// Glyphs of the filter entries, in `MAIN_FILTERS` order: List, Download,
/// Completed, Error, Video, Audio, Document, ZipFolder, AppIconDefault.
pub(super) const FILTER_GLYPHS: [u16; 9] = [
    0xe8fd, 0xe896, 0xe930, 0xe783, 0xe714, 0xe8d6, 0xe8a5, 0xf012, 0xecaa,
];

/// An entry of the open filter list, in epx.
pub(super) const FILTER_ITEM: i32 = 32;

/// Space from a filter box's window edge to its glyph: the focus margin and
/// the field's own padding, so the open list lines up with the closed box.
unsafe fn filter_glyph_x(dpi: u32) -> i32 {
    scale(FOCUS_MARGIN + INPUT_PAD_LEFT, dpi)
}

/// Draws one entry of the filter box's open list: the flyout surface, a
/// rounded plate and the accent pill under the selected entry, the entry's
/// glyph and its text. The open list moves its selection with the pointer,
/// and only the entry's own state says whether it is selected: the box's
/// current selection still names the entry being left while that entry is
/// repainted. The closed box paints itself (`combo_proc`), so a request for
/// it only asks for that paint; high contrast draws both with system colours.
pub(super) unsafe fn draw_filter_item(ui: &MainUi, item: &DRAWITEMSTRUCT) {
    let dc = item.hDC;
    let field = item.itemState & ODS_COMBOBOXEDIT != 0;
    let contrast = high_contrast();
    if field && !contrast {
        InvalidateRect(item.hwndItem, null(), 0);
        return;
    }
    let palette = main_palette(ui.hwnd);
    let hot = item.itemState & ODS_SELECTED != 0;
    let index = item.itemID as i32;
    let rect = item.rcItem;
    let text_colour = if contrast {
        let (back, fore) = if hot {
            (COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT)
        } else {
            (COLOR_WINDOW, COLOR_WINDOWTEXT)
        };
        fill_colour(dc, &rect, GetSysColor(back));
        GetSysColor(fore)
    } else {
        fill_colour(dc, &rect, palette.flyout);
        if hot {
            let plate = RECT {
                left: rect.left + scale(4, ui.dpi),
                top: rect.top + scale(2, ui.dpi),
                right: rect.right - scale(4, ui.dpi),
                bottom: rect.bottom - scale(2, ui.dpi),
            };
            // SubtleFillColorSecondary on the flyout.
            let subtle = if scheme_dark(ui.hwnd) {
                0x0fffffff
            } else {
                0x09000000
            };
            round_rect(
                dc,
                &plate,
                scale(CORNER, ui.dpi),
                over(subtle, palette.flyout),
                None,
            );
            let height = scale(16, ui.dpi).min(plate.bottom - plate.top);
            let top = plate.top + (plate.bottom - plate.top - height) / 2;
            let pill = RECT {
                left: plate.left,
                top,
                right: plate.left + scale(3, ui.dpi),
                bottom: top + height,
            };
            round_rect(dc, &pill, scale(2, ui.dpi), palette.accent, None);
        }
        palette.text
    };
    if index < 0 {
        return;
    }
    // The list's own border moves its items one pixel in from the box's edge.
    let glyph_x = rect.left + filter_glyph_x(ui.dpi) - if field { 0 } else { 1 };
    let size = scale(16, ui.dpi);
    if let Some(&code) = FILTER_GLYPHS.get(index as usize) {
        draw_glyph(
            dc,
            glyph_x,
            (rect.top + rect.bottom - size) / 2,
            ui.dpi,
            code,
            text_colour,
        );
    }
    let mut label: Vec<u16> = combo_text(item.hwndItem, index).encode_utf16().collect();
    let mut area = RECT {
        left: glyph_x + size + scale(8, ui.dpi),
        ..rect
    };
    let previous = SelectObject(dc, ui.font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, text_colour);
    draw_text(
        dc,
        &mut label,
        &mut area,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    SelectObject(dc, previous);
}

/// The text of one entry of a combo box.
unsafe fn combo_text(combo: HWND, index: i32) -> String {
    let length = SendMessageW(combo, CB_GETLBTEXTLEN, index as usize, 0);
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    SendMessageW(
        combo,
        CB_GETLBTEXT,
        index as usize,
        buffer.as_mut_ptr() as isize,
    );
    String::from_utf16_lossy(&buffer[..length as usize])
}

/// Gives each item of `menu` whose command has a glyph in `glyphs` a 16 epx
/// icon font glyph as its bitmap (`glyph_bitmap`), in the menu's text colour.
/// Menus draw a 32-bit premultiplied ARGB bitmap with its alpha. Returns the
/// bitmaps, which the caller deletes once the menu closed.
pub(super) unsafe fn set_menu_glyphs(
    menu: HMENU,
    hwnd: HWND,
    dpi: u32,
    glyphs: &[(i32, u16)],
) -> Vec<HBITMAP> {
    let size = scale(16, dpi);
    let font = icon_font(dpi);
    if font.is_null() {
        return Vec::new();
    }
    let colour = if high_contrast() {
        GetSysColor(COLOR_MENUTEXT)
    } else {
        main_palette(hwnd).text
    };
    let memory = CreateCompatibleDC(null_mut());
    let mut bitmaps = Vec::new();
    for index in 0..GetMenuItemCount(menu).max(0) {
        let id = GetMenuItemID(menu, index) as i32;
        let Some(&(_, code)) = glyphs.iter().find(|(command, _)| *command == id) else {
            continue;
        };
        let bitmap = glyph_bitmap(memory, font, size, code, colour);
        if bitmap.is_null() {
            continue;
        }
        let item = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            hbmpItem: bitmap,
            ..zeroed()
        };
        SetMenuItemInfoW(menu, index as u32, 1, &item);
        bitmaps.push(bitmap);
    }
    DeleteDC(memory);
    DeleteObject(font as HGDIOBJ);
    bitmaps
}

/// The New download dialog, in epx: the content card's padding, a label above
/// each field, the 32 epx fields and the command bar below the card.
pub(super) const ADD_PAD: i32 = 24;
pub(super) const ADD_TOP: i32 = 20;
pub(super) const ADD_LABEL: i32 = 20;
pub(super) const ADD_LABEL_GAP: i32 = 8;
pub(super) const ADD_URLS: i32 = 64;
pub(super) const ADD_FIELD: i32 = 32;
pub(super) const ADD_GROUP_GAP: i32 = 12;
pub(super) const ADD_FOOTER: i32 = 80;
/// The dialog's content height without and with the advanced rows.
pub(super) const ADD_HEIGHT: i32 = 376;
pub(super) const ADD_ADVANCED_HEIGHT: i32 = 592;
/// A one-field dialog (speed limit, rename) in the same rhythm: its label's
/// lines, the field, the error line and the command bar.
pub(super) const fn one_field_height(label_lines: i32) -> i32 {
    ADD_TOP
        + ADD_LABEL * label_lines
        + ADD_LABEL_GAP
        + ADD_FIELD
        + ADD_GROUP_GAP
        + ADD_LABEL
        + ADD_FOOTER
}
/// The list and form dialogs' content sizes: the queue manager, and the rule,
/// scan, sync, login and log dialogs. A form window fits the work area of a
/// 1366×768 screen at 125 % and of a 1080p screen at 175 %; the queue manager
/// scrolls there.
pub(super) const QUEUES_WIDTH: i32 = 860;
pub(super) const QUEUES_HEIGHT: i32 = 640;
pub(super) const FORM_WIDTH: i32 = 800;
pub(super) const FORM_HEIGHT: i32 = 528;
/// The media picker and the job view it turns into: their width, the content
/// height the picker opens with before it fits its rows, its summary's height
/// and the job view's content height.
pub(super) const MEDIA_WIDTH: i32 = 720;
pub(super) const MEDIA_HEIGHT: i32 = 416;
pub(super) const MEDIA_SUMMARY: i32 = 64;
pub(super) const PROGRESS_HEIGHT: i32 = 472;
/// Space between an input frame and the text field inside it: left, right
/// (a multiline field keeps its scroll bar inside the frame), and top for a
/// multiline field; a single line is centred.
const INPUT_PAD_LEFT: i32 = 11;
const INPUT_PAD_RIGHT: i32 = 8;
const INPUT_PAD_TOP: i32 = 6;

/// Readies a dialog that has its Modern layout: its text fields lose their
/// sunken border (the dialog paints their frames) and its field labels their
/// trailing colon, as Fluent writes them.
pub(super) unsafe fn prepare_modern_form(dlg: &DialogUi) {
    let Some(form) = modern_form(&dlg.kind) else {
        return;
    };
    for edit in form.inputs.into_iter().chain(form.texts) {
        flatten_edit(edit);
    }
    strip_label_colons(dlg.hwnd, dlg.error);
}

/// Drops the trailing colon from the labels of `parent`, all but its error line.
unsafe fn strip_label_colons(parent: HWND, error: HWND) {
    for label in child_statics(parent, error) {
        let value = super::text(label);
        let bare = field_label(&value);
        if bare != value {
            set_text(label, bare);
        }
    }
}

unsafe fn flatten_edit(edit: HWND) {
    let style = GetWindowLongPtrW(edit, GWL_STYLE) & !(WS_BORDER as isize);
    SetWindowLongPtrW(edit, GWL_STYLE, style);
    let extended = GetWindowLongPtrW(edit, GWL_EXSTYLE) & !(WS_EX_CLIENTEDGE as isize);
    SetWindowLongPtrW(edit, GWL_EXSTYLE, extended);
    SetWindowPos(
        edit,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// A Modern dialog's list box: no border of its own (the dialog paints its
/// frame) and entries it paints itself, `ROW_HEIGHT` tall; `style` adds the
/// selection and notification styles of the classic list it stands for.
pub(super) unsafe fn modern_list(parent: HWND, font: HFONT, id: i32, style: u32) -> HWND {
    let list = control(
        parent,
        "LISTBOX",
        "",
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_VSCROLL
            | LBS_NOINTEGRALHEIGHT as u32
            | LBS_OWNERDRAWFIXED as u32
            | LBS_HASSTRINGS as u32
            | style,
        0,
        id,
    );
    apply_font(list, font);
    list
}

/// A Modern dialog's progress bar, painted as the Windows 11 ProgressBar on
/// the card: a 1 epx track with a 3 epx rounded accent bar, sweeping while the
/// length is unknown. High contrast keeps the system's bar.
pub(super) unsafe extern "system" fn modern_progress_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if !high_contrast() {
        match msg {
            WM_PAINT => {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut paint);
                paint_modern_progress(hwnd, dc);
                EndPaint(hwnd, &paint);
                return 0;
            }
            WM_PRINTCLIENT => {
                paint_modern_progress(hwnd, wparam as HDC);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            _ => {}
        }
    }
    call_previous(hwnd, msg, wparam, lparam)
}

unsafe fn paint_modern_progress(hwnd: HWND, dc: HDC) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client) == 0 {
        return;
    }
    let palette = main_palette(GetParent(hwnd));
    let dpi = GetDpiForWindow(hwnd).max(96);
    fill_colour(dc, &client, palette.card);
    let mut range = PBRANGE {
        iLow: 0,
        iHigh: 100,
    };
    SendMessageW(hwnd, PBM_GETRANGE, 1, &mut range as *mut _ as isize);
    let position = SendMessageW(hwnd, PBM_GETPOS, 0, 0) as i32;
    let span = (range.iHigh - range.iLow).max(1);
    let width = client.right - client.left;
    let (start, end) = if GetWindowLongW(hwnd, GWL_STYLE) as u32 & PBS_MARQUEE != 0 {
        processing_sweep(sweep_clock(), width)
    } else {
        (
            0,
            ((position - range.iLow).clamp(0, span) as i64 * width as i64 / span as i64) as i32,
        )
    };
    let thickness = scale(3, dpi).max(3);
    let middle = (client.top + client.bottom) / 2;
    let line = scale(1, dpi).max(1);
    fill_colour(
        dc,
        &RECT {
            left: client.left,
            top: middle - line / 2,
            right: client.right,
            bottom: middle - line / 2 + line,
        },
        palette.strong_stroke,
    );
    if end > start {
        let bar = RECT {
            left: client.left + start,
            top: middle - thickness / 2,
            right: client.left + end,
            bottom: middle - thickness / 2 + thickness,
        };
        round_rect(dc, &bar, thickness / 2, palette.accent, None);
    }
}

/// The frame around a list box placed by `place_modern`.
unsafe fn list_frame(parent: HWND, list: HWND, dpi: u32) -> RECT {
    let inset = scale(LIST_INSET, dpi);
    inflate(child_rect(parent, list), inset, inset)
}

/// One entry of a Modern dialog's list box, painted like a job list row: the
/// card, the selected entry's tinted backplate with the accent pill, the focus
/// ring on the focused entry, and the text 12 epx in.
unsafe fn paint_list_item(item: &DRAWITEMSTRUCT, dpi: u32, palette: &MainPalette, font: HFONT) {
    let dc = item.hDC;
    let rect = item.rcItem;
    let selected = item.itemState & ODS_SELECTED != 0;
    let focused = item.itemState & ODS_FOCUS != 0
        && SendMessageW(item.hwndItem, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0;
    let contrast = high_contrast();
    let text_colour = if contrast {
        let (back, fore) = if selected {
            (COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT)
        } else {
            (COLOR_WINDOW, COLOR_WINDOWTEXT)
        };
        fill_colour(dc, &rect, GetSysColor(back));
        GetSysColor(fore)
    } else {
        fill_colour(dc, &rect, palette.card);
        if selected || focused {
            let fill = if selected {
                palette.row_selected
            } else {
                palette.card
            };
            paint_row_plate(dc, rect, dpi, palette, fill, focused, selected);
        }
        palette.text
    };
    if item.itemID == u32::MAX {
        return;
    }
    let length = SendMessageW(item.hwndItem, LB_GETTEXTLEN, item.itemID as usize, 0);
    if length > 0 {
        let mut entry = vec![0u16; length as usize + 1];
        SendMessageW(
            item.hwndItem,
            LB_GETTEXT,
            item.itemID as usize,
            entry.as_mut_ptr() as isize,
        );
        let mut area = RECT {
            left: rect.left + scale(12, dpi),
            right: rect.right - scale(8, dpi),
            ..rect
        };
        let previous = SelectObject(dc, font as HGDIOBJ);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, text_colour);
        DrawTextW(
            dc,
            entry.as_mut_ptr(),
            length as i32,
            &mut area,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(dc, previous);
    }
    if contrast && focused {
        DrawFocusRect(dc, &rect);
    }
}

fn is_multiline(edit: HWND) -> bool {
    unsafe { GetWindowLongPtrW(edit, GWL_STYLE) as u32 & ES_MULTILINE as u32 != 0 }
}

/// Puts an edit control inside the input frame `frame`: a single line is
/// centred on the frame's height, and a multiline field shows its scroll bar
/// only while its text overflows.
pub(super) unsafe fn place_in_frame(edit: HWND, frame: RECT, dpi: u32) {
    let left = frame.left + scale(INPUT_PAD_LEFT, dpi);
    if is_multiline(edit) {
        let top = frame.top + scale(INPUT_PAD_TOP, dpi);
        MoveWindow(
            edit,
            left,
            top,
            frame.right - scale(1, dpi) - left,
            frame.bottom - scale(INPUT_PAD_TOP, dpi) - top,
            1,
        );
        sync_edit_scrollbar(edit);
    } else {
        let font = SendMessageW(edit, WM_GETFONT, 0, 0) as HFONT;
        let height = line_height(edit, font).max(1);
        MoveWindow(
            edit,
            left,
            (frame.top + frame.bottom - height) / 2,
            frame.right - scale(INPUT_PAD_RIGHT, dpi) - left,
            height,
            1,
        );
    }
}

/// The input frame around an edit control placed by `place_in_frame`.
unsafe fn input_frame(parent: HWND, edit: HWND, dpi: u32) -> RECT {
    let inner = child_rect(parent, edit);
    let left = inner.left - scale(INPUT_PAD_LEFT, dpi);
    if is_multiline(edit) {
        RECT {
            left,
            top: inner.top - scale(INPUT_PAD_TOP, dpi),
            right: inner.right + scale(1, dpi),
            bottom: inner.bottom + scale(INPUT_PAD_TOP, dpi),
        }
    } else {
        let middle = (inner.top + inner.bottom) / 2;
        let half = scale(ADD_FIELD, dpi) / 2;
        RECT {
            left,
            top: middle - half,
            right: inner.right + scale(INPUT_PAD_RIGHT, dpi),
            bottom: middle + half,
        }
    }
}

fn add_inputs(f: &AddFields) -> [HWND; 6] {
    [
        f.urls,
        f.dir,
        f.filename,
        f.connections,
        f.checksum,
        f.schedule,
    ]
}

pub(super) fn settings_inputs(f: &SettingsFields) -> [HWND; 16] {
    [
        f.dir,
        f.max_active,
        f.connections,
        f.media_fragments,
        f.per_host,
        f.speed,
        f.retry,
        f.start,
        f.end,
        f.proxy_url,
        f.proxy_user,
        f.proxy_pass,
        f.speed_start,
        f.speed_end,
        f.speed_kib,
        f.history,
    ]
}

pub(super) fn settings_combos(f: &SettingsFields) -> [HWND; 6] {
    [
        f.ui_mode,
        f.ui_language,
        f.log_level,
        f.proxy_mode,
        f.double_click,
        f.theme,
    ]
}

/// The text fields of the list and form dialogs.
pub(super) fn queue_inputs(f: &QueueFields) -> [HWND; 7] {
    [
        f.name,
        f.concurrency,
        f.windows,
        f.quota,
        f.program,
        f.arguments,
        f.delay,
    ]
}

pub(super) fn rule_inputs(f: &RuleFields) -> [HWND; 3] {
    [f.host, f.dir, f.priority]
}

pub(super) fn crawler_inputs(f: &CrawlerFields) -> [HWND; 4] {
    [f.url, f.depth, f.pages, f.candidates]
}

pub(super) fn sync_inputs(f: &SyncFields) -> [HWND; 3] {
    [f.url, f.dir, f.interval]
}

pub(super) fn login_inputs(f: &LoginFields) -> [HWND; 3] {
    [f.host, f.user, f.pass]
}

/// What a Modern dialog paints itself: its framed inputs and list boxes, the
/// read-only text boxes it shows as plain text on the card, its
/// WinUI buttons with the primary one (0 for none), the buttons that sit on the content
/// card rather than on the command bar, and a static shown as a secondary hint.
struct ModernForm {
    inputs: Vec<HWND>,
    lists: Vec<HWND>,
    texts: Vec<HWND>,
    buttons: &'static [i32],
    primary: i32,
    card_buttons: &'static [i32],
    hint: HWND,
}

unsafe fn modern_form(kind: &DialogKind) -> Option<ModernForm> {
    match kind {
        DialogKind::Add(f) => Some(ModernForm {
            inputs: add_inputs(f).to_vec(),
            lists: vec![],
            texts: Vec::new(),
            buttons: &[D_ADD_OK, D_ADVANCED, D_CANCEL, D_BROWSE],
            primary: D_ADD_OK,
            card_buttons: &[D_BROWSE],
            hint: null_mut(),
        }),
        DialogKind::Settings(f) => Some(ModernForm {
            inputs: settings_inputs(f).to_vec(),
            lists: vec![],
            texts: Vec::new(),
            buttons: &[
                S_OK,
                D_CANCEL,
                S_BROWSE,
                ID_SETTINGS_REOPEN_WIZARD,
                ID_SETTINGS_CHECK_UPDATES,
                ID_SETTINGS_INSTALL_EXTENSION,
            ],
            primary: S_OK,
            card_buttons: &[
                S_BROWSE,
                ID_SETTINGS_CHECK_UPDATES,
                ID_SETTINGS_INSTALL_EXTENSION,
            ],
            hint: f.theme_note,
        }),
        DialogKind::Speed(f) => Some(ModernForm {
            inputs: vec![f.edit],
            lists: vec![],
            texts: Vec::new(),
            buttons: &[SP_OK, D_CANCEL],
            primary: SP_OK,
            card_buttons: &[],
            hint: null_mut(),
        }),
        DialogKind::Rename(f) => Some(ModernForm {
            inputs: vec![f.edit],
            lists: vec![],
            texts: Vec::new(),
            buttons: &[RN_OK, D_CANCEL],
            primary: RN_OK,
            card_buttons: &[],
            hint: null_mut(),
        }),
        DialogKind::Queues(f) => Some(ModernForm {
            inputs: queue_inputs(f).to_vec(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[
                Q_CREATE,
                Q_RENAME,
                Q_DELETE,
                Q_UPDATE,
                Q_MOVE_JOB,
                Q_CANCEL_COMPLETION,
                D_CANCEL,
            ],
            primary: Q_UPDATE,
            card_buttons: &[
                Q_CREATE,
                Q_RENAME,
                Q_DELETE,
                Q_UPDATE,
                Q_MOVE_JOB,
                Q_CANCEL_COMPLETION,
            ],
            hint: f.countdown,
        }),
        DialogKind::Rules(f) => Some(ModernForm {
            inputs: rule_inputs(f).to_vec(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[R_NEW, R_SAVE, R_REMOVE, D_CANCEL],
            primary: R_SAVE,
            card_buttons: &[R_NEW, R_SAVE, R_REMOVE],
            hint: null_mut(),
        }),
        DialogKind::Crawler(f) => Some(ModernForm {
            inputs: crawler_inputs(f).to_vec(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[C_SCAN, C_ADD, D_CANCEL],
            primary: C_SCAN,
            card_buttons: &[C_SCAN],
            hint: null_mut(),
        }),
        DialogKind::Synchronization(f) => Some(ModernForm {
            inputs: sync_inputs(f).to_vec(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[Y_NEW, Y_SAVE, Y_REMOVE, Y_RUN, D_CANCEL],
            primary: Y_SAVE,
            card_buttons: &[Y_NEW, Y_SAVE, Y_REMOVE, Y_RUN],
            hint: null_mut(),
        }),
        DialogKind::Media(f) => Some(ModernForm {
            inputs: vec![
                f.name,
                f.external_url,
                f.external_language,
                f.external_label,
                f.playlist_items,
            ],
            lists: vec![f.audio_tracks, f.subtitles, f.external_list],
            texts: vec![f.summary],
            buttons: &[
                M_QUEUE,
                D_CANCEL,
                D_ADVANCED,
                M_REPORT,
                M_MP3,
                ID_MEDIA_EXTERNAL_ADD,
                ID_MEDIA_EXTERNAL_REMOVE,
                M_RENEW,
            ],
            primary: M_QUEUE,
            card_buttons: &[ID_MEDIA_EXTERNAL_ADD, ID_MEDIA_EXTERNAL_REMOVE, M_RENEW],
            hint: f.size,
        }),
        DialogKind::Progress(f) => Some(ModernForm {
            inputs: Vec::new(),
            lists: vec![f.list],
            texts: vec![f.status],
            buttons: &[
                P_ACTION,
                P_CANCEL,
                P_OPEN_FILE,
                P_OPEN_FOLDER,
                P_CLOSE,
                P_MINI,
                P_REPORT,
            ],
            primary: P_ACTION,
            card_buttons: &[P_MINI, P_REPORT],
            hint: f.hint,
        }),
        DialogKind::Events(f) => Some(ModernForm {
            inputs: Vec::new(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[E_OPEN, E_PACKAGE, E_CLOSE],
            primary: 0,
            card_buttons: &[E_OPEN, E_PACKAGE],
            hint: null_mut(),
        }),
        DialogKind::JobLog(f) => Some(ModernForm {
            inputs: Vec::new(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[J_COPY, J_OPEN, J_CLOSE],
            primary: 0,
            card_buttons: &[J_COPY, J_OPEN],
            hint: null_mut(),
        }),
        DialogKind::Logins(f) => Some(ModernForm {
            inputs: login_inputs(f).to_vec(),
            lists: vec![f.list],
            texts: Vec::new(),
            buttons: &[L_ADD, L_REMOVE, L_SAVE, D_CANCEL],
            primary: L_SAVE,
            card_buttons: &[L_ADD, L_REMOVE],
            hint: null_mut(),
        }),
        _ => None,
    }
}

/// The top of a Modern dialog's command bar. Settings scrolls, so its bar
/// starts where its Save button's row does.
unsafe fn footer_top(dlg: &DialogUi, client: &RECT) -> i32 {
    let save = GetDlgItem(dlg.hwnd, S_OK);
    if matches!(dlg.kind, DialogKind::Settings(_)) && !save.is_null() {
        child_rect(dlg.hwnd, save).top + scale(FOCUS_MARGIN, dlg.dpi) - scale(ADD_PAD, dlg.dpi)
    } else {
        client.bottom - scale(ADD_FOOTER, dlg.dpi)
    }
}

/// The Windows 11 surfaces and controls of Modern's converted dialogs: the
/// content card, the command bar with its top stroke, flat input frames with
/// an accent line under the focused field that take a click anywhere on them,
/// WinUI buttons with the primary action as the accent button, WinUI check
/// boxes, and the label and field colours. Every other dialog keeps its own
/// painting.
pub(super) unsafe fn modern_dialog_message(
    dlg: &DialogUi,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if !matches!(
        msg,
        WM_ERASEBKGND
            | WM_CTLCOLORSTATIC
            | WM_CTLCOLOREDIT
            | WM_COMMAND
            | WM_NOTIFY
            | WM_LBUTTONDOWN
            | WM_DRAWITEM
            | WM_CTLCOLORLISTBOX
    ) {
        return None;
    }
    let form = modern_form(&dlg.kind)?;
    let palette = main_palette(dlg.hwnd);
    match msg {
        // The frame's padding belongs to its field: a click there goes to the
        // field at the nearest point inside it.
        WM_LBUTTONDOWN => {
            let point = POINT {
                x: loword(lparam as usize) as i16 as i32,
                y: hiword(lparam as usize) as i16 as i32,
            };
            let field = form.inputs.iter().copied().find(|&control| {
                IsWindowVisible(control) != 0
                    && PtInRect(&input_frame(dlg.hwnd, control, dlg.dpi), point) != 0
            })?;
            let area = child_rect(dlg.hwnd, field);
            let x = point.x.clamp(area.left, area.right - 1) - area.left;
            let y = point.y.clamp(area.top, area.bottom - 1) - area.top;
            let at = ((y as u32 & 0xffff) << 16 | (x as u32 & 0xffff)) as isize;
            SetFocus(field);
            SendMessageW(field, WM_LBUTTONDOWN, wparam, at);
            SendMessageW(field, WM_LBUTTONUP, 0, at);
            Some(0)
        }
        WM_ERASEBKGND => {
            paint_modern_surfaces(dlg, &form, wparam as HDC, &palette);
            Some(1)
        }
        WM_DRAWITEM => {
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            if item.CtlType != ODT_LISTBOX || !form.lists.contains(&item.hwndItem) {
                return None;
            }
            paint_list_item(item, dlg.dpi, &palette, dlg.font);
            Some(1)
        }
        WM_CTLCOLORLISTBOX => {
            if !form.lists.contains(&(lparam as HWND)) {
                return None;
            }
            let dc = wparam as HDC;
            let fill = if high_contrast() {
                GetSysColor(COLOR_WINDOW)
            } else {
                palette.card
            };
            SetBkColor(dc, fill);
            SetDCBrushColor(dc, fill);
            Some(GetStockObject(DC_BRUSH) as LRESULT)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let dc = wparam as HDC;
            let (text, fill) = if msg == WM_CTLCOLOREDIT {
                let (fill, text) = input_colours(&card_palette(dlg.hwnd));
                (text, fill)
            } else if lparam as HWND == dlg.error {
                (palette.critical, palette.card)
            } else if lparam as HWND == form.hint && !form.hint.is_null() {
                (palette.secondary, palette.card)
            } else {
                (palette.text, palette.card)
            };
            SetTextColor(dc, text);
            SetBkColor(dc, fill);
            SetDCBrushColor(dc, fill);
            Some(GetStockObject(DC_BRUSH) as LRESULT)
        }
        WM_COMMAND => {
            let code = hiword(wparam) as u32;
            let control = lparam as HWND;
            if (code == EN_SETFOCUS || code == EN_KILLFOCUS) && form.inputs.contains(&control) {
                let frame = input_frame(dlg.hwnd, control, dlg.dpi);
                InvalidateRect(dlg.hwnd, &frame, 1);
            }
            if code == EN_CHANGE && form.inputs.contains(&control) && is_multiline(control) {
                sync_edit_scrollbar(control);
            }
            if let DialogKind::Add(f) = &dlg.kind {
                if (code == EN_CHANGE && control == f.urls)
                    || (code == CBN_SELCHANGE && control == f.kind)
                {
                    refresh_add_ok(dlg, f);
                }
            }
            None
        }
        WM_NOTIFY => {
            let draw = &*(lparam as *const NMCUSTOMDRAW);
            let id = draw.hdr.idFrom as i32;
            let button = draw.hdr.hwndFrom;
            let check_box = draw.hdr.code == NM_CUSTOMDRAW
                && window_class(button).eq_ignore_ascii_case("Button")
                && is_check_box(button);
            if draw.hdr.code != NM_CUSTOMDRAW || !(form.buttons.contains(&id) || check_box) {
                return None;
            }
            // High contrast keeps the system's check box.
            if check_box && high_contrast() {
                return None;
            }
            if draw.dwDrawStage != CDDS_PREPAINT {
                return Some(CDRF_DODEFAULT as LRESULT);
            }
            let state = draw.uItemState;
            let disabled = state & CDIS_DISABLED != 0;
            let look = ButtonState {
                primary: id == form.primary,
                disabled,
                pressed: !disabled && state & CDIS_SELECTED != 0,
                hot: !disabled && state & CDIS_HOT != 0,
                focused: state & CDIS_FOCUS != 0
                    && SendMessageW(button, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0,
                underline: state & CDIS_SHOWKEYBOARDCUES != 0,
            };
            if check_box {
                paint_check_box(
                    draw.hdc,
                    draw.rc,
                    dlg.dpi,
                    &palette,
                    scheme_dark(dlg.hwnd),
                    dlg.font,
                    button,
                    look,
                );
                return Some(CDRF_SKIPDEFAULT as LRESULT);
            }
            let label: Vec<u16> = super::text(button).encode_utf16().collect();
            // Advanced options shows whether its rows are open.
            let open = match &dlg.kind {
                DialogKind::Add(f) if id == D_ADVANCED => Some(f.advanced),
                DialogKind::Media(f) if id == D_ADVANCED => Some(f.advanced),
                _ => None,
            };
            let chevrons = if open.is_some() {
                icon_font_px(dlg.dpi, 12)
            } else {
                null_mut()
            };
            let icon = (!chevrons.is_null()).then_some((
                chevrons,
                if open == Some(true) {
                    0xe70eu16
                } else {
                    0xe70d
                },
            ));
            let surface = if form.card_buttons.contains(&id) {
                card_palette(dlg.hwnd)
            } else {
                palette
            };
            paint_button(
                draw.hdc, draw.rc, dlg.dpi, &surface, dlg.font, &label, icon, look,
            );
            if !chevrons.is_null() {
                DeleteObject(chevrons as HGDIOBJ);
            }
            Some(CDRF_SKIPDEFAULT as LRESULT)
        }
        _ => None,
    }
}

/// Names on New download's OK what it does with the links and type as they
/// are now. A multiline edit sends no EN_CHANGE for WM_SETTEXT, so a caller
/// that writes the links calls this itself.
pub(super) unsafe fn refresh_add_ok(dlg: &DialogUi, f: &AddFields) {
    let ok = GetDlgItem(dlg.hwnd, D_ADD_OK);
    let label = add_ok_label(f);
    if super::text(ok) != label {
        set_text(ok, label);
        layout_dialog(dlg);
        InvalidateRect(ok, null(), 0);
    }
}

unsafe fn paint_modern_surfaces(dlg: &DialogUi, form: &ModernForm, dc: HDC, palette: &MainPalette) {
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let footer_top = footer_top(dlg, &client);
    fill_colour(
        dc,
        &RECT {
            bottom: footer_top,
            ..client
        },
        palette.card,
    );
    fill_colour(
        dc,
        &RECT {
            top: footer_top,
            ..client
        },
        palette.base,
    );
    fill_colour(
        dc,
        &RECT {
            top: footer_top,
            bottom: footer_top + 1,
            ..client
        },
        palette.stroke,
    );
    let focus = GetFocus();
    let card = card_palette(dlg.hwnd);
    for &edit in &form.inputs {
        if edit.is_null() || IsWindowVisible(edit) == 0 {
            continue;
        }
        let frame = input_frame(dlg.hwnd, edit, dlg.dpi);
        paint_input_frame(dc, &frame, dlg.dpi, &card, focus == edit);
    }
    for &list in &form.lists {
        if list.is_null() || IsWindowVisible(list) == 0 {
            continue;
        }
        let frame = list_frame(dlg.hwnd, list, dlg.dpi);
        round_rect(
            dc,
            &frame,
            scale(CORNER, dlg.dpi),
            palette.card,
            Some(card.control_stroke),
        );
    }
}

/// Re-applies everything that follows the scheme or the accent: the caption
/// colour, the accent shades, the list colours and the subthemes of the list
/// and the tooltip.
pub(super) unsafe fn refresh_main_surfaces(ui: &mut MainUi) {
    read_accent_shades();
    let palette = main_palette(ui.hwnd);
    let dark = scheme_dark(ui.hwnd);
    let contrast = high_contrast();
    // DWMWA_CAPTION_COLOR; Windows 10 ignores it. High contrast restores the default.
    let caption: u32 = if contrast { 0xffff_ffff } else { palette.base };
    DwmSetWindowAttribute(
        ui.hwnd,
        35,
        &caption as *const _ as *const c_void,
        size_of::<u32>() as u32,
    );
    let dark_theme = wide("DarkMode_Explorer");
    if !ui.tooltip.is_null() {
        SetWindowTheme(
            ui.tooltip,
            if dark { dark_theme.as_ptr() } else { null() },
            null(),
        );
    }
    // The rows are painted here; the subtheme gives the scroll bars the scheme.
    let items = wide(if dark {
        "DarkMode_Explorer"
    } else {
        "Explorer"
    });
    SetWindowTheme(ui.list, items.as_ptr(), null());
    SendMessageW(ui.list, LVM_SETBKCOLOR, 0, palette.card as isize);
    // Transparent text keeps the stripes and backplates under it.
    let text_background = if contrast {
        palette.card as isize
    } else {
        CLR_NONE as isize
    };
    SendMessageW(ui.list, LVM_SETTEXTBKCOLOR, 0, text_background);
    SendMessageW(ui.list, LVM_SETTEXTCOLOR, 0, palette.text as isize);
    // The theme change re-measured the filter box from its font; the layout
    // brings it back to the command row's height.
    layout_main(ui);
    RedrawWindow(
        ui.hwnd,
        null(),
        null_mut(),
        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME,
    );
}

/// Fonts and sizes that follow the DPI and the language: the type ramp fonts,
/// the icon font, the row height and the status height.
pub(super) unsafe fn apply_main_metrics(ui: &mut MainUi) {
    let font = ramp_font(ui.dpi, BODY_PX, false);
    let details = ramp_font(ui.dpi, BODY_PX, false);
    let caption = ramp_font(ui.dpi, CAPTION_PX, true);
    let menu = make_font(ui.dpi, 9, FW_NORMAL as i32);
    let icons = icon_font(ui.dpi);
    for button in MAIN_BUTTONS {
        apply_font(GetDlgItem(ui.hwnd, button.0), font);
    }
    for h in [ui.search, ui.filter, ui.list, ui.tabs] {
        apply_font(h, font);
    }
    apply_font(ui.details, details);
    let header = SendMessageW(ui.list, LVM_GETHEADER, 0, 0) as HWND;
    apply_font(header, caption);
    apply_font(ui.status, caption);
    apply_font(ui.tooltip, caption);
    for old in [
        ui.font,
        ui.details_font,
        ui.caption_font,
        ui.menu_font,
        ui.icon_font,
    ] {
        if !old.is_null() {
            DeleteObject(old as HGDIOBJ);
        }
    }
    ui.font = font;
    ui.details_font = details;
    ui.caption_font = caption;
    ui.menu_font = menu;
    ui.icon_font = icons;
    // The bar measures its items with the menu font, so the items have to be
    // re-measured with the new font before the strip paints with it.
    let bar = GetMenu(ui.hwnd);
    if !bar.is_null() {
        SendMessageW(bar as HWND, WM_SETFONT, ui.menu_font as usize, 1);
    }
    // A row is as tall as the small image list, so an empty list 32 epx high
    // sets the row height; the list destroys the one it holds when it goes.
    let spacer = ImageList_Create(1, scale(32, ui.dpi), ILC_COLOR32, 0, 0);
    let previous = SendMessageW(ui.list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, spacer);
    if previous != 0 {
        ImageList_Destroy(previous);
    }
    size_selector_tabs(ui.tabs, ui.dpi);
    SendMessageW(
        ui.filter,
        CB_SETITEMHEIGHT,
        0,
        scale(FILTER_ITEM, ui.dpi) as isize,
    );
    SendMessageW(ui.status, SB_SETMINHEIGHT, scale(24, ui.dpi) as usize, 0);
    SendMessageW(ui.status, WM_SIZE, 0, 0);
    refresh_main_surfaces(ui);
}

/// Space on each side of a tab's label.
pub(super) const TAB_PAD: i32 = 12;
pub(super) const SELECTOR_TABS_HEIGHT: i32 = 36;

/// Every tab as wide as its label plus TAB_PAD on each side, 36 epx high.
pub(super) unsafe fn size_selector_tabs(tabs: HWND, dpi: u32) {
    let pad = scale(TAB_PAD, dpi);
    SendMessageW(tabs, TCM_SETPADDING, 0, (pad as u32 & 0xffff) as isize);
    // Without a fixed width only the height is taken; the width follows the label.
    let height = scale(SELECTOR_TABS_HEIGHT, dpi);
    SendMessageW(tabs, TCM_SETITEMSIZE, 0, ((height as u32) << 16) as isize);
    // comctl32's default minimum would widen a short label such as "Ağ".
    SendMessageW(tabs, TCM_SETMINTABWIDTH, 0, 0);
}

/// Shows the page scroll bar only while the text is taller than the page.
pub(super) unsafe fn sync_details_scrollbar(ui: &MainUi) {
    sync_edit_scrollbar(ui.details);
}

/// Shows a multiline edit's scroll bar only while its text is taller than
/// the edit.
pub(super) unsafe fn sync_edit_scrollbar(edit: HWND) {
    let lines = SendMessageW(edit, EM_GETLINECOUNT, 0, 0) as i32;
    let mut area: RECT = zeroed();
    SendMessageW(edit, EM_GETRECT, 0, &mut area as *mut _ as isize);
    let font = SendMessageW(edit, WM_GETFONT, 0, 0) as HFONT;
    let overflow = lines * line_height(edit, font) > area.bottom - area.top;
    ShowScrollBar(edit, SB_VERT, overflow as BOOL);
}

/// The height of one line of `font`.
pub(super) unsafe fn line_height(hwnd: HWND, font: HFONT) -> i32 {
    let dc = GetDC(hwnd);
    if dc.is_null() {
        return 0;
    }
    let previous = SelectObject(dc, font as HGDIOBJ);
    let mut metrics: TEXTMETRICW = zeroed();
    GetTextMetricsW(dc, &mut metrics);
    SelectObject(dc, previous);
    ReleaseDC(hwnd, dc);
    metrics.tmHeight
}

/// The list's own painting with Fluent rows: alternating card and stripe
/// rows, a rounded backplate under a hovered or selected row with an accent
/// mark for the selection, the cell texts and the progress bar. Every row is
/// painted here because the theme blends its own selection over anything
/// drawn under it; high contrast keeps the system's rows and adds the bar.
pub(super) unsafe fn modern_list_custom_draw(ui: &MainUi, lparam: LPARAM) -> LRESULT {
    let draw = &mut *(lparam as *mut NMLVCUSTOMDRAW);
    let stage = draw.nmcd.dwDrawStage;
    let contrast = high_contrast();
    if stage == CDDS_PREPAINT {
        if !contrast {
            modern_list_tail(ui, draw.nmcd.hdc);
        }
        return CDRF_NOTIFYITEMDRAW as LRESULT;
    }
    if stage == CDDS_ITEMPREPAINT {
        if contrast {
            return CDRF_NOTIFYSUBITEMDRAW as LRESULT;
        }
        paint_row_backplate(ui, draw);
        paint_row_cells(ui, draw.nmcd.hdc, draw.nmcd.dwItemSpec);
        return CDRF_SKIPDEFAULT as LRESULT;
    }
    let progress = ui.columns.get(draw.iSubItem as usize) == Some(&COL_PROGRESS);
    if stage == CDDS_ITEMPREPAINT | CDDS_SUBITEM {
        return if progress {
            CDRF_NOTIFYPOSTPAINT as LRESULT
        } else {
            CDRF_DODEFAULT as LRESULT
        };
    }
    if stage == CDDS_ITEMPOSTPAINT | CDDS_SUBITEM && progress {
        modern_row_progress(ui, draw.nmcd.hdc, draw.nmcd.dwItemSpec, draw.nmcd.rc);
    }
    CDRF_DODEFAULT as LRESULT
}

/// The card under the last row. The themed background carries a line at every
/// column edge; the rows cover it with their own colour and this covers the
/// rest. Nothing above the last row is touched, so a paint that redraws only
/// some rows, such as a hot-tracking change, leaves the other rows as they are.
unsafe fn modern_list_tail(ui: &MainUi, dc: HDC) {
    let mut client: RECT = zeroed();
    GetClientRect(ui.list, &mut client);
    let count = SendMessageW(ui.list, LVM_GETITEMCOUNT, 0, 0) as usize;
    let mut top = 0;
    if count > 0 {
        let mut last = RECT {
            left: LVIR_BOUNDS as i32,
            ..zeroed()
        };
        if SendMessageW(
            ui.list,
            LVM_GETITEMRECT,
            count - 1,
            &mut last as *mut _ as isize,
        ) == 0
        {
            return;
        }
        top = last.bottom;
    }
    let header = SendMessageW(ui.list, LVM_GETHEADER, 0, 0) as HWND;
    let mut header_rect: RECT = zeroed();
    if !header.is_null() && GetClientRect(header, &mut header_rect) != 0 {
        top = top.max(header_rect.bottom);
    }
    if top < client.bottom {
        let tail = RECT { top, ..client };
        fill_colour(dc, &tail, main_palette(ui.hwnd).card);
    }
}

/// The info tip of a cell: its full text when Modern's painting cuts it
/// short. The list measures a cell with its own, smaller insets and then
/// calls it unfolded, so a text cut by Modern's insets is named here.
pub(super) unsafe fn cell_tip(ui: &MainUi, tip: &mut NMLVGETINFOTIPW) {
    if tip.dwFlags & LVGIT_UNFOLDED == 0 || tip.pszText.is_null() || tip.cchTextMax < 2 {
        return;
    }
    // The notification names the row only; the column is the one under the
    // pointer.
    let mut hit: LVHITTESTINFO = zeroed();
    GetCursorPos(&mut hit.pt);
    ScreenToClient(ui.list, &mut hit.pt);
    SendMessageW(ui.list, LVM_SUBITEMHITTEST, 0, &mut hit as *mut _ as isize);
    let (Ok(view), Ok(row)) = (usize::try_from(hit.iSubItem), usize::try_from(tip.iItem)) else {
        return;
    };
    if hit.iItem != tip.iItem {
        return;
    }
    let Some(&column) = ui.columns.get(view) else {
        return;
    };
    let Some(text) = ui
        .visible_ids
        .get(row)
        .and_then(|id| ui.row_cache.get(id))
        .and_then(|cells| cells.get(column))
    else {
        return;
    };
    let width = SendMessageW(ui.list, LVM_GETCOLUMNWIDTH, view, 0) as i32;
    let dc = GetDC(ui.list);
    let previous = SelectObject(dc, SendMessageW(ui.list, WM_GETFONT, 0, 0) as HGDIOBJ);
    let needed = label_width(dc, text);
    SelectObject(dc, previous);
    ReleaseDC(ui.list, dc);
    if !cell_cut_short(needed, width, column, ui.dpi) {
        return;
    }
    let text: Vec<u16> = text.encode_utf16().collect();
    let length = text.len().min(tip.cchTextMax as usize - 1);
    let target = std::slice::from_raw_parts_mut(tip.pszText, length + 1);
    target[..length].copy_from_slice(&text[..length]);
    target[length] = 0;
}

/// Whether a text `needed` wide is cut short in a cell `width` wide, with the
/// insets `paint_row_cells` draws: 8 epx in, 6 epx short of the edge, and the
/// name's glyph slot and gap.
fn cell_cut_short(needed: i32, width: i32, column: usize, dpi: u32) -> bool {
    let insets = if column == COL_NAME {
        8 + 16 + 8 + 6
    } else {
        8 + 6
    };
    needed > width - scale(insets, dpi)
}

/// Every cell's text, 8 epx in from the column edge like the header's titles
/// and with an ellipsis when it does not fit (in the middle of a location),
/// and the progress bar in the
/// progress cell. The location and the dash of an empty value are secondary
/// text, except on a selected row, whose accent tint is too dark for it in
/// light mode. The name has a 16 epx glyph slot in front of it: an error mark for a
/// failed job and a pause mark for a paused one.
unsafe fn paint_row_cells(ui: &MainUi, dc: HDC, row: usize) {
    let palette = main_palette(ui.hwnd);
    let pad = scale(8, ui.dpi);
    let slot = scale(16, ui.dpi);
    let state = ui
        .visible_ids
        .get(row)
        .and_then(|id| ui.snapshot.jobs.iter().find(|job| &job.id == id))
        .map(|job| job.state);
    let glyph = match state {
        Some(JobState::Failed) => Some((0xe783u16, palette.critical)),
        Some(JobState::Paused) => Some((0xe769, palette.secondary)),
        _ => None,
    };
    let secondary = if SendMessageW(ui.list, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) != 0 {
        palette.text
    } else {
        palette.secondary
    };
    SetBkMode(dc, TRANSPARENT as i32);
    let mut buffer = [0u16; 512];
    for (view, &column) in ui.columns.iter().enumerate() {
        let view = view as i32;
        let mut cell = RECT {
            left: LVIR_BOUNDS as i32,
            top: view,
            ..zeroed()
        };
        if SendMessageW(
            ui.list,
            LVM_GETSUBITEMRECT,
            row,
            &mut cell as *mut _ as isize,
        ) == 0
        {
            continue;
        }
        // The first column's bounds are the whole row.
        if view == 0 {
            cell.right = cell.left + SendMessageW(ui.list, LVM_GETCOLUMNWIDTH, 0, 0) as i32;
        }
        let mut item: LVITEMW = zeroed();
        item.iSubItem = view;
        item.pszText = buffer.as_mut_ptr();
        item.cchTextMax = buffer.len() as i32;
        let length =
            SendMessageW(ui.list, LVM_GETITEMTEXTW, row, &mut item as *mut _ as isize) as i32;
        let mut text = RECT {
            left: cell.left + pad,
            right: cell.right - scale(6, ui.dpi),
            ..cell
        };
        if column == COL_NAME {
            if let (Some((code, colour)), false) = (glyph, ui.icon_font.is_null()) {
                let mut mark = RECT {
                    right: text.left + slot,
                    ..text
                };
                let previous = SelectObject(dc, ui.icon_font as HGDIOBJ);
                SetTextColor(dc, colour);
                DrawTextW(
                    dc,
                    &code,
                    1,
                    &mut mark,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
                SelectObject(dc, previous);
            }
            text.left += slot + pad;
        }
        let empty = length == 1 && buffer[0] == '—' as u16;
        SetTextColor(
            dc,
            if column == COL_LOCATION || empty {
                secondary
            } else {
                palette.text
            },
        );
        // A location keeps its drive and its last folder: the middle gives way.
        let ellipsis = if column == COL_LOCATION {
            DT_PATH_ELLIPSIS
        } else {
            DT_END_ELLIPSIS
        };
        DrawTextW(
            dc,
            item.pszText,
            length,
            &mut text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | ellipsis | DT_NOPREFIX,
        );
        if column == COL_PROGRESS {
            modern_row_progress(ui, dc, row, cell);
        }
    }
}

/// The row's colour across the whole list width, then its backplate.
unsafe fn paint_row_backplate(ui: &MainUi, draw: &NMLVCUSTOMDRAW) {
    let palette = main_palette(ui.hwnd);
    let dc = draw.nmcd.hdc;
    let row = draw.nmcd.dwItemSpec;
    let mut client: RECT = zeroed();
    GetClientRect(ui.list, &mut client);
    let band = RECT {
        left: client.left,
        right: client.right,
        ..draw.nmcd.rc
    };
    fill_colour(
        dc,
        &band,
        if row % 2 == 1 {
            palette.stripe
        } else {
            palette.card
        },
    );
    let selected = SendMessageW(ui.list, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) != 0;
    let hot = SendMessageW(ui.list, LVM_GETHOTITEM, 0, 0) == row as isize;
    let focused = GetFocus() == ui.list
        && SendMessageW(ui.list, LVM_GETITEMSTATE, row, LVIS_FOCUSED as isize) != 0
        && SendMessageW(ui.list, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0;
    if !(selected || hot || focused) {
        return;
    }
    let fill = match (selected, hot) {
        (true, true) => palette.row_selected_hover,
        (true, false) => palette.row_selected,
        (false, true) => palette.row_hover,
        (false, false) => {
            if row % 2 == 1 {
                palette.stripe
            } else {
                palette.card
            }
        }
    };
    paint_row_plate(dc, band, ui.dpi, &palette, fill, focused, selected);
}

/// A list row's backplate as WinUI's ListView draws it: `fill`, rounded, 2 epx
/// inside the row's `band`, the focus rings around it while `focused`, and
/// the selection mark at its start while `selected`.
unsafe fn paint_row_plate(
    dc: HDC,
    band: RECT,
    dpi: u32,
    palette: &MainPalette,
    fill: u32,
    focused: bool,
    selected: bool,
) {
    let plate = inflate(band, -scale(2, dpi), -scale(2, dpi));
    if focused {
        focus_plate(dc, &plate, scale(CORNER, dpi), dpi, palette, fill);
    } else {
        round_rect(dc, &plate, scale(CORNER, dpi), fill, None);
    }
    if selected {
        // ListView's selection mark: a 3 x 16 epx accent pill at the row start.
        let height = scale(16, dpi).min(plate.bottom - plate.top);
        let top = plate.top + (plate.bottom - plate.top - height) / 2;
        // Inside the focus rings when they show.
        let left = plate.left + if focused { scale(3, dpi) } else { 0 };
        let pill = RECT {
            left,
            top,
            right: left + scale(3, dpi),
            bottom: top + height,
        };
        round_rect(dc, &pill, scale(2, dpi), palette.accent, None);
    }
}

/// Draws a row's progress the way the Windows 11 ProgressBar does: a 1 epx
/// track under the cell text with a 3 epx rounded fill on it, accent while
/// running, success, caution or critical once the job finished, paused or failed.
pub(super) unsafe fn modern_row_progress(ui: &MainUi, dc: HDC, row: usize, cell: RECT) {
    let Some(job) = ui
        .visible_ids
        .get(row)
        .and_then(|id| ui.snapshot.jobs.iter().find(|job| &job.id == id))
    else {
        return;
    };
    let palette = main_palette(ui.hwnd);
    let contrast = high_contrast();
    let selected =
        contrast && SendMessageW(ui.list, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) != 0;
    let line = scale(1, ui.dpi).max(1);
    let thickness = scale(3, ui.dpi).max(3);
    let left = cell.left + scale(8, ui.dpi);
    let right = cell.right - scale(10, ui.dpi);
    if right <= left {
        return;
    }
    let middle = cell.bottom - scale(6, ui.dpi);
    let track = RECT {
        left,
        top: middle - line / 2,
        right,
        bottom: middle - line / 2 + line,
    };
    let track_colour = if selected {
        GetSysColor(COLOR_HIGHLIGHTTEXT)
    } else {
        palette.strong_stroke
    };
    fill_colour(dc, &track, track_colour);
    let width = right - left;
    let (start, end) = if job.state == JobState::Processing {
        processing_sweep(sweep_clock(), width)
    } else {
        let Some(fraction) = progress_fraction(job.state, job.downloaded, job.total) else {
            return;
        };
        (0, (width as f64 * fraction).round() as i32)
    };
    if end <= start {
        return;
    }
    let fill = RECT {
        left: left + start,
        top: middle - thickness / 2,
        right: left + end,
        bottom: middle - thickness / 2 + thickness,
    };
    let colour = if selected {
        GetSysColor(COLOR_HIGHLIGHTTEXT)
    } else if contrast {
        GetSysColor(COLOR_HIGHLIGHT)
    } else {
        match job.state {
            JobState::Completed => palette.success,
            JobState::Failed | JobState::Cancelled => palette.critical,
            JobState::Paused | JobState::AwaitingSource => palette.caution,
            JobState::Scheduled | JobState::Queued => palette.secondary,
            _ => palette.accent,
        }
    };
    round_rect(dc, &fill, thickness / 2, colour, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUE: AccentShades = AccentShades {
        dark1: 0xb85f00,
        light2: 0xffcd60,
    };

    #[test]
    fn composited_colours_match_the_winui_values_on_their_layer() {
        // COLORREF is 0x00BBGGRR; the WinUI colour is 0xAARRGGBB.
        assert_eq!(over(0xb3ffffff, 0xf3f3f3), 0xfbfbfb);
        assert_eq!(over(0x0dffffff, 0x202020), 0x2b2b2b);
        assert_eq!(over(0x19000000, 0x202020), 0x1d1d1d);
        assert_eq!(over(0x800000ff, 0x000000), 0x800000);
        assert_eq!(over(0x80ff0000, 0x000000), 0x000080);
        assert_eq!(over(with_alpha(0x0000ff, 0xff), 0x123456), 0x0000ff);
    }

    #[test]
    fn controls_on_a_card_stand_out_from_it_in_both_schemes() {
        let step = |a: u32, b: u32| {
            (0..3)
                .map(|shift| ((a >> (8 * shift)) & 0xff).abs_diff((b >> (8 * shift)) & 0xff))
                .max()
                .unwrap()
        };
        for dark in [false, true] {
            let palette = fluent_palette(dark, BLUE);
            let card = on_card(palette, dark);
            assert_eq!(card.base, palette.card);
            // WinUI's ControlFillColorDefault and ControlStrokeColorDefault
            // over the card, not over the window's base layer.
            let (fill, stroke) = if dark {
                (0x0fffffff, 0x12ffffff)
            } else {
                (0xb3ffffff, 0x0f000000)
            };
            assert_eq!(card.control, over(fill, palette.card));
            assert_eq!(card.control_stroke, over(stroke, palette.card));
            // The stroke outlines the control against the card.
            assert!(step(card.control_stroke, palette.card) >= 8, "dark {dark}");
            if dark {
                // On the dark card a control is lighter than the card itself.
                assert!(step(card.control, palette.card) >= 10);
            }
        }
    }

    #[test]
    fn text_stays_readable_on_every_main_surface_in_both_schemes() {
        for dark in [false, true] {
            let palette = fluent_palette(dark, BLUE);
            // Primary text: rows, tab page, buttons and the menu bar.
            for surface in [
                palette.base,
                palette.card,
                palette.stripe,
                palette.row_hover,
                palette.row_selected,
                palette.row_selected_hover,
                palette.control,
                palette.control_hover,
                palette.flyout,
            ] {
                assert!(contrast(palette.text, surface) >= 7.0, "dark={dark}");
            }
            // Secondary text: header and unselected tabs on the card and the
            // base, the status line on the base, a pressed button's label,
            // and the location and empty values on an unselected row; a
            // selected row shows them as primary text. On the control fill:
            // the cue banners of the search box and the dialog fields.
            for surface in [
                palette.base,
                palette.card,
                palette.control,
                palette.control_pressed,
                palette.stripe,
                palette.row_hover,
            ] {
                assert!(
                    contrast(palette.secondary, surface) >= 4.5,
                    "dark={dark} surface={surface:06x}"
                );
            }
            // The failed and paused marks before a name are graphics: 3:1.
            for surface in [
                palette.card,
                palette.stripe,
                palette.row_hover,
                palette.row_selected,
                palette.row_selected_hover,
            ] {
                assert!(contrast(palette.critical, surface) >= 3.0, "dark={dark}");
            }
            for fill in [palette.accent, palette.accent_hover] {
                assert!(contrast(palette.on_accent, fill) >= 4.5, "dark={dark}");
            }
        }
    }

    #[test]
    fn hover_and_selection_stand_out_from_both_row_colours() {
        for dark in [false, true] {
            let palette = fluent_palette(dark, BLUE);
            // The stripe is a subtle but visible step; hover and selection
            // are a clear one.
            let stripe = contrast(palette.stripe, palette.card);
            assert!(stripe > 1.07 && stripe < 1.15, "dark={dark} {stripe}");
            for row in [palette.card, palette.stripe] {
                for plate in [palette.row_hover, palette.row_selected] {
                    assert!(contrast(plate, row) > 1.12, "dark={dark}");
                }
            }
        }
    }

    #[test]
    fn the_card_is_a_lighter_layer_than_the_base_in_both_schemes() {
        for dark in [false, true] {
            let palette = fluent_palette(dark, BLUE);
            assert!(
                luminance(palette.card) > luminance(palette.base),
                "dark={dark}"
            );
            assert_ne!(palette.stroke, palette.card, "dark={dark}");
        }
    }

    #[test]
    fn arabic_never_asks_for_segoe_ui_variable() {
        for caption in [false, true] {
            assert_eq!(ramp_faces(Language::Arabic, caption), ["Segoe UI"]);
            for language in [Language::Turkish, Language::English, Language::German] {
                let faces = ramp_faces(language, caption);
                assert!(faces[0].starts_with("Segoe UI Variable"));
                assert_eq!(faces.last(), Some(&"Segoe UI"));
            }
        }
    }

    #[test]
    fn face_font_takes_an_installed_face_and_refuses_a_substituted_one() {
        unsafe {
            let font = face_font(-14, FW_NORMAL as i32, CLEARTYPE_QUALITY, &["Segoe UI"]);
            assert!(font.is_some());
            DeleteObject(font.unwrap() as HGDIOBJ);
            let missing = ["SSDownload Missing Face", "Another Missing Face"];
            assert!(face_font(-14, FW_NORMAL as i32, CLEARTYPE_QUALITY, &missing).is_none());
        }
    }

    #[test]
    fn rounded_corners_are_partly_covered_and_straight_edges_are_sharp() {
        let rect = [0.0, 0.0, 40.0, 32.0];
        assert_eq!(rounded_coverage(20.5, 16.5, rect, 4.0), 1.0);
        assert_eq!(rounded_coverage(-0.5, 16.5, rect, 4.0), 0.0);
        // A straight edge on a pixel boundary covers the pixel inside fully.
        assert_eq!(rounded_coverage(0.5, 16.5, rect, 4.0), 1.0);
        // The corner pixel is outside the arc, the next one along the diagonal
        // is cut by it: that partial pixel is the anti-aliasing.
        assert_eq!(rounded_coverage(0.5, 0.5, rect, 4.0), 0.0);
        let cut = rounded_coverage(1.5, 1.5, rect, 4.0);
        assert!(cut > 0.0 && cut < 1.0, "{cut}");
        // Without a radius the corner pixel is covered like any other.
        assert_eq!(rounded_coverage(0.5, 0.5, rect, 0.0), 1.0);
    }

    #[test]
    fn a_button_surface_has_its_darker_edge_at_the_bottom_and_a_ring_only_with_focus() {
        let surface = |focus| ButtonSurface {
            background: 0x111111,
            fill: 0xeeeeee,
            stroke: 0x888888,
            stroke_bottom: 0x444444,
            radius: 4.0,
            focus,
        };
        let (width, height) = (40, 38);
        let plain = paint_button_surface(width, height, 3.0, &surface(None));
        let at = |pixels: &[u32], x: usize, y: usize| pixels[y * width + x];
        assert_eq!(at(&plain, 20, 19), 0xeeeeee);
        assert_eq!(at(&plain, 20, 3), 0x888888);
        assert_eq!(at(&plain, 20, height - 4), 0x444444);
        assert_eq!(at(&plain, 20, 1), 0x111111);
        let ring = paint_button_surface(width, height, 3.0, &surface(Some((0xff0000, 0x00ff00))));
        assert_eq!(at(&ring, 20, 1), 0xff0000);
        assert_eq!(at(&ring, 20, 2), 0x00ff00);
        assert_eq!(at(&ring, 20, 19), 0xeeeeee);
    }

    #[test]
    fn text_on_a_custom_accent_takes_the_colour_that_reads() {
        // A pale yellow accent in the light theme, a deep navy in the dark one:
        // the reverse of the default blue's shades.
        let pale = AccentShades {
            dark1: 0x80f0ff,
            light2: 0x80f0ff,
        };
        let deep = AccentShades {
            dark1: 0x401010,
            light2: 0x401010,
        };
        for dark in [false, true] {
            let on_pale = fluent_palette(dark, pale);
            assert_eq!(on_pale.on_accent, 0x000000);
            assert!(contrast(on_pale.on_accent, on_pale.accent) >= 4.5);
            let on_deep = fluent_palette(dark, deep);
            assert_eq!(on_deep.on_accent, 0xffffff);
            assert!(contrast(on_deep.on_accent, on_deep.accent) >= 4.5);
        }
    }

    #[test]
    fn text_on_any_accent_reads_at_rest_and_under_the_pointer() {
        for dark in [false, true] {
            for r in (0..=255u32).step_by(5) {
                for g in (0..=255u32).step_by(5) {
                    for b in (0..=255u32).step_by(5) {
                        let colour = r | g << 8 | b << 16;
                        let palette = fluent_palette(
                            dark,
                            AccentShades {
                                dark1: colour,
                                light2: colour,
                            },
                        );
                        for fill in [palette.accent, palette.accent_hover] {
                            let ratio = contrast(palette.on_accent, fill);
                            assert!(ratio >= 4.5, "dark={dark} accent={colour:06x} {ratio:.2}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_check_box_takes_winui_s_colours_for_each_state() {
        use super::{check_box_colours, ButtonState};
        let look = |disabled, pressed, hot| ButtonState {
            primary: false,
            disabled,
            pressed,
            hot,
            focused: false,
            underline: false,
        };
        for dark in [false, true] {
            let p = fluent_palette(
                dark,
                AccentShades {
                    dark1: 0xb35a00,
                    light2: 0xffc54c,
                },
            );
            let rest = look(false, false, false);
            assert_eq!(
                check_box_colours(&p, dark, true, rest),
                (p.accent, p.accent, p.on_accent)
            );
            assert_eq!(
                check_box_colours(&p, dark, true, look(false, false, true)),
                (p.accent_hover, p.accent_hover, p.on_accent)
            );
            // Pressed wins over hot, as the pointer is on a pressed box.
            assert_eq!(
                check_box_colours(&p, dark, true, look(false, true, true)),
                (p.accent_pressed, p.accent_pressed, p.on_accent_pressed)
            );
            let (fill, stroke, mark) = check_box_colours(&p, dark, false, rest);
            assert_eq!((stroke, mark), (p.strong_stroke, p.card));
            assert_ne!(fill, p.card);
            // A disabled clear box is the card with a muted stroke, whatever
            // the pointer does.
            let disabled = check_box_colours(&p, dark, false, look(true, true, true));
            assert_eq!(disabled.0, p.card);
            assert_ne!(disabled.1, p.strong_stroke);
            assert_eq!(
                disabled,
                check_box_colours(&p, dark, false, look(true, false, false))
            );
            // A disabled checked box fills with AccentFillColorDisabled, the
            // same colour as the disabled stroke, and the light theme's mark
            // stays white.
            let (fill, stroke, mark) = check_box_colours(&p, dark, true, look(true, false, false));
            assert_eq!((fill, stroke), (disabled.1, disabled.1));
            assert_ne!(fill, p.accent);
            if dark {
                assert_ne!(mark, 0xffffff);
                assert_ne!(mark, fill);
            } else {
                assert_eq!(mark, 0xffffff);
            }
            // A clear box under the pointer keeps its strong stroke on a
            // stronger fill; pressed, the fill deepens again and the stroke
            // fades to the disabled one.
            let (rest_fill, ..) = check_box_colours(&p, dark, false, rest);
            let hot = check_box_colours(&p, dark, false, look(false, false, true));
            assert_eq!((hot.1, hot.2), (p.strong_stroke, p.card));
            assert_ne!(hot.0, rest_fill);
            assert_ne!(hot.0, p.card);
            let pressed = check_box_colours(&p, dark, false, look(false, true, true));
            assert_eq!((pressed.1, pressed.2), (disabled.1, p.card));
            assert_ne!(pressed.0, hot.0);
            assert_ne!(pressed.0, rest_fill);
        }
    }

    #[test]
    fn a_cell_gets_a_tip_once_modern_s_insets_cut_its_text() {
        use super::{cell_cut_short, COL_LOCATION, COL_NAME};
        // A name keeps 38 epx for its insets and glyph slot, other cells 14.
        assert!(!cell_cut_short(162, 200, COL_NAME, 96));
        assert!(cell_cut_short(163, 200, COL_NAME, 96));
        assert!(!cell_cut_short(186, 200, COL_LOCATION, 96));
        assert!(cell_cut_short(187, 200, COL_LOCATION, 96));
        // The insets grow with the DPI: 57 and 21 px at 144.
        assert!(cell_cut_short(244, 300, COL_NAME, 144));
        assert!(!cell_cut_short(243, 300, COL_NAME, 144));
        assert!(cell_cut_short(280, 300, COL_LOCATION, 144));
        assert!(!cell_cut_short(279, 300, COL_LOCATION, 144));
    }

    #[test]
    fn the_focus_rings_grow_with_the_margin_at_a_higher_dpi() {
        let surface = ButtonSurface {
            background: 0x111111,
            fill: 0xeeeeee,
            stroke: 0x888888,
            stroke_bottom: 0x444444,
            radius: 8.0,
            focus: Some((0xff0000, 0x00ff00)),
        };
        // 200%: a 6 px margin holds a 4 px outer and a 2 px inner ring.
        let width = 80;
        let pixels = paint_button_surface(width, 76, 6.0, &surface);
        let at = |y: usize| pixels[y * width + 40];
        assert_eq!(at(0), 0xff0000);
        assert_eq!(at(3), 0xff0000);
        assert_eq!(at(4), 0x00ff00);
        assert_eq!(at(5), 0x00ff00);
        assert_eq!(at(6), 0x888888);
    }

    /// A checked check box labelled `label`, painted at `width` x `height` into
    /// a right-to-left DIB, as 0x00BBGGRR colours, with the palette it used.
    unsafe fn rtl_check_box_colours(
        width: i32,
        height: i32,
        label: &str,
    ) -> (Vec<u32>, MainPalette) {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        assert!(!bitmap.is_null() && !bits.is_null());
        let previous = SelectObject(dc, bitmap as HGDIOBJ);
        SetLayout(dc, LAYOUT_RTL);
        let parent = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
        let check = control(
            parent,
            "BUTTON",
            label,
            WS_CHILD | BS_AUTOCHECKBOX as u32,
            0,
            1,
        );
        SendMessageW(check, BM_SETCHECK, BST_CHECKED as usize, 0);
        let font = ramp_font(96, BODY_PX, false);
        let palette = fluent_palette(false, BLUE);
        let look = ButtonState {
            primary: false,
            disabled: false,
            pressed: false,
            hot: false,
            focused: false,
            underline: false,
        };
        paint_check_box(
            dc,
            RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            96,
            &palette,
            false,
            font,
            check,
            look,
        );
        let pixels = std::slice::from_raw_parts(bits as *const u32, (width * height) as usize)
            .iter()
            // A DIB pixel is 0x00RRGGBB, a COLORREF 0x00BBGGRR.
            .map(|&pixel| ((pixel & 0xff) << 16) | (pixel & 0xff00) | ((pixel >> 16) & 0xff))
            .collect();
        SelectObject(dc, previous);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteObject(font as HGDIOBJ);
        DeleteDC(dc);
        DestroyWindow(parent);
        (pixels, palette)
    }

    /// The columns of a `width` wide picture holding a colour `wanted` accepts.
    fn columns_with(colours: &[u32], width: i32, wanted: &dyn Fn(u32) -> bool) -> Vec<i32> {
        let height = colours.len() as i32 / width;
        (0..width)
            .filter(|&x| (0..height).any(|y| wanted(colours[(y * width + x) as usize])))
            .collect()
    }

    fn dark(c: u32) -> bool {
        (c & 0xff) + ((c >> 8) & 0xff) + ((c >> 16) & 0xff) < 200
    }

    #[test]
    fn a_right_to_left_check_box_draws_its_box_beside_its_label() {
        unsafe {
            let width = 200;
            let (colours, palette) = rtl_check_box_colours(width, 32, "WWWW");
            let boxed = columns_with(&colours, width, &|c| c == palette.accent);
            let label = columns_with(&colours, width, &dark);
            assert!(
                !boxed.is_empty(),
                "the checked box is painted in the accent"
            );
            assert!(!label.is_empty(), "the label is painted");
            // Mirrored, the label runs left of the box, which keeps the focus
            // margin on the right.
            assert!(
                label.iter().max() < boxed.iter().min(),
                "box {boxed:?} label {label:?}"
            );
            assert!(*boxed.iter().max().unwrap() >= width - 3 - 20 - 1);
        }
    }

    #[test]
    fn a_right_to_left_check_box_narrower_than_its_box_paints_what_fits() {
        unsafe {
            // A check box at the size it is created at, before the layout.
            let (colours, palette) = rtl_check_box_colours(10, 10, "Keep on top");
            assert!(colours.contains(&palette.accent));
        }
    }

    #[test]
    fn a_check_box_with_no_label_paints_only_its_box() {
        unsafe {
            let width = 200;
            let (colours, palette) = rtl_check_box_colours(width, 32, "");
            assert!(colours.contains(&palette.accent));
            assert!(columns_with(&colours, width, &dark).is_empty());
        }
    }

    #[test]
    fn empty_text_measures_and_draws_nothing() {
        unsafe {
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(null_mut(), screen);
            assert_eq!(label_width(dc, ""), 0);
            let mut area = RECT {
                right: 100,
                bottom: 20,
                ..zeroed()
            };
            for format in [
                DT_CALCRECT | DT_WORDBREAK | DT_EXPANDTABS | DT_NOPREFIX,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            ] {
                draw_text(dc, &mut [], &mut area, format);
            }
            DeleteDC(dc);
        }
    }

    #[test]
    fn a_new_check_box_holds_its_box_and_focus_ring() {
        unsafe {
            let parent = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
            let check = control(
                parent,
                "BUTTON",
                "Keep on top",
                WS_CHILD | BS_AUTOCHECKBOX as u32,
                0,
                1,
            );
            hold_check_box(check);
            let mut rect: RECT = zeroed();
            GetClientRect(check, &mut rect);
            let dpi = GetDpiForWindow(check).max(96);
            DestroyWindow(parent);
            let side = scale(CHECK_BOX + 2 * FOCUS_MARGIN, dpi);
            assert_eq!((rect.right, rect.bottom), (side, side));
        }
    }

    #[test]
    fn a_combo_box_with_no_selection_paints_its_face_without_text() {
        unsafe {
            let (width, height) = (200, 38);
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(null_mut(), screen);
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null() && !bits.is_null());
            let previous = SelectObject(dc, bitmap as HGDIOBJ);
            let parent = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
            let font = ramp_font(96, BODY_PX, false);
            // The text area: right of the left padding, left of the chevron.
            let text_columns = |selected: bool| -> usize {
                let combo = control(
                    parent,
                    "COMBOBOX",
                    "",
                    WS_CHILD | CBS_DROPDOWNLIST as u32,
                    0,
                    1,
                );
                apply_font(combo, font);
                MoveWindow(combo, 0, 0, width, 200, 0);
                combo_add(combo, "WWWW");
                if selected {
                    combo_select(combo, 0);
                }
                let mut face: RECT = zeroed();
                GetClientRect(combo, &mut face);
                let all = RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                };
                FillRect(dc, &all, GetStockObject(WHITE_BRUSH) as HBRUSH);
                paint_combo(combo, dc);
                DestroyWindow(combo);
                let pixels =
                    std::slice::from_raw_parts(bits as *const u32, (width * height) as usize);
                let dark = |p: u32| (p & 0xff) + ((p >> 8) & 0xff) + ((p >> 16) & 0xff) < 200;
                (20..width - 40)
                    .filter(|&x| {
                        (0..face.bottom.min(height)).any(|y| dark(pixels[(y * width + x) as usize]))
                    })
                    .count()
            };
            let empty = text_columns(false);
            let chosen = text_columns(true);
            SelectObject(dc, previous);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteObject(font as HGDIOBJ);
            DeleteDC(dc);
            DestroyWindow(parent);
            assert_eq!(empty, 0, "a box with no selection shows no text");
            assert!(chosen > 0, "the selected entry is painted");
        }
    }

    #[test]
    fn a_modern_list_keeps_its_texts_and_sits_inside_its_frame() {
        unsafe {
            let parent = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
            let font = ramp_font(96, BODY_PX, false);
            for (style, notifies) in [(0, false), (LBS_NOTIFY as u32, true)] {
                let list = modern_list(parent, font, 1, style);
                let bits = GetWindowLongW(list, GWL_STYLE) as u32;
                let owner_drawn = (LBS_OWNERDRAWFIXED | LBS_HASSTRINGS) as u32;
                assert_eq!(bits & owner_drawn, owner_drawn);
                assert_eq!(bits & LBS_NOTIFY as u32 != 0, notifies);
                assert_eq!(bits & WS_BORDER, 0, "the dialog paints the frame");
                // An owner-drawn list keeps its entries' texts only with
                // LBS_HASSTRINGS, and type-ahead and LB_GETTEXT need them.
                let entry = wide("Night");
                SendMessageW(list, LB_ADDSTRING, 0, entry.as_ptr() as isize);
                let mut read = [0u16; 16];
                let length = SendMessageW(list, LB_GETTEXT, 0, read.as_mut_ptr() as isize);
                assert_eq!(
                    String::from_utf16_lossy(&read[..length.max(0) as usize]),
                    "Night"
                );
                let face = RECT {
                    left: 10,
                    top: 20,
                    right: 210,
                    bottom: 120,
                };
                place_modern(list, true, face, 96);
                let edges = |r: RECT| (r.left, r.top, r.right, r.bottom);
                assert_eq!(edges(child_rect(parent, list)), (12, 22, 208, 118));
                assert_eq!(edges(list_frame(parent, list, 96)), edges(face));
                DestroyWindow(list);
            }
            DeleteObject(font as HGDIOBJ);
            DestroyWindow(parent);
        }
    }

    #[test]
    fn a_selected_list_entry_gets_the_tinted_plate_and_the_accent_mark() {
        unsafe {
            let (width, height) = (200, 32);
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(null_mut(), screen);
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null() && !bits.is_null());
            let previous = SelectObject(dc, bitmap as HGDIOBJ);
            let parent = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
            let font = ramp_font(96, BODY_PX, false);
            let list = modern_list(parent, font, 1, 0);
            let entry = wide("Night");
            SendMessageW(list, LB_ADDSTRING, 0, entry.as_ptr() as isize);
            let palette = fluent_palette(false, BLUE);
            // A DIB pixel is 0x00RRGGBB, a COLORREF 0x00BBGGRR.
            let colour =
                |pixel: u32| ((pixel & 0xff) << 16) | (pixel & 0xff00) | ((pixel >> 16) & 0xff);
            let paint = |selected: bool| -> Vec<u32> {
                let item = DRAWITEMSTRUCT {
                    CtlType: ODT_LISTBOX,
                    CtlID: 1,
                    itemID: 0,
                    itemAction: ODA_DRAWENTIRE,
                    itemState: if selected { ODS_SELECTED } else { 0 },
                    hwndItem: list,
                    hDC: dc,
                    rcItem: RECT {
                        left: 0,
                        top: 0,
                        right: width,
                        bottom: height,
                    },
                    itemData: 0,
                };
                paint_list_item(&item, 96, &palette, font);
                std::slice::from_raw_parts(bits as *const u32, (width * height) as usize)
                    .iter()
                    .map(|&pixel| colour(pixel))
                    .collect()
            };
            let selected = paint(true);
            let plain = paint(false);
            SelectObject(dc, previous);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteObject(font as HGDIOBJ);
            DeleteDC(dc);
            DestroyWindow(parent);
            let has = |pixels: &[u32], wanted: u32| pixels.contains(&wanted);
            assert!(has(&selected, palette.row_selected), "the selection plate");
            assert!(has(&selected, palette.accent), "the selection mark");
            assert!(!has(&plain, palette.row_selected));
            assert!(!has(&plain, palette.accent));
        }
    }

    #[test]
    fn only_the_dialogs_modern_lays_out_lose_their_label_colons() {
        unsafe {
            let tools = DialogKind::Tools(ToolsFields {
                status: null_mut(),
                progress: null_mut(),
                install: null_mut(),
                update: null_mut(),
                last_render: String::new(),
            });
            let rules = DialogKind::Rules(RuleFields {
                list: null_mut(),
                ids: Vec::new(),
                host: null_mut(),
                dir: null_mut(),
                subdomains: null_mut(),
                file: null_mut(),
                video: null_mut(),
                audio: null_mut(),
                priority: null_mut(),
            });
            assert!(
                modern_form(&tools).is_none(),
                "the media tools keep Classic's form"
            );
            assert!(modern_form(&rules).is_some());
            let parent = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
            let label = control(parent, "STATIC", "Host:", WS_CHILD, 0, 1);
            let error = control(parent, "STATIC", "Failed:", WS_CHILD, 0, 2);
            strip_label_colons(parent, error);
            assert_eq!(super::super::text(label), "Host");
            assert_eq!(
                super::super::text(error),
                "Failed:",
                "the error line is left alone"
            );
            DestroyWindow(parent);
        }
    }
}
