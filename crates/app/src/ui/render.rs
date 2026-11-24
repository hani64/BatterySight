//! Text rendering into 32-bit bitmaps with per-pixel alpha, for the widget's strip, the
//! tray icons and the hover card.
//!
//! DirectWrite draws the text (`dwrite`): grayscale antialiasing with correct premultiplied
//! alpha, over a transparent background (the strip, the icons) or the card's opaque one.
//! Without it, GDI draws: text white on black with grayscale antialiasing, [`colorize`]
//! then turning the coverage into alpha and applying each part's colour (the card drawn in
//! its colours on its opaque background instead).

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateBitmap,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, FW_BOLD, FW_NORMAL, FW_SEMIBOLD, FillRect, GdiFlush,
    GetTextExtentPoint32W, GetTextFaceW, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};
use windows::core::{PCWSTR, w};

use super::DeviceView;
use super::dwrite::{self, Engine, Fill, FontSpec, Run};

/// Icon fonts, best first: Windows 11 ships Segoe Fluent Icons, Windows 10 Segoe MDL2 Assets
/// (same code points for the glyphs used here).
const ICON_FACES: [PCWSTR; 2] = [w!("Segoe Fluent Icons"), w!("Segoe MDL2 Assets")];
const TEXT_FACE: PCWSTR = w!("Segoe UI");
/// Shown after the level of a charging device.
const BOLT: char = '\u{E945}';
/// Shown after the level of a device charged full on power (instead of the bolt).
const CHECK: char = '\u{E73E}';
/// Shown when there is no device, so the widget stays clickable.
const NO_DEVICE: char = '\u{E83F}';
/// Opacity of an asleep device (its last known level).
const DIMMED: u8 = 115;

/// Colour and opacity for the columns `x0..x1` of a bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub x0: i32,
    pub x1: i32,
    pub rgb: [u8; 3],
    pub opacity: u8,
}

/// Turns white-on-black text coverage (BGRA rows of `width` pixels) into coloured pixels.
///
/// A pixel's alpha is its coverage (brightest of B, G, R) times the opacity of the span
/// covering its column; pixels outside every span get no colour. No pixel gets less alpha
/// than `floor` (black there). With `premultiplied` the colour is scaled by alpha, as
/// `UpdateLayeredWindow` needs; otherwise it is left whole, as icons need.
pub fn colorize(pixels: &mut [u8], width: usize, spans: &[Span], floor: u8, premultiplied: bool) {
    let (pixels, _) = pixels.as_chunks_mut::<4>();
    for (i, px) in pixels.iter_mut().enumerate() {
        let x = (i % width.max(1)) as i32;
        let coverage = u32::from(px[0].max(px[1]).max(px[2]));
        let (rgb, alpha) = match spans.iter().find(|s| (s.x0..s.x1).contains(&x)) {
            Some(s) => (s.rgb, coverage * u32::from(s.opacity) / 255),
            None => ([0; 3], 0),
        };
        let scale = |c: u8| {
            if premultiplied {
                (u32::from(c) * alpha / 255) as u8
            } else if alpha == 0 {
                0
            } else {
                c
            }
        };
        px[0] = scale(rgb[2]);
        px[1] = scale(rgb[1]);
        px[2] = scale(rgb[0]);
        px[3] = (alpha as u8).max(floor);
    }
}

/// Text and level colours for the taskbar's theme.
struct Palette {
    text: [u8; 3],
    low: [u8; 3],
}

impl Palette {
    fn new(light: bool) -> Palette {
        if light {
            Palette {
                text: [0, 0, 0],
                low: [196, 43, 28],
            }
        } else {
            Palette {
                text: [255, 255, 255],
                low: [255, 153, 164],
            }
        }
    }
}

/// What a part of the widget's strip is drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Face {
    /// The level.
    Text,
    /// The device's glyph.
    Icon,
    /// The bolt or the check after the level.
    Mark,
    /// The caption telling apart devices of a kind.
    Caption,
}

/// One text of the strip: drawn at (`x`, `y`), `size` as measured.
#[derive(Debug)]
struct Part {
    face: Face,
    text: Vec<u16>,
    x: i32,
    y: i32,
    size: SIZE,
    rgb: [u8; 3],
    opacity: u8,
}

/// Where the strip's parts go.
#[derive(Debug)]
struct Layout {
    parts: Vec<Part>,
    /// Each view's drawn extent (`x0`, `x1`), in view order.
    drawn: Vec<(i32, i32)>,
    width: i32,
}

/// A painted strip, and each view's columns of it ([`device_columns`]).
pub type Strip = (Painter, Vec<(i32, i32)>);

/// The widget's strip for `views`: `dpi` scales it, `height` is the taskbar's client
/// height. Every pixel gets at least alpha 1, so the whole strip takes clicks. Also returns
/// each view's columns of the strip ([`device_columns`]), in view order.
pub fn widget_strip(views: &[DeviceView], dpi: u32, height: i32, light: bool) -> Option<Strip> {
    dwrite::with(|e| widget_strip_direct(e, views, dpi, height, light))
        .or_else(|| widget_strip_gdi(views, dpi, height, light))
}

