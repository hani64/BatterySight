//! Text by DirectWrite, drawn by Direct2D into a painter's DIB (`render`): grayscale
//! antialiasing with DirectWrite's gamma and contrast, and correct premultiplied alpha, so
//! it is smooth on any taskbar and on any monitor (no subpixel order to get wrong).
//!
//! The factories, the system font collection and a software render target are made once
//! per thread ([`init`]: on the UI thread at start, before any widget shares Explorer's
//! input queue, with a little text drawn so fonts and rasteriser load then, as loading them
//! may read from disk) and released by [`shutdown`]. A paint makes only its text formats,
//! layouts and brush, and binds the target to the painter's DC: all in-process, nothing
//! waits on another thread. Should a paint fail, GDI draws it; a lost render target is
//! made again for the next paint, any other failure turns DirectWrite off for the rest of
//! the session (traced once).

use std::cell::RefCell;

use batterysight_win::trace::ui_line;
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection,
    IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HDC;
use windows::core::{BOOL, PCWSTR, Result, w};

thread_local! {
    static ENGINE: RefCell<State> = const { RefCell::new(State::Untried) };
}

enum State {
    Untried,
    Ready(Engine),
    /// Failed, or shut down: GDI draws.
    Off,
}

/// Direct2D and DirectWrite, for one thread.
pub struct Engine {
    dwrite: IDWriteFactory,
    fonts: IDWriteFontCollection,
    target: ID2D1DCRenderTarget,
}

/// A font: the first of `faces` the system has (else the last), `size` px per em, `weight`
/// as GDI's `FW_*`.
#[derive(Clone, Copy)]
pub struct FontSpec {
    pub faces: &'static [PCWSTR],
    pub size: f32,
    pub weight: i32,
}

/// Text formats for [`FontSpec`]s, by index.
pub struct Fonts(Vec<IDWriteTextFormat>);

/// A text to draw: in font `font`, at (`x`, `y`) in a box of `size` (as measured), in
/// `rgb` at `opacity`.
pub struct Run<'a> {
    pub font: usize,
    pub text: &'a [u16],
    pub x: i32,
    pub y: i32,
    pub size: SIZE,
    pub rgb: [u8; 3],
    pub opacity: u8,
}

/// The card's opaque background: a 1 px `border` around `inside`.
pub struct Fill {
    pub border: [u8; 3],
    pub inside: [u8; 3],
}

/// Makes this thread's engine, if not tried yet, and draws a little with it, so its fonts
/// and rasteriser load now (before any widget exists, on the UI thread) rather than in the
/// first paint.
pub fn init() {
    let untried = ENGINE.with_borrow(|s| matches!(s, State::Untried));
    if untried {
        let state = match Engine::new().and_then(|e| super::render::warm_up(&e).map(|()| e)) {
            Ok(engine) => State::Ready(engine),
            Err(e) => {
                off(&e);
                State::Off
            }
        };
        ENGINE.set(state);
    }
}

/// Releases this thread's engine; GDI draws from now on.
pub fn shutdown() {
    ENGINE.set(State::Off);
}

/// Runs `f` with this thread's engine (made first if need be). `None` while DirectWrite is
/// off or when `f` gives up or fails ([`failed`]).
pub fn with<R>(f: impl FnOnce(&Engine) -> Result<Option<R>>) -> Option<R> {
    init();
    let result = ENGINE.with_borrow(|s| match s {
        State::Ready(engine) => Some(f(engine)),
        _ => None,
    })?;
    result.unwrap_or_else(|e| {
        failed(&e);
        None
    })
}

/// A paint failed with `e` (GDI draws it): a lost render target is rebuilt for the next
/// paint; any other error turns DirectWrite off for the session.
fn failed(e: &windows::core::Error) {
    if e.code() == D2DERR_RECREATE_TARGET {
        ENGINE.set(State::Untried);
    } else {
        off(e);
        shutdown();
    }
}

fn off(e: &windows::core::Error) {
    ui_line(&format!("text: DirectWrite off, GDI draws ({e})"));
}

