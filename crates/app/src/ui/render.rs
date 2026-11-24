//! GDI text rendering into 32-bit bitmaps with per-pixel alpha.
//!
//! Text is drawn white on black with grayscale antialiasing; [`colorize`] then turns the
//! coverage into alpha and applies each part's colour.

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, SIZE};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateBitmap,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, FW_NORMAL, FW_SEMIBOLD, GdiFlush, GetTextExtentPoint32W, GetTextFaceW, HBITMAP,
    HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    TextOutW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};
use windows::core::{PCWSTR, w};

use super::DeviceView;

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
/// height. Every pixel gets at least alpha 1, so the whole strip takes clicks.
pub fn widget_strip(views: &[DeviceView], dpi: u32, height: i32, light: bool) -> Option<Painter> {
    let px = |v: i32| v * dpi as i32 / 96;
    let palette = Palette::new(light);
    let mut p = Painter::new()?;
    let text_font = p.font(&[TEXT_FACE], -px(12), FW_NORMAL.0 as i32)?;
    let icon_font = p.font(&ICON_FACES, -px(15), FW_NORMAL.0 as i32)?;
    let bolt_font = p.font(&ICON_FACES, -px(11), FW_NORMAL.0 as i32)?;

    // (font, text, x, colour, opacity)
    let mut parts = Vec::new();
    let mut x = px(4);
    let mut place = |p: &mut Painter, font, text: String, rgb, opacity, gap_after| {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let size = p.measure(font, &wide);
        parts.push((font, wide, x, size, rgb, opacity));
        x += size.cx + gap_after;
    };
    if views.is_empty() {
        place(&mut p, icon_font, NO_DEVICE.into(), palette.text, 255, 0);
    }
    for (i, v) in views.iter().enumerate() {
        let opacity = if v.dimmed { DIMMED } else { 255 };
        let level = if v.low { palette.low } else { palette.text };
        let last = i + 1 == views.len();
        let gap = if last { 0 } else { px(10) };
        place(
            &mut p,
            icon_font,
            v.glyph.into(),
            palette.text,
            opacity,
            px(4),
        );
        if v.charging {
            place(&mut p, text_font, v.text.clone(), level, opacity, px(2));
            place(&mut p, bolt_font, BOLT.into(), palette.text, opacity, gap);
        } else {
            place(&mut p, text_font, v.text.clone(), level, opacity, gap);
        }
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
    Some(p)
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

    /// Flushes GDI and colours the pixels (see [`colorize`]).
    fn finish(&mut self, spans: &[Span], floor: u8, premultiplied: bool) {
        let Some(s) = &self.surface else { return };
        // SAFETY: no pointers involved.
        let _ = unsafe { GdiFlush() };
        let len = (s.width * s.height * 4) as usize;
        // SAFETY: the DIB holds `width * height` 4-byte pixels and stays alive and unaliased
        // while GDI is flushed and nothing else draws.
        let pixels = unsafe { std::slice::from_raw_parts_mut(s.bits, len) };
        colorize(pixels, s.width as usize, spans, floor, premultiplied);
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
}