/// The fonts of the strip's faces, in [`face_index`] order: `dpi` scales them.
fn strip_fonts(dpi: u32) -> [FontSpec; 4] {
    let px = |v: f32| v * dpi as f32 / 96.0;
    let spec = |faces, size, weight: i32| FontSpec {
        faces,
        size: px(size),
        weight,
    };
    [
        spec(&[TEXT_FACE], 12.0, FW_NORMAL.0 as i32),
        spec(&ICON_FACES, 15.0, FW_NORMAL.0 as i32),
        spec(&ICON_FACES, 11.0, FW_NORMAL.0 as i32),
        spec(&[TEXT_FACE], 9.0, FW_SEMIBOLD.0 as i32),
    ]
}

fn face_index(face: Face) -> usize {
    match face {
        Face::Text => 0,
        Face::Icon => 1,
        Face::Mark => 2,
        Face::Caption => 3,
    }
}

/// [`widget_strip`] by DirectWrite. `Ok(None)`: no DIB.
fn widget_strip_direct(
    e: &Engine,
    views: &[DeviceView],
    dpi: u32,
    height: i32,
    light: bool,
) -> windows::core::Result<Option<Strip>> {
    let fonts = e.fonts(&strip_fonts(dpi))?;
    let height = height.max(1);
    let mut failed = None;
    let layout = layout(views, dpi, height, light, |face, text| {
        e.measure(&fonts, face_index(face), text)
            .unwrap_or_else(|err| {
                failed.get_or_insert(err);
                SIZE::default()
            })
    });
    if let Some(err) = failed {
        return Err(err);
    }
    let Some(mut p) = Painter::new() else {
        return Ok(None);
    };
    if !p.begin(layout.width, height) {
        return Ok(None);
    }
    let runs: Vec<Run> = layout
        .parts
        .iter()
        .map(|part| Run {
            font: face_index(part.face),
            text: &part.text,
            x: part.x,
            y: part.y,
            size: part.size,
            rgb: part.rgb,
            opacity: part.opacity,
        })
        .collect();
    let size = SIZE {
        cx: layout.width,
        cy: height,
    };
    e.draw(p.dc, size, &fonts, None, &runs)?;
    if let Some(pixels) = p.pixels() {
        floor_alpha(pixels, 1);
    }
    Ok(Some((p, device_columns(&layout.drawn, layout.width))))
}

/// [`widget_strip`] by GDI.
fn widget_strip_gdi(views: &[DeviceView], dpi: u32, height: i32, light: bool) -> Option<Strip> {
    let px = |v: i32| v * dpi as i32 / 96;
    let mut p = Painter::new()?;
    let text_font = p.font(&[TEXT_FACE], -px(12), FW_NORMAL.0 as i32)?;
    let icon_font = p.font(&ICON_FACES, -px(15), FW_NORMAL.0 as i32)?;
    let mark_font = p.font(&ICON_FACES, -px(11), FW_NORMAL.0 as i32)?;
    let caption_font = p.font(&[TEXT_FACE], -px(9), FW_SEMIBOLD.0 as i32)?;
    let font = |face| match face {
        Face::Text => text_font,
        Face::Icon => icon_font,
        Face::Mark => mark_font,
        Face::Caption => caption_font,
    };
    let height = height.max(1);
    let layout = layout(views, dpi, height, light, |face, text| {
        p.measure(font(face), text)
    });
    if !p.begin(layout.width, height) {
        return None;
    }
    let mut spans = Vec::with_capacity(layout.parts.len());
    for part in &layout.parts {
        p.draw(font(part.face), part.x, part.y, &part.text);
        spans.push(Span {
            x0: part.x,
            x1: part.x + part.size.cx,
            rgb: part.rgb,
            opacity: part.opacity,
        });
    }
    p.finish(&spans, 1, true);
    Some((p, device_columns(&layout.drawn, layout.width)))
}

