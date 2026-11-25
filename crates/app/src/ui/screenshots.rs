//! Renders the README's screenshots with the app's own drawing code, at 2× (192 DPI), from
//! demo devices and a demo day of history. Not a test of behaviour: run it on purpose with
//!
//! ```text
//! cargo test -p batterysight screenshots -- --ignored
//! ```
//!
//! It writes raw images to `target/screenshots` (each `name.bgra`, premultiplied BGRA rows,
//! with `name.txt` holding `width height`); `assets/screenshots/compose.py` turns them into
//! the PNGs the README shows.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use batterysight_core::history::Sample;
use batterysight_core::model::{
    Charge, DeviceKey, DeviceKind, DeviceState, Level, Presence, Source, Verification, Via,
};
use batterysight_core::settings::Threshold;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    FW_NORMAL, GdiFlush, GetCurrentObject, GetObjectW, HDC, OBJ_BITMAP, SelectObject,
};
use windows::core::w;

use super::{chart, render, view};

/// 2× the 96-DPI baseline: crisp on high-density screens and when scaled down.
const DPI: u32 = 192;
/// The Windows 11 taskbar's height at 96 DPI.
const TASKBAR_PX: i32 = 48;
const HOUR: u64 = 3_600_000;
const MIN: u64 = 60_000;

fn device(name: &str, kind: DeviceKind, level: u8, charge: Charge, reports: bool) -> DeviceState {
    DeviceState {
        key: DeviceKey(format!("demo:{name}")),
        name: name.into(),
        kind,
        via: Via::Receiver,
        level: Some(Level::Percent(level)),
        millivolts: None,
        charge,
        presence: Presence::Online,
        updated_ms: super::unix_now_ms(),
        source: Source::Push,
        verification: Verification::HardwareTested,
        poll_min_ms: None,
        reports_changes: reports,
    }
}

/// The demo devices on the taskbar: a headset, a mouse and a charging controller.
fn devices() -> Vec<DeviceState> {
    vec![
        device(
            "PRO X Wireless Gaming Headset",
            DeviceKind::Headset,
            88,
            Charge::Discharging,
            false,
        ),
        device(
            "PRO X Superlight",
            DeviceKind::Mouse,
            70,
            Charge::Discharging,
            true,
        ),
        device(
            "Xbox Wireless Controller",
            DeviceKind::Controller,
            45,
            Charge::Charging,
            true,
        ),
    ]
}

/// A mouse's last 24 hours: charged overnight from 34 % to full, then used through the day
/// down to 70 % now, with a reading at each 1 % step.
fn demo_day(now: u64) -> Vec<Sample> {
    let key = DeviceKey("demo:mouse".into());
    let sample = |at: u64, level: u8, charge: Charge| Sample {
        at_unix_ms: at,
        key: key.clone(),
        level: Some(Level::Percent(level)),
        charge,
        presence: Presence::Online,
        millivolts: None,
        source: Source::Push,
    };
    let start = now - 24 * HOUR;
    let mut samples = Vec::new();
    // 0–1.5 h: the evening before, in use, 40 % down to 34 %.
    for (i, level) in (34..=40).rev().enumerate() {
        samples.push(sample(
            start + i as u64 * 15 * MIN,
            level,
            Charge::Discharging,
        ));
    }
    // 1.5–4 h: on the cable, 34 % to 100 %.
    let charge_from = start + 90 * MIN;
    for (i, level) in (34..=100).step_by(3).enumerate() {
        samples.push(sample(
            charge_from + i as u64 * 7 * MIN,
            level,
            Charge::Charging,
        ));
    }
    samples.push(sample(start + 4 * HOUR, 100, Charge::Full));
    // 5–24 h: unplugged and used, 99 % down to 70 % now: one reading per 1 % step, spread
    // over the day and closer together in the busy afternoon.
    let used_from = start + 5 * HOUR;
    let weights: Vec<u64> = (0..30)
        .map(|i| if (8..20).contains(&i) { 2 } else { 3 })
        .collect();
    let total: u64 = weights.iter().sum();
    let mut at = used_from;
    for (level, weight) in (70..100).rev().zip(&weights) {
        at += (now - used_from) * weight / total;
        samples.push(sample(at, level, Charge::Discharging));
    }
    samples
}

/// Writes `pixels` (BGRA rows) as `name.bgra` with `name.txt` = `width height`.
fn save(dir: &Path, name: &str, width: i32, height: i32, pixels: &[u8]) {
    fs::write(dir.join(format!("{name}.bgra")), pixels).unwrap();
    fs::write(dir.join(format!("{name}.txt")), format!("{width} {height}")).unwrap();
}