impl Engine {
    fn new() -> Result<Engine> {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            // Pixels, not DIPs: the painter's sizes already follow the DPI.
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        // SAFETY: plain factory calls; `props` and `fonts` outlive the calls that read them.
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut fonts = None;
            dwrite.GetSystemFontCollection(&mut fonts, false)?;
            let fonts = fonts.ok_or_else(windows::core::Error::empty)?;
            let target = factory.CreateDCRenderTarget(&props)?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Ok(Engine {
                dwrite,
                fonts,
                target,
            })
        }
    }

    /// Text formats for `specs`, in their order.
    pub fn fonts(&self, specs: &[FontSpec]) -> Result<Fonts> {
        let mut formats = Vec::with_capacity(specs.len());
        for spec in specs {
            let face = self.face(spec.faces)?;
            // SAFETY: `face` and the locale are NUL-terminated strings alive for the call.
            let format = unsafe {
                self.dwrite.CreateTextFormat(
                    face,
                    &self.fonts,
                    DWRITE_FONT_WEIGHT(spec.weight),
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    spec.size,
                    w!("en-us"),
                )?
            };
            // One line, however narrow the box drawn into.
            // SAFETY: plain call on a live format.
            unsafe { format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)? };
            formats.push(format);
        }
        Ok(Fonts(formats))
    }

    /// The first of `faces` the system has, else the last.
    fn face(&self, faces: &[PCWSTR]) -> Result<PCWSTR> {
        for &face in faces {
            let mut index = 0;
            let mut exists = BOOL(0);
            // SAFETY: `face` is NUL-terminated; `index` and `exists` outlive the call.
            unsafe { self.fonts.FindFamilyName(face, &mut index, &mut exists)? };
            if exists.as_bool() {
                return Ok(face);
            }
        }
        faces
            .last()
            .copied()
            .ok_or_else(windows::core::Error::empty)
    }

    /// The box `text` takes in font `font` of `fonts`: whole pixels, rounded up.
    pub fn measure(&self, fonts: &Fonts, font: usize, text: &[u16]) -> Result<SIZE> {
        let mut m = DWRITE_TEXT_METRICS::default();
        // SAFETY: `text` and `m` outlive the calls.
        unsafe {
            let layout =
                self.dwrite
                    .CreateTextLayout(text, &fonts.0[font], 100_000.0, 100_000.0)?;
            layout.GetMetrics(&mut m)?;
        }
        Ok(SIZE {
            cx: m.widthIncludingTrailingWhitespace.ceil() as i32,
            cy: m.height.ceil() as i32,
        })
    }

    /// Draws `runs` into the `size` DIB selected in `dc`: over `fill` (opaque), or over a
    /// transparent background.
    pub fn draw(
        &self,
        dc: HDC,
        size: SIZE,
        fonts: &Fonts,
        fill: Option<&Fill>,
        runs: &[Run],
    ) -> Result<()> {
        let whole = RECT {
            left: 0,
            top: 0,
            right: size.cx,
            bottom: size.cy,
        };
        let t = &self.target;
        // SAFETY: `dc` holds a live DIB of `size`; every pointer outlives the call that
        // reads it; drawing ends (EndDraw) on every path once it has begun.
        unsafe {
            t.BindDC(dc, &whole)?;
            let brush = t.CreateSolidColorBrush(&color([0; 3], 0), None)?;
            t.BeginDraw();
            match fill {
                Some(fill) => {
                    t.Clear(Some(&color(fill.border, 255)));
                    brush.SetColor(&color(fill.inside, 255));
                    let inside = D2D_RECT_F {
                        left: 1.0,
                        top: 1.0,
                        right: (size.cx - 1) as f32,
                        bottom: (size.cy - 1) as f32,
                    };
                    t.FillRectangle(&inside, &brush);
                }
                None => t.Clear(Some(&color([0; 3], 0))),
            }
            for run in runs {
                brush.SetColor(&color(run.rgb, run.opacity));
                let at = D2D_RECT_F {
                    left: run.x as f32,
                    top: run.y as f32,
                    right: (run.x + run.size.cx) as f32,
                    bottom: (run.y + run.size.cy) as f32,
                };
                t.DrawText(
                    run.text,
                    &fonts.0[run.font],
                    &at,
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            t.EndDraw(None, None)
        }
    }
}

fn color([r, g, b]: [u8; 3], a: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: f32::from(a) / 255.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, E_FAIL};

    fn state() -> &'static str {
        ENGINE.with_borrow(|s| match s {
            State::Untried => "untried",
            State::Ready(_) => "ready",
            State::Off => "off",
        })
    }

    #[test]
    fn a_lost_target_is_rebuilt_other_errors_switch_off() {
        // This test's thread only.
        init();
        assert_eq!(state(), "ready");
        failed(&windows::core::Error::from_hresult(D2DERR_RECREATE_TARGET));
        assert_eq!(state(), "untried");
        // The next paint rebuilds it.
        assert!(with(|_| Ok(Some(()))).is_some());
        assert_eq!(state(), "ready");
        failed(&windows::core::Error::from_hresult(E_FAIL));
        assert_eq!(state(), "off");
        assert!(with(|_| Ok(Some(()))).is_none());
    }
}