/// Lays the strip for `views` out on a `height` px taskbar (`dpi` scales it), with
/// `measure` giving a text's size in a face.
fn layout(
    views: &[DeviceView],
    dpi: u32,
    height: i32,
    light: bool,
    mut measure: impl FnMut(Face, &[u16]) -> SIZE,
) -> Layout {
    let px = |v: i32| v * dpi as i32 / 96;
    let palette = Palette::new(light);
    // A part measured and centred vertically, its `x` still to be set.
    let mut part = |face, text: &str, rgb, opacity| {
        let text: Vec<u16> = text.encode_utf16().collect();
        let size = measure(face, &text);
        Part {
            face,
            text,
            x: 0,
            y: (height - size.cy) / 2,
            size,
            rgb,
            opacity,
        }
    };
    let mut parts = Vec::new();
    let mut x = px(4);
    if views.is_empty() {
        let none = part(Face::Icon, &NO_DEVICE.to_string(), palette.text, 255);
        next(&mut parts, &mut x, none, 0);
    }
    let mut drawn = Vec::with_capacity(views.len());
    for (i, v) in views.iter().enumerate() {
        let opacity = if v.dimmed { DIMMED } else { 255 };
        let level = if v.low { palette.low } else { palette.text };
        let last = i + 1 == views.len();
        let gap = if last { 0 } else { px(10) };
        let glyph = part(Face::Icon, &v.glyph.to_string(), palette.text, opacity);
        let caption = v.caption.as_deref();
        let caption = caption.map(|c| part(Face::Caption, c, palette.text, opacity));
        let start = x;
        match caption {
            // Under the glyph, the two centred as one, when they fit the taskbar. They share
            // columns, and `colorize` colours a column by the first span covering it: the
            // two must keep one colour and opacity (both the text colour at the device's).
            Some(mut caption) if glyph.size.cy + caption.size.cy <= height => {
                let column = glyph.size.cx.max(caption.size.cx);
                let top = (height - glyph.size.cy - caption.size.cy) / 2;
                let glyph = Part {
                    x: x + (column - glyph.size.cx) / 2,
                    y: top,
                    ..glyph
                };
                caption.x = x + (column - caption.size.cx) / 2;
                caption.y = top + glyph.size.cy;
                parts.extend([glyph, caption]);
                x += column + px(4);
            }
            Some(caption) => {
                next(&mut parts, &mut x, glyph, px(2));
                next(&mut parts, &mut x, caption, px(4));
            }
            None => {
                next(&mut parts, &mut x, glyph, px(4));
            }
        }
        let text = part(Face::Text, &v.text, level, opacity);
        let mark = if v.full {
            Some(CHECK)
        } else if v.charging {
            Some(BOLT)
        } else {
            None
        };
        let (_, end) = match mark {
            Some(mark) => {
                next(&mut parts, &mut x, text, px(2));
                let mark = part(Face::Mark, &mark.to_string(), palette.text, opacity);
                next(&mut parts, &mut x, mark, gap)
            }
            None => next(&mut parts, &mut x, text, gap),
        };
        drawn.push((start, end));
    }
    Layout {
        parts,
        drawn,
        width: x + px(4),
    }
}

/// Puts `part` at `x`, then moves `x` past it and `gap_after`. Returns its extent.
fn next(parts: &mut Vec<Part>, x: &mut i32, mut part: Part, gap_after: i32) -> (i32, i32) {
    part.x = *x;
    let extent = (*x, *x + part.size.cx);
    *x += part.size.cx + gap_after;
    parts.push(part);
    extent
}

/// Each device's columns of the strip, from its drawn extent (`x0`, `x1`) per device: the
/// gap between two devices is split at its middle, the first starts at 0 and the last ends
/// at `width`, so every column of the strip belongs to a device.
fn device_columns(drawn: &[(i32, i32)], width: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(drawn.len());
    let mut left = 0;
    for (i, &(_, end)) in drawn.iter().enumerate() {
        let right = drawn
            .get(i + 1)
            .map_or(width, |&(next, _)| (end + next) / 2);
        out.push((left, right));
        left = right;
    }
    out
}

/// A `size`×`size` tray icon: the percentage as a number, else the device's glyph.
pub fn tray_icon(view: Option<&DeviceView>, size: i32, light: bool) -> Option<HICON> {
    dwrite::with(|e| tray_icon_direct(e, view, size, light))
        .or_else(|| tray_icon_gdi(view, size, light))
}

/// A tray icon's number (the percentage without "%"; `None`: draw the glyph), colour and
/// opacity.
fn tray_look(view: Option<&DeviceView>, light: bool) -> (Option<String>, [u8; 3], u8) {
    let palette = Palette::new(light);
    match view {
        Some(v) => (
            v.text.strip_suffix('%').map(str::to_owned),
            if v.low { palette.low } else { palette.text },
            if v.dimmed { DIMMED } else { 255 },
        ),
        None => (None, palette.text, 255),
    }
}

