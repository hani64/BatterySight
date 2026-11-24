//! The battery chart, shared by the history window and the hover card: a device's level
//! over a time window ending now, charging spans shaded, gaps where it slept or was off.
//!
//! Drawn with plain GDI on the caller's DC in the font selected there (labels are sized
//! from it); everything the chart selects is restored before it returns.

use std::time::Duration;

use batterysight_core::history::Sample;
use batterysight_core::model::{Charge, CoarseLevel, Level, Presence};
use windows::Win32::Foundation::{COLORREF, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, Ellipse, FillRect, GetTextExtentPoint32W,
    GetTextMetricsW, HDC, IntersectClipRect, LineTo, MoveToEx, PS_SOLID, Polyline, RestoreDC,
    SaveDC, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT, TextOutW,
};

/// Percent lines across the chart; the first, middle and last are labelled.
const GRID: [u8; 5] = [0, 25, 50, 75, 100];
const NO_DATA: &str = "No readings in this period";

/// What the chart shows, in unix milliseconds and percent.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plot {
    /// Lines through the samples of stretches the device was awake; one point alone is a dot.
    lines: Vec<Vec<(u64, u8)>>,
    /// `(from, to)` while charging.
    charging: Vec<(u64, u64)>,
    /// Whether a reading was taken in the window (the sample carried in from before it
    /// and session breaks are none).
    readings: bool,
}

/// The plot of `samples` (in time order) from `from` to `now`. It starts with the last
/// sample before `from`, so a line enters from the left edge. A line runs from a sample on
/// only while the device is awake and has a level; a sample asleep or offline (a session
/// break too) ends the line at its level, and draws nothing until the device is awake
/// again. An awake charging sample shades the time until the next sample (or now, for the
/// latest).
fn plot(samples: &[Sample], from: u64, now: u64) -> Plot {
    let end = samples.partition_point(|s| s.at_unix_ms <= now);
    let start = samples[..end]
        .partition_point(|s| s.at_unix_ms < from)
        .saturating_sub(1);
    let shown = &samples[start..end];
    let mut plot = Plot {
        readings: shown.iter().any(|s| s.at_unix_ms >= from && !s.is_break()),
        ..Plot::default()
    };
    let mut line = Vec::new();
    for (i, s) in shown.iter().enumerate() {
        let awake = s.presence == Presence::Online;
        match s.level.map(height) {
            Some(h) if awake || !line.is_empty() => line.push((s.at_unix_ms, h)),
            _ => {}
        }
        if (!awake || s.level.is_none()) && !line.is_empty() {
            plot.lines.push(std::mem::take(&mut line));
        }
        if awake && s.charge == Charge::Charging {
            let to = shown.get(i + 1).map_or(now, |next| next.at_unix_ms);
            match plot.charging.last_mut() {
                Some(last) if last.1 == s.at_unix_ms => last.1 = to,
                _ => plot.charging.push((s.at_unix_ms, to)),
            }
        }
    }
    if !line.is_empty() {
        plot.lines.push(line);
    }
    plot
}

/// Where a level is drawn, in percent; a coarse level at a fixed height in its band.
fn height(level: Level) -> u8 {
    match level {
        Level::Percent(p) => p.min(100),
        Level::Coarse(CoarseLevel::Critical) => 5,
        Level::Coarse(CoarseLevel::Low) => 20,
        Level::Coarse(CoarseLevel::Good) => 60,
        Level::Coarse(CoarseLevel::Full) => 100,
    }
}

/// "24 h ago", "7 days ago": the left end of a `span_ms` window.
fn start_label(span_ms: u64) -> String {
    const HOUR: u64 = 3_600_000;
    let hours = span_ms / HOUR;
    if hours > 48 {
        format!("{} days ago", hours / 24)
    } else {
        format!("{hours} h ago")
    }
}

struct Colors {
    background: COLORREF,
    grid: COLORREF,
    text: COLORREF,
    line: COLORREF,
    charging: COLORREF,
}

