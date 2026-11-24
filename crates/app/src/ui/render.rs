//! GDI text rendering into 32-bit bitmaps with per-pixel alpha.
//!
//! Text is drawn white on black with grayscale antialiasing; [`colorize`] then turns the
//! coverage into alpha and applies each part's colour. The hover card ([`card`]) is drawn
//! in its colours on an opaque background instead.

use std::ffi::c_void;
use std::time::Duration;

use batterysight_core::history::Sample;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateBitmap,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, FW_NORMAL, FW_SEMIBOLD, FillRect, GdiFlush,
    GetTextExtentPoint32W, GetTextFaceW, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};
use windows::core::{PCWSTR, w};

use super::{DeviceView, chart};

/// Icon fonts, best first: Windows 11 ships Segoe Fluent Icons, Windows 10 Segoe MDL2 Assets
/// (same code points for the glyphs used here).
const ICON_FACES: [PCWSTR; 2] = [w!("Segoe Fluent Icons"), w!("Segoe MDL2 Assets")];
const TEXT_FACE: PCWSTR = w!("Segoe UI");
/// Shown after the level of a charging device.
const BOLT: char = '\u{E945}';
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

/// The widget's strip for `views`: `dpi` scales it, `height` is the taskbar's client
/// height. Every pixel gets at least alpha 1, so the whole strip takes clicks. Also returns
/// each view's columns of the strip ([`device_columns`]), in view order.
pub fn widget_strip(
    views: &[DeviceView],
    dpi: u32,
    height: i32,
    light: bool,
) -> Option<(Painter, Vec<(i32, i32)>)> {
    let px = |v: i32| v * dpi as i32 / 96;
    let palette = Palette::new(light);
    let mut p = Painter::new()?;
    let text_font = p.font(&[TEXT_FACE], -px(12), FW_NORMAL.0 as i32)?;
    let icon_font = p.font(&ICON_FACES, -px(15), FW_NORMAL.0 as i32)?;
    let bolt_font = p.font(&ICON_FACES, -px(11), FW_NORMAL.0 as i32)?;

    // (font, text, x, colour, opacity)
    let mut parts = Vec::new();
    let mut x = px(4);
    // Returns the part's extent.
    let mut place = |p: &mut Painter, font, text: String, rgb, opacity, gap_after| {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let size = p.measure(font, &wide);
        parts.push((font, wide, x, size, rgb, opacity));
        let extent = (x, x + size.cx);
        x += size.cx + gap_after;
        extent
    };
    if views.is_empty() {
        place(&mut p, icon_font, NO_DEVICE.into(), palette.text, 255, 0);
    }
    // Each view's drawn extent.
    let mut drawn = Vec::with_capacity(views.len());
    for (i, v) in views.iter().enumerate() {
        let opacity = if v.dimmed { DIMMED } else { 255 };
        let level = if v.low { palette.low } else { palette.text };
        let last = i + 1 == views.len();
        let gap = if last { 0 } else { px(10) };
        let (start, _) = place(
            &mut p,
            icon_font,
            v.glyph.into(),
            palette.text,
            opacity,
            px(4),
        );
        let (_, end) = if v.charging {
            place(&mut p, text_font, v.text.clone(), level, opacity, px(2));
            place(&mut p, bolt_font, BOLT.into(), palette.text, opacity, gap)
        } else {
            place(&mut p, text_font, v.text.clone(), level, opacity, gap)
        };
        drawn.push((start, end));
    }
    let width = x + px(4);
    let height = height.max(1);
    if !p.begin(width, height) {
        return None;
    }
    let mut spans = Vec::new();
    for (font, wide, x, size, rgb, opacity) in &parts {
        p.draw(*font, *x, (height - size.cy) / 2, wide);
        spans.push(Span {
            x0: *x,
            x1: x + size.cx,
            rgb: *rgb,
            opacity: *opacity,
        });
    }
    p.finish(&spans, 1, true);
    Some((p, device_columns(&drawn, width)))
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
    let palette = Palette::new(light);
    let (text, rgb, opacity) = match view {
        Some(v) => (
            v.text.strip_suffix('%').map(str::to_owned),
            if v.low { palette.low } else { palette.text },
            if v.dimmed { DIMMED } else { 255 },
        ),
        None => (None, palette.text, 255),
    };
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

/// Colours of the hover card for the taskbar's theme; its background is the chart's.
struct CardColors {
    background: [u8; 3],
    border: [u8; 3],
    title: [u8; 3],
    text: [u8; 3],
}

impl CardColors {
    fn new(light: bool) -> CardColors {
        if light {
            CardColors {
                background: [255, 255, 255],
                border: [200, 200, 200],
                title: [0, 0, 0],
                text: [64, 64, 64],
            }
        } else {
            CardColors {
                background: [32, 32, 32],
                border: [80, 80, 80],
                title: [255, 255, 255],
                text: [220, 220, 220],
            }
        }
    }
}

/// The hover card: `lines` (the first, the device's name, emphasised) above the chart of
/// `samples` over `window`, opaque, in the taskbar's theme; `dpi` scales it.
pub fn card(
    lines: &[String],
    samples: &[Sample],
    window: Duration,
    dpi: u32,
    light: bool,
) -> Option<Painter> {
    let px = |v: i32| v * dpi as i32 / 96;
    let colors = CardColors::new(light);
    let mut p = Painter::new()?;
    let title_font = p.font(&[TEXT_FACE], -px(14), FW_SEMIBOLD.0 as i32)?;
    let text_font = p.font(&[TEXT_FACE], -px(12), FW_NORMAL.0 as i32)?;
    let chart_font = p.font(&[TEXT_FACE], -px(11), FW_NORMAL.0 as i32)?;
    let pad = px(12);
    // (font, text, y, colour)
    let mut rows = Vec::with_capacity(lines.len());
    let mut y = pad;
    let mut width = px(300);
    for (i, line) in lines.iter().enumerate() {
        let (font, rgb, gap) = if i == 0 {
            (title_font, colors.title, px(4))
        } else {
            (text_font, colors.text, px(2))
        };
        let wide: Vec<u16> = line.encode_utf16().collect();
        let size = p.measure(font, &wide);
        width = width.max(size.cx + 2 * pad);
        rows.push((font, wide, y, rgb));
        y += size.cy + gap;
    }
    let chart = RECT {
        left: pad / 2,
        top: y + px(6),
        right: width - pad / 2,
        bottom: y + px(126),
    };
    let height = chart.bottom + pad / 2;
    if !p.begin(width, height) {
        return None;
    }
    let whole = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    let inside = RECT {
        left: 1,
        top: 1,
        right: width - 1,
        bottom: height - 1,
    };
    p.fill(&whole, colors.border);
    p.fill(&inside, colors.background);
    for (font, wide, y, rgb) in &rows {
        // SAFETY: the painter's DC is alive.
        unsafe { SetTextColor(p.dc, colorref(*rgb)) };
        p.draw(*font, pad, *y, wide);
    }
    p.select(chart_font);
    chart::paint(p.dc, chart, samples, window, light);
    p.opaque();
    Some(p)
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
}