/// [`tray_icon`] by DirectWrite. `Ok(None)`: no DIB or icon.
fn tray_icon_direct(
    e: &Engine,
    view: Option<&DeviceView>,
    size: i32,
    light: bool,
) -> windows::core::Result<Option<HICON>> {
    let (text, rgb, opacity) = tray_look(view, light);
    let (fonts, wide, measured) = match text {
        Some(number) => {
            let wide: Vec<u16> = number.encode_utf16().collect();
            // The largest font whose text fits the icon.
            let mut height = size;
            loop {
                let spec = FontSpec {
                    faces: &[TEXT_FACE],
                    size: height as f32,
                    weight: FW_SEMIBOLD.0 as i32,
                };
                let fonts = e.fonts(&[spec])?;
                let measured = e.measure(&fonts, 0, &wide)?;
                if measured.cx <= size || height <= 6 {
                    break (fonts, wide, measured);
                }
                height -= 1;
            }
        }
        None => {
            let glyph = view.map_or(NO_DEVICE, |v| v.glyph);
            let spec = FontSpec {
                faces: &ICON_FACES,
                size: (size * 7 / 8) as f32,
                weight: FW_NORMAL.0 as i32,
            };
            let fonts = e.fonts(&[spec])?;
            let wide: Vec<u16> = glyph.to_string().encode_utf16().collect();
            let measured = e.measure(&fonts, 0, &wide)?;
            (fonts, wide, measured)
        }
    };
    let Some(mut p) = Painter::new() else {
        return Ok(None);
    };
    if !p.begin(size, size) {
        return Ok(None);
    }
    let run = Run {
        font: 0,
        text: &wide,
        x: (size - measured.cx) / 2,
        y: (size - measured.cy) / 2,
        size: measured,
        rgb,
        opacity,
    };
    let square = SIZE { cx: size, cy: size };
    e.draw(p.dc, square, &fonts, None, &[run])?;
    // Icons take straight alpha.
    if let Some(pixels) = p.pixels() {
        unpremultiply(pixels);
    }
    Ok(p.into_icon())
}

/// [`tray_icon`] by GDI.
fn tray_icon_gdi(view: Option<&DeviceView>, size: i32, light: bool) -> Option<HICON> {
    let (text, rgb, opacity) = tray_look(view, light);
    let mut p = Painter::new()?;
    let (font, wide) = match text {
        Some(number) => {
            let wide: Vec<u16> = number.encode_utf16().collect();
            // The largest font whose text fits the icon.
            let mut height = size;
            loop {
                let font = p.font(&[TEXT_FACE], -height, FW_SEMIBOLD.0 as i32)?;
                let fits = p.measure(font, &wide).cx <= size;
                if fits || height <= 6 {
                    break (font, wide);
                }
                height -= 1;
            }
        }
        None => {
            let glyph = view.map_or(NO_DEVICE, |v| v.glyph);
            let font = p.font(&ICON_FACES, -(size * 7 / 8), FW_NORMAL.0 as i32)?;
            (font, glyph.to_string().encode_utf16().collect())
        }
    };
    let measured = p.measure(font, &wide);
    if !p.begin(size, size) {
        return None;
    }
    p.draw(
        font,
        (size - measured.cx) / 2,
        (size - measured.cy) / 2,
        &wide,
    );
    let all = Span {
        x0: 0,
        x1: size,
        rgb,
        opacity,
    };
    p.finish(&[all], 0, false);
    p.into_icon()
}

/// Colours of the hover card for the taskbar's theme.
struct CardColors {
    background: [u8; 3],
    border: [u8; 3],
    title: [u8; 3],
    /// The second line's.
    muted: [u8; 3],
}

impl CardColors {
    fn new(light: bool) -> CardColors {
        if light {
            CardColors {
                background: [255, 255, 255],
                border: [200, 200, 200],
                title: [0, 0, 0],
                muted: [96, 96, 96],
            }
        } else {
            CardColors {
                background: [32, 32, 32],
                border: [80, 80, 80],
                title: [255, 255, 255],
                muted: [170, 170, 170],
            }
        }
    }
}

/// The hover card, as compact as a tooltip: `lines` (the first in bold at a tooltip's
/// size, the rest smaller and muted), opaque, in the taskbar's theme; `dpi` scales it.
pub fn card(lines: &[String], dpi: u32, light: bool) -> Option<Painter> {
    dwrite::with(|e| card_direct(e, lines, dpi, light)).or_else(|| card_gdi(lines, dpi, light))
}

/// One line of the card: in font 0 (the title's) or 1, at (`x`, `y`), `size` as measured.
struct Row {
    font: usize,
    text: Vec<u16>,
    x: i32,
    y: i32,
    size: SIZE,
    rgb: [u8; 3],
}

/// The card's rows and size, `measure` giving a text's size in font 0 or 1.
fn card_layout(
    lines: &[String],
    dpi: u32,
    colors: &CardColors,
    mut measure: impl FnMut(usize, &[u16]) -> SIZE,
) -> (Vec<Row>, SIZE) {
    let px = |v: i32| v * dpi as i32 / 96;
    let (pad_x, pad_y) = (px(8), px(5));
    let mut rows = Vec::with_capacity(lines.len());
    let mut y = pad_y;
    let mut bottom = pad_y;
    let mut width = 0;
    for (i, line) in lines.iter().enumerate() {
        let (font, rgb) = if i == 0 {
            (0, colors.title)
        } else {
            (1, colors.muted)
        };
        let text: Vec<u16> = line.encode_utf16().collect();
        let size = measure(font, &text);
        width = width.max(size.cx + 2 * pad_x);
        rows.push(Row {
            font,
            text,
            x: pad_x,
            y,
            size,
            rgb,
        });
        bottom = y + size.cy;
        y = bottom + px(1);
    }
    let size = SIZE {
        cx: width.max(1),
        cy: bottom + pad_y,
    };
    (rows, size)
}