/// The pixels of the DIB selected in `dc`.
fn dib_pixels(dc: HDC) -> (i32, i32, Vec<u8>) {
    // SAFETY: `dc` is a live memory DC with a 32-bit top-down DIB section selected; the
    // section's bits stay alive while it is selected, and GDI is flushed before reading.
    unsafe {
        let _ = GdiFlush();
        let bitmap = GetCurrentObject(dc, OBJ_BITMAP);
        let mut info = BITMAP::default();
        GetObjectW(
            bitmap,
            size_of::<BITMAP>() as i32,
            Some((&raw mut info).cast()),
        );
        let len = (info.bmWidthBytes * info.bmHeight) as usize;
        let bits = std::slice::from_raw_parts(info.bmBits.cast::<u8>(), len);
        (info.bmWidth, info.bmHeight, bits.to_vec())
    }
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "renders the README's screenshots; run on purpose"]
fn screenshots() {
    let dir = out_dir();
    let devices = devices();
    let views: Vec<_> = devices
        .iter()
        .map(|s| view(s, Threshold::Percent(15), None, String::new()))
        .collect();
    let height = TASKBAR_PX * DPI as i32 / 96;
    for (light, theme) in [(false, "dark"), (true, "light")] {
        let (strip, _) = render::widget_strip(&views, DPI, height, light).unwrap();
        let (w, h, pixels) = dib_pixels(strip.dc());
        save(&dir, &format!("widget-{theme}"), w, h, &pixels);

        let mouse = &devices[1];
        let lines = super::card::card_text(
            mouse,
            Some(Duration::from_secs(46 * 3600)),
            super::unix_now_ms(),
        );
        let card = render::card(&lines, DPI, light).unwrap();
        let (w, h, pixels) = dib_pixels(card.dc());
        save(&dir, &format!("card-{theme}"), w, h, &pixels);
    }

    // The history chart, as the window draws it at 192 DPI.
    let now = super::unix_now_ms();
    let day = Duration::from_secs(24 * 3600);
    render_chart(&dir, "chart", &demo_day(now), day, (656, 413), DPI);
}

/// Paints `samples` over `window` with [`chart::paint`] into a `size` (96-DPI pixels) chart
/// at `dpi`, in the history window's 9 pt Segoe UI, and saves it as `name`.
fn render_chart(
    dir: &Path,
    name: &str,
    samples: &[Sample],
    window: Duration,
    size: (i32, i32),
    dpi: u32,
) {
    let (width, height) = (size.0 * dpi as i32 / 96, size.1 * dpi as i32 / 96);
    // SAFETY: a memory DC with its own DIB section and font, all deleted below; the bits
    // pointer is written by CreateDIBSection and stays valid while the section lives.
    unsafe {
        let dc = CreateCompatibleDC(None);
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
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
        let old_bitmap = SelectObject(dc, dib.into());
        let font = CreateFontW(
            -(9 * dpi as i32 / 72),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            Default::default(),
            Default::default(),
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, font.into());
        let rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        chart::paint(dc, rect, samples, window);
        let (w, h, pixels) = dib_pixels(dc);
        save(dir, name, w, h, &pixels);
        SelectObject(dc, old_font);
        SelectObject(dc, old_bitmap);
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(dc);
    }
}

/// Renders the history in `BATTERYSIGHT_HISTORY` (a `history.tsv`; read only) through the
/// chart, every device over 24 hours and 7 days, at the window's 96-DPI size, into
/// `target/screenshots/<BATTERYSIGHT_PREVIEW or "preview">`: a before/after check of chart
/// changes on real data.
#[test]
#[ignore = "renders real history for a visual check; run on purpose"]
fn chart_previews() {
    let Some(path) = std::env::var_os("BATTERYSIGHT_HISTORY") else {
        return;
    };
    let label = std::env::var("BATTERYSIGHT_PREVIEW").unwrap_or_else(|_| "preview".into());
    let dir = out_dir().join(label);
    fs::create_dir_all(&dir).unwrap();
    let text = fs::read_to_string(path).unwrap();
    let mut by_key: std::collections::BTreeMap<String, Vec<Sample>> = Default::default();
    for sample in text.lines().filter_map(Sample::parse) {
        by_key.entry(sample.key.0.clone()).or_default().push(sample);
    }
    for (i, samples) in by_key.values_mut().enumerate() {
        samples.sort_by_key(|s| s.at_unix_ms);
        for (range, window) in [("24h", 24), ("7d", 7 * 24)] {
            let window = Duration::from_secs(window * 3600);
            render_chart(
                &dir,
                &format!("device{i}-{range}"),
                samples,
                window,
                (656, 413),
                96,
            );
        }
    }
    let keys: Vec<&String> = by_key.keys().collect();
    fs::write(dir.join("keys.txt"), format!("{keys:?}")).unwrap();
}