impl Colors {
    fn new(light: bool) -> Colors {
        if light {
            Colors {
                background: rgb(255, 255, 255),
                grid: rgb(229, 229, 229),
                text: rgb(96, 96, 96),
                line: rgb(0, 103, 192),
                charging: rgb(223, 246, 221),
            }
        } else {
            Colors {
                background: rgb(32, 32, 32),
                grid: rgb(64, 64, 64),
                text: rgb(200, 200, 200),
                line: rgb(96, 205, 255),
                charging: rgb(31, 58, 35),
            }
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

/// Paints the chart of `samples` (one device's, in time order) over the `window` ending
/// now into `rect` of `dc`, for a light or dark background. Labels use the font selected
/// in `dc`; line widths follow its height, so they scale with the DPI the font was made for.
pub fn paint(dc: HDC, rect: RECT, samples: &[Sample], window: Duration, light: bool) {
    let now = super::unix_now_ms();
    let span = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
    let from = now.saturating_sub(span);
    let plot = plot(samples, from, now);
    let colors = Colors::new(light);
    // SAFETY: GDI calls on the caller's live DC; the strings and points outlive the calls
    // that read them. The DC's state is saved first and restored before the pens and
    // brushes made here are deleted, so none is deleted while selected.
    unsafe {
        let saved = SaveDC(dc);
        fill(dc, &rect, colors.background);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, colors.text);
        let mut metrics = TEXTMETRICW::default();
        let _ = GetTextMetricsW(dc, &mut metrics);
        let text_h = metrics.tmHeight.max(1);
        // 2 px with a 9 pt font at 96 DPI.
        let thick = (text_h / 7).max(1);
        let area = RECT {
            left: rect.left + text_width(dc, "100 %") + text_h / 2,
            top: rect.top + text_h / 2,
            right: rect.right - text_h / 2,
            bottom: rect.bottom - text_h - text_h / 4,
        };
        let (width, height) = (area.right - area.left, area.bottom - area.top);
        if width < 2 || height < 2 {
            let _ = RestoreDC(dc, saved);
            return;
        }
        let x = |t: u64| {
            let offset = (i128::from(t) - i128::from(from)) * i128::from(width) / i128::from(span);
            area.left + offset.clamp(-1_000_000, 1_000_000) as i32
        };
        let y = |p: u8| area.bottom - i32::from(p) * height / 100;

        for &(a, b) in &plot.charging {
            let band = RECT {
                left: x(a).max(area.left),
                top: area.top,
                right: x(b).min(area.right),
                bottom: area.bottom,
            };
            if band.right > band.left {
                fill(dc, &band, colors.charging);
            }
        }
        let grid_pen = CreatePen(PS_SOLID, 1, colors.grid);
        SelectObject(dc, grid_pen.into());
        for p in GRID {
            let _ = MoveToEx(dc, area.left, y(p), None);
            let _ = LineTo(dc, area.right, y(p));
            if p % 50 == 0 {
                let label = format!("{p} %");
                let w = text_width(dc, &label);
                text(dc, area.left - text_h / 4 - w, y(p) - text_h / 2, &label);
            }
        }
        let below = area.bottom + text_h / 8;
        text(dc, area.left, below, &start_label(span));
        text(dc, area.right - text_width(dc, "now"), below, "now");
        if !plot.readings {
            let w = text_width(dc, NO_DATA);
            text(
                dc,
                area.left + (width - w) / 2,
                area.top + (height - text_h) / 2,
                NO_DATA,
            );
        }

        IntersectClipRect(
            dc,
            area.left,
            area.top - thick,
            area.right + thick,
            area.bottom + thick,
        );
        let line_pen = CreatePen(PS_SOLID, thick, colors.line);
        let dot_brush = CreateSolidBrush(colors.line);
        SelectObject(dc, line_pen.into());
        SelectObject(dc, dot_brush.into());
        for line in &plot.lines {
            let points: Vec<POINT> = line
                .iter()
                .map(|&(t, p)| POINT { x: x(t), y: y(p) })
                .collect();
            if let [dot] = points[..] {
                let r = thick + 1;
                let _ = Ellipse(dc, dot.x - r, dot.y - r, dot.x + r + 1, dot.y + r + 1);
            } else {
                let _ = Polyline(dc, &points);
            }
        }
        let _ = RestoreDC(dc, saved);
        let _ = DeleteObject(grid_pen.into());
        let _ = DeleteObject(line_pen.into());
        let _ = DeleteObject(dot_brush.into());
    }
}

/// # Safety
/// `dc` is a live DC.
unsafe fn fill(dc: HDC, rect: &RECT, color: COLORREF) {
    // SAFETY: the brush is not selected anywhere and is deleted after use.
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

/// # Safety
/// `dc` is a live DC.
unsafe fn text(dc: HDC, x: i32, y: i32, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: `wide` outlives the call.
    let _ = unsafe { TextOutW(dc, x, y, &wide) };
}

/// # Safety
/// `dc` is a live DC.
unsafe fn text_width(dc: HDC, text: &str) -> i32 {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    // SAFETY: `wide` and `size` outlive the call.
    let _ = unsafe { GetTextExtentPoint32W(dc, &wide, &mut size) };
    size.cx
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{Charge, DeviceKey, Presence, Source};

    const T: u64 = 1_795_478_400_000;
    const MIN: u64 = 60_000;

    fn s(at: u64, level: Option<Level>, presence: Presence, charge: Charge) -> Sample {
        Sample {
            at_unix_ms: T + at * MIN,
            key: DeviceKey("logi:4082:1".into()),
            level,
            charge,
            presence,
            millivolts: None,
            source: Source::Query,
        }
    }

    fn on(at: u64, p: u8) -> Sample {
        s(
            at,
            Some(Level::Percent(p)),
            Presence::Online,
            Charge::Discharging,
        )
    }

    fn at(m: u64) -> u64 {
        T + m * MIN
    }

    #[test]
    fn lines_break_where_the_device_slept() {
        let samples = [
            on(10, 90),
            on(20, 88),
            // Went to sleep at 86 %: the line ends there.
            s(
                30,
                Some(Level::Percent(86)),
                Presence::Asleep,
                Charge::Discharging,
            ),
            // Further asleep or offline samples draw nothing.
            s(
                40,
                Some(Level::Percent(86)),
                Presence::Offline,
                Charge::Discharging,
            ),
            on(50, 85),
            // A sample without a level breaks the line too.
            s(55, None, Presence::Online, Charge::Unknown),
            on(60, 84),
        ];
        assert_eq!(
            plot(&samples, at(0), at(70)).lines,
            [
                vec![(at(10), 90), (at(20), 88), (at(30), 86)],
                vec![(at(50), 85)],
                vec![(at(60), 84)],
            ]
        );
    }

    #[test]
    fn line_enters_from_the_sample_before_the_window() {
        let samples = [on(0, 95), on(10, 94), on(30, 93), on(40, 92)];
        let p = plot(&samples, at(20), at(50));
        assert_eq!(p.lines, [vec![(at(10), 94), (at(30), 93), (at(40), 92)]]);
        // Samples after now are left out.
        assert_eq!(
            plot(&samples, at(0), at(15)).lines,
            [vec![(at(0), 95), (at(10), 94)]]
        );
    }

    #[test]
    fn charging_is_shaded_until_the_next_sample_or_now() {
        let charging = |m, p| {
            s(
                m,
                Some(Level::Percent(p)),
                Presence::Online,
                Charge::Charging,
            )
        };
        let samples = [
            on(0, 20),
            charging(10, 25),
            charging(20, 40),
            on(30, 60),
            charging(40, 61),
        ];
        let p = plot(&samples, at(0), at(50));
        assert_eq!(p.charging, [(at(10), at(30)), (at(40), at(50))]);
        assert_eq!(p.lines.len(), 1, "charging does not break the line");
    }

    #[test]
    fn coarse_levels_are_plotted_at_fixed_heights() {
        let coarse = |m, c| {
            s(
                m,
                Some(Level::Coarse(c)),
                Presence::Online,
                Charge::Discharging,
            )
        };
        let samples = [
            coarse(0, CoarseLevel::Full),
            coarse(1, CoarseLevel::Good),
            coarse(2, CoarseLevel::Low),
            coarse(3, CoarseLevel::Critical),
        ];
        let heights: Vec<u8> = plot(&samples, at(0), at(5)).lines[0]
            .iter()
            .map(|p| p.1)
            .collect();
        assert_eq!(heights, [100, 60, 20, 5]);
    }

    #[test]
    fn session_break_leaves_a_gap() {
        let samples = [
            on(0, 82),
            on(60, 80),
            // The PC was off: started again at 08:00.
            Sample::session_break(DeviceKey("logi:4082:1".into()), at(600)),
            on(600, 80),
            on(660, 78),
        ];
        assert_eq!(
            plot(&samples, at(0), at(700)).lines,
            [
                vec![(at(0), 82), (at(60), 80)],
                vec![(at(600), 80), (at(660), 78)]
            ]
        );
    }

    #[test]
    fn only_samples_in_the_window_are_readings() {
        // Only the carry-in before the window: nothing read in it.
        let samples = [on(0, 90)];
        assert!(!plot(&samples, at(10), at(20)).readings);
        // Nor is a session break.
        let samples = [
            on(0, 90),
            Sample::session_break(DeviceKey("logi:4082:1".into()), at(15)),
        ];
        assert!(!plot(&samples, at(10), at(20)).readings);
        assert!(plot(&[on(0, 90), on(15, 89)], at(10), at(20)).readings);
        assert!(!plot(&[], at(10), at(20)).readings);
    }

    #[test]
    fn charging_is_shaded_only_while_online() {
        let charging = |m, presence| s(m, Some(Level::Percent(50)), presence, Charge::Charging);
        // Offline or asleep with a charging state kept: no shade.
        let samples = [
            charging(0, Presence::Offline),
            charging(10, Presence::Asleep),
        ];
        assert!(plot(&samples, at(0), at(50)).charging.is_empty());
        // Online and charging: shaded until the next sample, here the device leaving.
        let samples = [
            charging(10, Presence::Online),
            charging(30, Presence::Offline),
        ];
        assert_eq!(plot(&samples, at(0), at(50)).charging, [(at(10), at(30))]);
    }
}