/// The card's fonts: 9 pt bold, a tooltip's size; then 8 pt. `dpi` scales them.
fn card_fonts(dpi: u32) -> [FontSpec; 2] {
    let px = |v: f32| v * dpi as f32 / 96.0;
    [
        FontSpec {
            faces: &[TEXT_FACE],
            size: px(12.0),
            weight: FW_BOLD.0 as i32,
        },
        FontSpec {
            faces: &[TEXT_FACE],
            size: px(11.0),
            weight: FW_NORMAL.0 as i32,
        },
    ]
}

/// [`card`] by DirectWrite. `Ok(None)`: no DIB.
fn card_direct(
    e: &Engine,
    lines: &[String],
    dpi: u32,
    light: bool,
) -> windows::core::Result<Option<Painter>> {
    let colors = CardColors::new(light);
    let fonts = e.fonts(&card_fonts(dpi))?;
    let mut failed = None;
    let (rows, size) = card_layout(lines, dpi, &colors, |font, text| {
        e.measure(&fonts, font, text).unwrap_or_else(|err| {
            failed.get_or_insert(err);
            SIZE::default()
        })
    });
    if let Some(err) = failed {
        return Err(err);
    }
    let Some(mut p) = Painter::new() else {
        return Ok(None);
    };
    if !p.begin(size.cx, size.cy) {
        return Ok(None);
    }
    let fill = Fill {
        border: colors.border,
        inside: colors.background,
    };
    let runs: Vec<Run> = rows
        .iter()
        .map(|row| Run {
            font: row.font,
            text: &row.text,
            x: row.x,
            y: row.y,
            size: row.size,
            rgb: row.rgb,
            opacity: 255,
        })
        .collect();
    e.draw(p.dc, size, &fonts, Some(&fill), &runs)?;
    p.opaque();
    Ok(Some(p))
}

/// [`card`] by GDI.
fn card_gdi(lines: &[String], dpi: u32, light: bool) -> Option<Painter> {
    let px = |v: i32| v * dpi as i32 / 96;
    let colors = CardColors::new(light);
    let mut p = Painter::new()?;
    let title_font = p.font(&[TEXT_FACE], -px(12), FW_BOLD.0 as i32)?;
    let text_font = p.font(&[TEXT_FACE], -px(11), FW_NORMAL.0 as i32)?;
    let fonts = [title_font, text_font];
    let (rows, size) = card_layout(lines, dpi, &colors, |font, text| {
        p.measure(fonts[font], text)
    });
    if !p.begin(size.cx, size.cy) {
        return None;
    }
    let whole = RECT {
        left: 0,
        top: 0,
        right: size.cx,
        bottom: size.cy,
    };
    let inside = RECT {
        left: 1,
        top: 1,
        right: size.cx - 1,
        bottom: size.cy - 1,
    };
    p.fill(&whole, colors.border);
    p.fill(&inside, colors.background);
    for row in &rows {
        // SAFETY: the painter's DC is alive.
        unsafe { SetTextColor(p.dc, colorref(row.rgb)) };
        p.draw(fonts[row.font], row.x, row.y, &row.text);
    }
    p.opaque();
    Some(p)
}

/// Raises every pixel's alpha to at least `floor` (premultiplied: the colour is 0 there).
fn floor_alpha(pixels: &mut [u8], floor: u8) {
    for px in pixels.as_chunks_mut::<4>().0 {
        px[3] = px[3].max(floor);
    }
}

