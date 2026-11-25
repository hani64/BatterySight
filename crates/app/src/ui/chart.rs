//! The battery history window's chart: a device's level over a time window ending now,
//! charging spans shaded, gaps where it slept or was off or was not observed.
//!
//! Drawn with plain GDI on the caller's DC in the font selected there (labels are sized
//! from it); everything the chart selects is restored before it returns. Drawing is bounded
//! by the chart's width: at most a column's first and last point per line ([`columns`]).

use std::time::Duration;

use batterysight_core::history::{Sample, provisional};
use batterysight_core::model::{Charge, CoarseLevel, Level, Presence};
use windows::Win32::Foundation::{COLORREF, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, Ellipse, FillRect, GetTextExtentPoint32W,
    GetTextMetricsW, HDC, IntersectClipRect, LineTo, MoveToEx, PS_DOT, PS_SOLID, Polyline,
    RestoreDC, SaveDC, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT, TextOutW,
};

use super::dwrite::Stroke;

/// Percent lines across the chart; the first, middle and last are labelled.
const GRID: [u8; 5] = [0, 25, 50, 75, 100];
const NO_DATA: &str = "No readings in this period";

/// What the chart shows, in unix milliseconds and percent.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plot {
    /// Lines through the samples of stretches the device was awake; one point alone is a dot.
    lines: Vec<Vec<(u64, u8)>>,
    /// From where a line ended as the device fell asleep to where the next began as it woke,
    /// drawn dashed: its battery barely moves while it sleeps.
    bridges: Vec<((u64, u8), (u64, u8))>,
    /// `(from, to)` while charging.
    charging: Vec<(u64, u64)>,
    /// Whether anything is drawn in the window (the sample carried in from before it alone
    /// draws nothing there).
    drawn: bool,
}

/// The longest stretch nothing was observed (the device offline, BatterySight restarting)
/// that does not break a line: a quick restart is not a gap worth showing.
const SHORT_GAP_MS: u64 = 5 * 60_000;

/// The most a level may change across a sleep for the sleep to be bridged: more (a device
/// charged while switched off, say) leaves a gap, as no line can show how it got there.
const BRIDGE_MAX_CHANGE: u8 = 5;

/// Why the last line ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// The device fell asleep (and nothing else happened since).
    Asleep,
    /// Nothing was observed: the device went offline, had no level, or BatterySight or the
    /// PC stopped.
    Unobserved,
}