/// Premultiplied BGRA pixels to straight alpha (icons take it).
fn unpremultiply(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a == 0 {
            continue;
        }
        for c in &mut px[..3] {
            *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

fn colorref([r, g, b]: [u8; 3]) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

/// A font owned by a [`Painter`].
#[derive(Clone, Copy)]
pub struct Font(usize);

/// A memory DC with its fonts and, after [`Painter::begin`], a top-down 32-bit DIB.
/// Dropping it restores the DC's original font and bitmap before deleting anything.
pub struct Painter {
    dc: HDC,
    fonts: Vec<HFONT>,
    /// The DC's own font, saved by the first font selection.
    original_font: Option<HGDIOBJ>,
    surface: Option<Surface>,
}

struct Surface {
    bitmap: HBITMAP,
    /// The DC's own bitmap, selected back before `bitmap` is deleted.
    original: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Painter {
    fn new() -> Option<Painter> {
        // SAFETY: no pointers involved.
        let dc = unsafe { CreateCompatibleDC(None) };
        (!dc.is_invalid()).then(|| Painter {
            dc,
            fonts: Vec::new(),
            original_font: None,
            surface: None,
        })
    }

    /// The first of `faces` that GDI provides (else the last one, substituted), `height` px
    /// (negative: character height, positive: cell height), grayscale antialiased.
    fn font(&mut self, faces: &[PCWSTR], height: i32, weight: i32) -> Option<Font> {
        for (i, face) in faces.iter().enumerate() {
            // SAFETY: `face` is a NUL-terminated literal.
            let font = unsafe {
                CreateFontW(
                    height,
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    0,
                    *face,
                )
            };
            if font.is_invalid() {
                continue;
            }
            self.fonts.push(font);
            let handle = Font(self.fonts.len() - 1);
            if i + 1 == faces.len() || self.selected_face_is(handle, *face) {
                return Some(handle);
            }
        }
        None
    }

    /// Whether GDI matched `font` to `face` rather than substituting another face.
    fn selected_face_is(&mut self, font: Font, face: PCWSTR) -> bool {
        self.select(font);
        let mut buf = [0u16; 64];
        // SAFETY: `buf` outlives the call; `face` is a NUL-terminated literal.
        let n = unsafe { GetTextFaceW(self.dc, Some(&mut buf)) };
        let got = String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0)]);
        let got = got.trim_end_matches('\0');
        // SAFETY: as above.
        unsafe { face.to_string() }.is_ok_and(|want| got.eq_ignore_ascii_case(&want))
    }

    fn select(&mut self, font: Font) {
        // SAFETY: the DC and the font are alive.
        let previous = unsafe { SelectObject(self.dc, self.fonts[font.0].into()) };
        self.original_font.get_or_insert(previous);
    }

    fn measure(&mut self, font: Font, text: &[u16]) -> SIZE {
        self.select(font);
        let mut size = SIZE::default();
        // SAFETY: `size` outlives the call.
        let _ = unsafe { GetTextExtentPoint32W(self.dc, text, &mut size) };
        size
    }

    /// Selects a zeroed `width`×`height` DIB to draw on.
    fn begin(&mut self, width: i32, height: i32) -> bool {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: `info` and `bits` outlive the call.
        let Ok(bitmap) =
            (unsafe { CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) })
        else {
            return false;
        };
        // SAFETY: the DC and the bitmap are alive.
        let original = unsafe { SelectObject(self.dc, bitmap.into()) };
        // SAFETY: no pointers involved.
        unsafe {
            SetBkMode(self.dc, TRANSPARENT);
            SetTextColor(self.dc, COLORREF(0x00FF_FFFF));
        }
        self.surface = Some(Surface {
            bitmap,
            original,
            bits: bits.cast(),
            width,
            height,
        });
        true
    }

    fn draw(&mut self, font: Font, x: i32, y: i32, text: &[u16]) {
        self.select(font);
        // SAFETY: no pointers outlive the call.
        let _ = unsafe { TextOutW(self.dc, x, y, text) };
    }

    /// Fills `rect` with `rgb` (alpha left to [`Painter::opaque`]).
    fn fill(&mut self, rect: &RECT, rgb: [u8; 3]) {
        // SAFETY: the DC is alive; the brush is not selected anywhere and is deleted after
        // use.
        unsafe {
            let brush = CreateSolidBrush(colorref(rgb));
            FillRect(self.dc, rect, brush);
            let _ = DeleteObject(brush.into());
        }
    }

    /// Flushes GDI and returns the DIB's pixels (BGRA rows), if one is selected.
    fn pixels(&mut self) -> Option<&mut [u8]> {
        let s = self.surface.as_ref()?;
        // SAFETY: no pointers involved.
        let _ = unsafe { GdiFlush() };
        let len = (s.width * s.height * 4) as usize;
        // SAFETY: the DIB holds `width * height` 4-byte pixels and stays alive and unaliased
        // while GDI is flushed and nothing else draws (the slice borrows the painter).
        Some(unsafe { std::slice::from_raw_parts_mut(s.bits, len) })
    }

    /// Flushes GDI and colours the pixels (see [`colorize`]).
    fn finish(&mut self, spans: &[Span], floor: u8, premultiplied: bool) {
        let Some(width) = self.surface.as_ref().map(|s| s.width as usize) else {
            return;
        };
        if let Some(pixels) = self.pixels() {
            colorize(pixels, width, spans, floor, premultiplied);
        }
    }

    /// Flushes GDI and makes every pixel opaque: GDI leaves alpha 0 where it draws, and
    /// `UpdateLayeredWindow` would show nothing there.
    fn opaque(&mut self) {
        if let Some(pixels) = self.pixels() {
            for px in pixels.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
    }

    pub fn dc(&self) -> HDC {
        self.dc
    }

    pub fn size(&self) -> SIZE {
        self.surface.as_ref().map_or(SIZE::default(), |s| SIZE {
            cx: s.width,
            cy: s.height,
        })
    }

    /// An icon with the DIB as its colour (with alpha) and an empty mask.
    fn into_icon(mut self) -> Option<HICON> {
        let s = self.surface.take()?;
        // SAFETY: handles are alive; the bitmap is deselected before the icon copies it.
        unsafe {
            SelectObject(self.dc, s.original);
            let mask = CreateBitmap(s.width, s.height, 1, 1, None);
            let info = ICONINFO {
                fIcon: true.into(),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: s.bitmap,
            };
            let icon = CreateIconIndirect(&info).ok();
            let _ = DeleteObject(mask.into());
            let _ = DeleteObject(s.bitmap.into());
            icon
        }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        // SAFETY: every handle was created by this painter; originals are selected back
        // before the objects they replaced are deleted.
        unsafe {
            if let Some(s) = self.surface.take() {
                SelectObject(self.dc, s.original);
                let _ = DeleteObject(s.bitmap.into());
            }
            if let Some(font) = self.original_font {
                SelectObject(self.dc, font);
            }
            for font in self.fonts.drain(..) {
                let _ = DeleteObject(font.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tray::tests::keyed;

    const RED: [u8; 3] = [255, 0, 0];

    fn span(x0: i32, x1: i32, opacity: u8) -> Span {
        Span {
            x0,
            x1,
            rgb: RED,
            opacity,
        }
    }

    #[test]
    fn coverage_becomes_premultiplied_alpha() {
        // Two pixels: full coverage, half coverage (stored as grey in B, G, R).
        let mut px = [255, 255, 255, 0, 128, 128, 128, 0];
        colorize(&mut px, 2, &[span(0, 2, 255)], 0, true);
        assert_eq!(px, [0, 0, 255, 255, 0, 0, 128, 128]);
    }

    #[test]
    fn straight_alpha_keeps_full_colour() {
        let mut px = [128, 128, 128, 0];
        colorize(&mut px, 1, &[span(0, 1, 255)], 0, false);
        assert_eq!(px, [0, 0, 255, 128]);
    }

    #[test]
    fn opacity_scales_alpha() {
        let mut px = [255, 255, 255, 0];
        colorize(&mut px, 1, &[span(0, 1, 102)], 0, true);
        assert_eq!(px, [0, 0, 102, 102]);
    }

    #[test]
    fn floor_alpha_is_black_and_outside_spans_too() {
        // Pixel 0 has no coverage inside the span, pixel 1 has coverage outside any span.
        let mut px = [0, 0, 0, 0, 255, 255, 255, 0];
        colorize(&mut px, 2, &[span(0, 1, 255)], 1, true);
        assert_eq!(px, [0, 0, 0, 1, 0, 0, 0, 1]);
    }

    #[test]
    fn spans_apply_per_column_on_every_row() {
        // 2×2: left column red span, right column white span.
        let white = Span {
            rgb: [255, 255, 255],
            ..span(1, 2, 255)
        };
        let mut px = [255; 16];
        colorize(&mut px, 2, &[span(0, 1, 255), white], 0, true);
        assert_eq!(
            px,
            [
                0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 255, 255, 255, 255, 255, 255
            ]
        );
    }

    #[test]
    fn device_columns_cover_the_strip() {
        assert_eq!(
            device_columns(&[(4, 50), (60, 110), (121, 170)], 174),
            [(0, 55), (55, 115), (115, 174)]
        );
        assert_eq!(device_columns(&[(4, 50)], 54), [(0, 54)]);
        assert!(device_columns(&[], 20).is_empty());
    }

    /// The layout with stand-in sizes: 10 px a character; 20 px tall icons, 16 px text,
    /// 12 px for the rest.
    fn laid(views: &[DeviceView], height: i32) -> Layout {
        layout(views, 96, height, false, |face, text| SIZE {
            cx: 10 * text.len() as i32,
            cy: match face {
                Face::Icon => 20,
                Face::Text => 16,
                _ => 12,
            },
        })
    }

    fn part<'a>(layout: &'a Layout, text: &str) -> Option<&'a Part> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        layout.parts.iter().find(|p| p.text == wide)
    }

    #[test]
    fn full_on_power_shows_check() {
        let full = DeviceView {
            full: true,
            ..keyed("M", "100%")
        };
        let l = laid(&[full], 40);
        let check = part(&l, &CHECK.to_string()).expect("a check");
        let level = part(&l, "100%").unwrap();
        assert!(check.x > level.x, "after the level, where the bolt goes");
        assert!(part(&l, &BOLT.to_string()).is_none());
        // Charging keeps its bolt.
        let charging = DeviceView {
            charging: true,
            ..keyed("M", "80%")
        };
        let l = laid(&[charging], 40);
        assert!(part(&l, &BOLT.to_string()).is_some());
        assert!(part(&l, &CHECK.to_string()).is_none());
    }

    #[test]
    fn caption_under_the_glyph_when_it_fits() {
        let v = DeviceView {
            caption: Some("SUP".into()),
            ..keyed("M", "50%")
        };
        let l = laid(std::slice::from_ref(&v), 40);
        let glyph = part(&l, &v.glyph.to_string()).unwrap();
        let caption = part(&l, "SUP").expect("a caption");
        // The glyph (20 px) and the caption (12 px) stacked, centred as one on 40 px.
        assert_eq!((glyph.y, caption.y), (4, 24));
        // The caption is wider (30 px): the glyph (10 px) is centred above it.
        assert_eq!((caption.x, glyph.x), (4, 14));
        assert_eq!(part(&l, "50%").unwrap().x, 4 + 30 + 4);
        assert_eq!(l.drawn, [(4, 38 + 30)]);
    }

    #[test]
    fn caption_after_the_glyph_on_a_short_taskbar() {
        let v = DeviceView {
            caption: Some("SUP".into()),
            ..keyed("M", "50%")
        };
        let l = laid(std::slice::from_ref(&v), 30);
        let glyph = part(&l, &v.glyph.to_string()).unwrap();
        let caption = part(&l, "SUP").expect("a caption");
        assert_eq!((glyph.x, caption.x), (4, 4 + 10 + 2));
        assert_eq!(caption.y, (30 - 12) / 2);
        assert_eq!(part(&l, "50%").unwrap().x, 16 + 30 + 4);
    }

    #[test]
    fn floor_raises_only_lower_alpha() {
        let mut px = [0, 0, 0, 0, 10, 20, 30, 40, 0, 0, 0, 1];
        floor_alpha(&mut px, 1);
        assert_eq!(px, [0, 0, 0, 1, 10, 20, 30, 40, 0, 0, 0, 1]);
    }

    #[test]
    fn unpremultiply_restores_straight_colour() {
        // Half-covered red, opaque grey, transparent.
        let mut px = [0, 0, 128, 128, 90, 90, 90, 255, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [0, 0, 255, 128, 90, 90, 90, 255, 0, 0, 0, 0]);
        // Rounded to the nearest, never above 255.
        let mut px = [1, 2, 3, 3];
        unpremultiply(&mut px);
        assert_eq!(px, [85, 170, 255, 3]);
    }

    fn strip_pixels(dpi: u32, light: bool) -> Vec<u8> {
        let views = [keyed("H", "43%"), keyed("M", "76%")];
        let (mut p, columns) =
            widget_strip(&views, dpi, 48 * dpi as i32 / 96, light).expect("a strip");
        assert_eq!(columns.len(), 2);
        p.pixels().expect("pixels").to_vec()
    }

    fn directwrite_on() -> bool {
        dwrite::with(|_| Ok(Some(()))).is_some()
    }

    #[test]
    fn direct_strip_is_premultiplied_and_clickable_everywhere() {
        assert!(
            directwrite_on(),
            "DirectWrite is available on Windows 10 and 11"
        );
        for (dpi, light) in [(144, false), (96, true)] {
            let px = strip_pixels(dpi, light);
            let pixels = px.as_chunks::<4>().0;
            assert!(pixels.iter().all(|p| p[3] >= 1), "every pixel takes clicks");
            assert!(
                pixels.iter().all(|p| p[..3].iter().all(|&c| c <= p[3])),
                "premultiplied"
            );
            assert!(pixels.iter().any(|p| p[3] == 255), "text is drawn");
            // Smooth edges: many partial coverages.
            let partial: std::collections::BTreeSet<u8> = pixels
                .iter()
                .map(|p| p[3])
                .filter(|&a| a > 1 && a < 255)
                .collect();
            assert!(partial.len() > 8, "{} partial alphas", partial.len());
        }
    }

    #[test]
    fn direct_card_is_opaque_and_icons_draw() {
        assert!(directwrite_on());
        let lines = [
            "PRO X Wireless Gaming Headset · 43%".to_owned(),
            "just now".to_owned(),
        ];
        let mut p = card(&lines, 144, false).expect("a card");
        assert!(
            p.pixels()
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[3] == 255)
        );
        for view in [Some(keyed("H", "43%")), None] {
            let icon = tray_icon(view.as_ref(), 16, false).expect("an icon");
            // SAFETY: the icon was made here and is not used elsewhere.
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyIcon(icon) };
        }
    }

    #[test]
    fn gdi_draws_when_directwrite_is_off() {
        // This test's thread only.
        dwrite::shutdown();
        assert!(!directwrite_on());
        let px = strip_pixels(96, false);
        assert!(px.as_chunks::<4>().0.iter().all(|p| p[3] >= 1));
        let lines = ["G305 · 80%".to_owned(), "just now".to_owned()];
        assert!(card(&lines, 96, true).is_some());
        let icon = tray_icon(Some(&keyed("H", "43%")), 16, true).expect("an icon");
        // SAFETY: the icon was made here and is not used elsewhere.
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyIcon(icon) };
    }
}