/// The plot of `samples` (in time order) from `from` to `now`. It starts with the last
/// sample before `from`, so a line enters from the left edge. A line runs from a sample on
/// only while the device is awake and has a level; a sample asleep or offline (a session
/// break too) ends the line at its level, and draws nothing until the device is awake
/// again. A line that ended as the device fell asleep is bridged to the next ([`Plot`]);
/// one that ended unobserved continues if the next starts within [`SHORT_GAP_MS`], and
/// otherwise leaves a gap. A [`provisional`] wake reading draws no point. An awake
/// charging sample shades the time until the next sample (or now, for the latest).
fn plot(samples: &[Sample], from: u64, now: u64) -> Plot {
    let end = samples.partition_point(|s| s.at_unix_ms <= now);
    let start = samples[..end]
        .partition_point(|s| s.at_unix_ms < from)
        .saturating_sub(1);
    let shown = &samples[start..end];
    let mut plot = Plot::default();
    let mut line = Vec::new();
    // Why the last finished line ended, until the next one starts.
    let mut ended: Option<Ended> = None;
    for (i, s) in shown.iter().enumerate() {
        let awake = s.presence == Presence::Online;
        let point = if provisional(&samples[..end], start + i) {
            None
        } else {
            s.level.map(height)
        };
        match point {
            Some(h) if awake || !line.is_empty() => {
                if line.is_empty() {
                    let last = plot.lines.last().and_then(|l| l.last()).copied();
                    match (ended.take(), last) {
                        (Some(Ended::Asleep), Some(last))
                            if last.1.abs_diff(h) <= BRIDGE_MAX_CHANGE =>
                        {
                            plot.bridges.push((last, (s.at_unix_ms, h)));
                        }
                        (Some(Ended::Unobserved), Some(last))
                            if s.at_unix_ms - last.0 <= SHORT_GAP_MS =>
                        {
                            line = plot.lines.pop().unwrap_or_default();
                        }
                        _ => {}
                    }
                }
                line.push((s.at_unix_ms, h));
            }
            _ => {}
        }
        if !awake || s.level.is_none() {
            let why = if s.presence == Presence::Asleep && s.level.is_some() {
                Ended::Asleep
            } else {
                Ended::Unobserved
            };
            if !line.is_empty() {
                plot.lines.push(std::mem::take(&mut line));
                ended = Some(why);
            } else if ended.is_some() && why == Ended::Unobserved {
                // Asleep, then unobserved too: nothing is known about the time between.
                ended = Some(Ended::Unobserved);
            }
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
    plot.drawn = plot.lines.iter().flatten().any(|&(t, _)| t >= from)
        || plot.charging.iter().any(|&(_, to)| to > from);
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

/// The plot in pixel columns (`x` maps a time to its column), at most a column's first
/// and last point per line, so drawing is bounded by the chart's width however many
/// samples there are; a change within a column is a step in it. A line that starts in the
/// column the line before it ended in continues that line: a gap narrower than a column
/// cannot be drawn, and dropping the point would open a false one. Charging spans
/// clamped to `left..right` that touch or overlap are merged.
fn columns(plot: &Plot, x: impl Fn(u64) -> i32, left: i32, right: i32) -> Columns {
    let mut out = Columns::default();
    // The column of the last point kept, in any line.
    let mut taken: Option<i32> = None;
    for line in &plot.lines {
        let joins = line
            .first()
            .is_some_and(|&(t, _)| taken.is_some_and(|c| x(t) <= c));
        let mut points = if joins {
            out.lines.pop().unwrap_or_default()
        } else {
            Vec::new()
        };
        for &(t, p) in line {
            let column = x(t);
            if taken.is_some_and(|c| column <= c) {
                // In the last column (an earlier one, after a clock set back, is skipped):
                // the column's first point stays, its last is this one (none when equal).
                match points[..] {
                    [.., a, b] if a.0 == column && b.0 == column => {
                        points.pop();
                        if a.1 != p {
                            points.push((column, p));
                        }
                    }
                    [.., b] if b.0 == column && b.1 != p => points.push((column, p)),
                    _ => {}
                }
                continue;
            }
            points.push((column, p));
            taken = Some(column);
        }
        if !points.is_empty() {
            out.lines.push(points);
        }
    }
    for &(a, b) in &plot.charging {
        let (from, to) = (x(a).max(left), x(b).min(right));
        if to <= from {
            continue;
        }
        match out.charging.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.charging.push((from, to)),
        }
    }
    out.bridges = plot
        .bridges
        .iter()
        .map(|&((a, p), (b, q))| ((x(a), p), (x(b), q)))
        .collect();
    out
}

/// A [`Plot`] in pixel columns ([`columns`]).
#[derive(Debug, Default, PartialEq, Eq)]
struct Columns {
    /// (column, percent) per line.
    lines: Vec<Vec<(i32, u8)>>,
    /// The plot's bridges, (column, percent) at each end.
    bridges: Vec<((i32, u8), (i32, u8))>,
    /// (first column, end column) while charging.
    charging: Vec<(i32, i32)>,
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

/// The chart's colours, on the window's light background.
const COLORS: Colors = Colors {
    background: rgb(255, 255, 255),
    grid: rgb(229, 229, 229),
    text: rgb(96, 96, 96),
    line: rgb(LINE_RGB[0], LINE_RGB[1], LINE_RGB[2]),
    charging: rgb(223, 246, 221),
};

/// The line's colour; also [`COLORS`]'s.
const LINE_RGB: [u8; 3] = [0, 103, 192];

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Paints the chart of `samples` (one device's, in time order) over the `window` ending
/// now into `rect` of `dc`. Labels use the font selected in `dc`; line widths follow its
/// height, so they scale with the DPI the font was made for.
pub fn paint(dc: HDC, rect: RECT, samples: &[Sample], window: Duration) {
    let now = super::unix_now_ms();
    let span = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
    let from = now.saturating_sub(span);
    let plot = plot(samples, from, now);
    let colors = &COLORS;
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
        // Bounded by the width, however many samples the window holds.
        let columns = columns(&plot, x, area.left, area.right);

        for &(left, right) in &columns.charging {
            let band = RECT {
                left,
                top: area.top,
                right,
                bottom: area.bottom,
            };
            fill(dc, &band, colors.charging);
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
        if !plot.drawn {
            let w = text_width(dc, NO_DATA);
            text(
                dc,
                area.left + (width - w) / 2,
                area.top + (height - text_h) / 2,
                NO_DATA,
            );
        }

        // Lines may reach above 100 % and below 0 % by their own width, and never left of
        // the area into the labels.
        let bounds = RECT {
            left: area.left,
            top: area.top - thick,
            right: area.right + thick,
            bottom: area.bottom + thick,
        };
        let at = |x: i32, p: u8| ((x - bounds.left) as f32, (y(p) - bounds.top) as f32);
        let strokes: Vec<Stroke> = columns
            .bridges
            .iter()
            .map(|&((x0, p0), (x1, p1))| Stroke {
                points: vec![at(x0, p0), at(x1, p1)],
                dashed: true,
            })
            .chain(columns.lines.iter().map(|line| Stroke {
                points: line.iter().map(|&(x, p)| at(x, p)).collect(),
                dashed: false,
            }))
            .collect();
        // Antialiased by Direct2D; GDI draws them when it cannot.
        let width = text_h as f32 / 7.0;
        let smooth =
            super::dwrite::with(|e| e.stroke(dc, bounds, &strokes, LINE_RGB, width).map(Some))
                .is_some();
        IntersectClipRect(dc, bounds.left, bounds.top, bounds.right, bounds.bottom);
        let line_pen = CreatePen(PS_SOLID, thick, colors.line);
        let bridge_pen = CreatePen(PS_DOT, 1, colors.line);
        let dot_brush = CreateSolidBrush(colors.line);
        if !smooth {
            SelectObject(dc, bridge_pen.into());
            for &((x0, p0), (x1, p1)) in &columns.bridges {
                let _ = MoveToEx(dc, x0, y(p0), None);
                let _ = LineTo(dc, x1, y(p1));
            }
            SelectObject(dc, line_pen.into());
            SelectObject(dc, dot_brush.into());
            for line in &columns.lines {
                let points: Vec<POINT> = line.iter().map(|&(x, p)| POINT { x, y: y(p) }).collect();
                if let [dot] = points[..] {
                    // A reading alone: a filled dot of diameter 2 × the line's thickness
                    // + 1, in the line's colour.
                    let r = thick;
                    let _ = Ellipse(dc, dot.x - r, dot.y - r, dot.x + r + 1, dot.y + r + 1);
                } else {
                    let _ = Polyline(dc, &points);
                }
            }
        }
        let _ = RestoreDC(dc, saved);
        let _ = DeleteObject(grid_pen.into());
        let _ = DeleteObject(line_pen.into());
        let _ = DeleteObject(bridge_pen.into());
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

    fn restart(at: u64) -> Sample {
        Sample::session_break(DeviceKey("logi:4082:1".into()), T + at * MIN)
    }

    #[test]
    fn a_sleep_is_bridged_to_the_next_wake() {
        let samples = [on(10, 90), asleep(30, 88), on(60, 87), on(70, 86)];
        let p = plot(&samples, at(0), at(80));
        assert_eq!(
            p.lines,
            [
                vec![(at(10), 90), (at(30), 88)],
                vec![(at(60), 87), (at(70), 86)],
            ]
        );
        assert_eq!(p.bridges, [((at(30), 88), (at(60), 87))]);
    }

    #[test]
    fn a_sleep_that_changed_the_level_much_is_not_bridged() {
        // Switched off at 78 % and charged while off: no line can say how it got to 100 %.
        let samples = [on(10, 78), asleep(20, 78), on(600, 100)];
        let p = plot(&samples, at(0), at(610));
        assert_eq!(p.lines.len(), 2);
        assert!(p.bridges.is_empty());
    }

    #[test]
    fn a_quick_restart_does_not_break_the_line() {
        // BatterySight restarted within a few minutes: the line goes on.
        let samples = [on(10, 90), on(20, 89), restart(23), on(23, 89), on(30, 88)];
        let p = plot(&samples, at(0), at(40));
        assert_eq!(
            p.lines,
            [vec![(at(10), 90), (at(20), 89), (at(23), 89), (at(30), 88)]]
        );
        assert!(p.bridges.is_empty());
    }

    #[test]
    fn unobserved_stretches_stay_gaps() {
        // The PC was off for an hour: nothing is known about that time.
        let off = [on(10, 90), restart(11), on(70, 85)];
        let p = plot(&off, at(0), at(80));
        assert_eq!(p.lines, [vec![(at(10), 90)], vec![(at(70), 85)]]);
        assert!(p.bridges.is_empty());
        // Asleep, then BatterySight or the PC stopped: the sleep is not bridged either.
        let slept_then_off = [on(10, 90), asleep(20, 89), restart(30), on(90, 80)];
        let p = plot(&slept_then_off, at(0), at(100));
        assert_eq!(p.lines.len(), 2);
        assert!(p.bridges.is_empty());
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
    fn no_data_whenever_nothing_is_drawn() {
        // Only the carry-in before the window.
        let samples = [on(0, 90)];
        assert!(!plot(&samples, at(10), at(20)).drawn);
        // Nor a session break.
        let samples = [
            on(0, 90),
            Sample::session_break(DeviceKey("logi:4082:1".into()), at(15)),
        ];
        assert!(!plot(&samples, at(10), at(20)).drawn);
        assert!(plot(&[on(0, 90), on(15, 89)], at(10), at(20)).drawn);
        assert!(!plot(&[], at(10), at(20)).drawn);
        // Readings that draw nothing: asleep or offline with no awake sample before, awake
        // without a level.
        let samples = [
            s(
                12,
                Some(Level::Percent(50)),
                Presence::Asleep,
                Charge::Discharging,
            ),
            s(
                14,
                Some(Level::Percent(50)),
                Presence::Offline,
                Charge::Discharging,
            ),
            s(16, None, Presence::Online, Charge::Unknown),
        ];
        assert!(!plot(&samples, at(10), at(20)).drawn);
        // An awake charging stretch is drawn, even without a level.
        let charging = [s(12, None, Presence::Online, Charge::Charging)];
        assert!(plot(&charging, at(10), at(20)).drawn);
        // A sleep that ends a line entering the window is drawn.
        let samples = [
            on(0, 90),
            s(
                15,
                Some(Level::Percent(89)),
                Presence::Asleep,
                Charge::Discharging,
            ),
        ];
        assert!(plot(&samples, at(10), at(20)).drawn);
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

    #[test]
    fn columns_keep_a_column_s_first_and_last_point() {
        let plot = Plot {
            lines: vec![
                vec![(0, 90), (1, 89), (4, 88), (10, 80)],
                // Starts in the column the line before ended in: continues it, with a step
                // in that column from where the line before ended.
                vec![(11, 79), (20, 70)],
                // Wholly in that column: joins it too, the later value ending the step.
                vec![(21, 69)],
                vec![(40, 60)],
            ],
            bridges: Vec::new(),
            charging: vec![(0, 3), (5, 12), (14, 16), (30, 200)],
            drawn: true,
        };
        let c = columns(&plot, |t| (t / 5) as i32, 0, 20);
        assert_eq!(
            c.lines,
            [
                vec![(0, 90), (0, 88), (2, 80), (2, 79), (4, 70), (4, 69)],
                vec![(8, 60)]
            ]
        );
        // Pixel spans (0,0) empty, (1,2) and (2,3) merge, (6,20) clamped to the right edge.
        assert_eq!(c.charging, [(1, 3), (6, 20)]);
    }

    #[test]
    fn a_level_held_then_dropping_stays_flat_until_the_drop() {
        // 67 % held for hours, then asleep, then 61 % a moment later in the same column.
        let samples = [
            on(0, 67),
            s(
                230,
                Some(Level::Percent(67)),
                Presence::Asleep,
                Charge::Discharging,
            ),
            on(231, 61),
        ];
        let p = plot(&samples, at(0), at(240));
        let c = columns(&p, |t| ((t - at(0)) / (2 * MIN)) as i32, 0, 120);
        // Not a slope from 67 to 61 over the hours it read 67.
        assert_eq!(c.lines, [vec![(0, 67), (115, 67), (115, 61)]]);
    }

    /// A device's push as it wakes, with millivolts (its level comes from the voltage).
    fn wake_push(at: u64, p: u8) -> Sample {
        Sample {
            millivolts: Some(3_800),
            source: Source::Push,
            ..on(at, p)
        }
    }

    fn asleep(at: u64, p: u8) -> Sample {
        s(
            at,
            Some(Level::Percent(p)),
            Presence::Asleep,
            Charge::Discharging,
        )
    }

    #[test]
    fn a_wake_reading_gives_way_to_a_reading_soon_after() {
        let brk = |m| Sample::session_break(DeviceKey("logi:4082:1".into()), at(m));
        // Asleep, then the wake push reads 64 % on a rested battery; a minute later (after
        // a restart) the device reads 61 % in use: the line starts at the 61.
        let samples = [
            asleep(0, 61),
            wake_push(10, 64),
            brk(11),
            on(11, 61),
            on(20, 60),
        ];
        assert_eq!(
            plot(&samples, at(0), at(30)).lines,
            [vec![(at(11), 61), (at(20), 60)]]
        );
        // The wake push and the asleep sample that kept its level draw nothing either.
        let samples = [
            asleep(0, 50),
            wake_push(10, 58),
            asleep(12, 58),
            wake_push(14, 58),
            on(15, 50),
        ];
        assert_eq!(plot(&samples, at(0), at(30)).lines, [vec![(at(15), 50)]]);
        // The latest reading stays, as does one with no reading within 10 min after it.
        let samples = [asleep(0, 50), wake_push(10, 58)];
        assert_eq!(plot(&samples, at(0), at(30)).lines, [vec![(at(10), 58)]]);
        let samples = [asleep(0, 50), wake_push(10, 58), on(21, 50)];
        assert_eq!(
            plot(&samples, at(0), at(30)).lines,
            [vec![(at(10), 58), (at(21), 50)]]
        );
        // A reading while awake, or one without millivolts (a percentage the device
        // reports itself), is drawn.
        let samples = [on(0, 60), wake_push(10, 64), on(11, 61)];
        assert_eq!(plot(&samples, at(0), at(30)).lines[0].len(), 3);
        let pushed = Sample {
            source: Source::Push,
            ..on(10, 64)
        };
        let samples = [asleep(0, 61), pushed, on(11, 61)];
        assert_eq!(
            plot(&samples, at(0), at(30)).lines,
            [vec![(at(10), 64), (at(11), 61)]]
        );
    }

    #[test]
    fn a_nap_within_a_column_leaves_no_gap() {
        // 2 min per column. The device slept for a minute within one column (the first
        // line ends at its asleep sample), then was in use for 30 min at the same level.
        let samples = [
            on(0, 90),
            on(8, 89),
            s(
                10,
                Some(Level::Percent(89)),
                Presence::Asleep,
                Charge::Discharging,
            ),
            on(11, 89),
            s(
                42,
                Some(Level::Percent(89)),
                Presence::Asleep,
                Charge::Discharging,
            ),
        ];
        let p = plot(&samples, at(0), at(60));
        assert_eq!(p.lines.len(), 2);
        let c = columns(&p, |t| ((t - at(0)) / (2 * MIN)) as i32, 0, 30);
        // One line: the session after the nap is drawn, not a dot after a gap.
        assert_eq!(c.lines, [vec![(0, 90), (4, 89), (5, 89), (21, 89)]]);
    }
}
