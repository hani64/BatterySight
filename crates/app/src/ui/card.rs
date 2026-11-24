//! The hover card: a device's name, level, status, time left and its battery chart of the
//! last 24 h, shown above the taskbar once the cursor has rested on the device on a widget
//! for [`HOVER_MS`](super::HOVER_MS). The tray icons' tooltips carry the same text
//! ([`card_text`]).
//!
//! The card is a layered top-level popup of the UI thread (`WS_POPUP`; a tool window,
//! topmost, never activated and click-through), never a child of a taskbar: creating,
//! showing, moving and hiding it are calls on our own window that Explorer takes no part
//! in. It is created when first shown and painted from the snapshot and the history in
//! memory: no device traffic, no disk. Its chart is bounded by its width (`chart`).

use std::time::Duration;

use batterysight_core::history::{self, Sample};
use batterysight_core::model::{Charge, DeviceKey, DeviceState, Presence};
use batterysight_core::time::Ms;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MonitorFromRect,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, MA_NOACTIVATE, SWP_HIDEWINDOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA,
    UpdateLayeredWindow, WM_MOUSEACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

use super::widget::Hover;
use super::{CARD_CLASS, hinstance, level_text, render};

/// What the card's chart covers.
const CHART_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// The card's window, created when first shown, and the device it shows.
pub struct Card {
    owner: HWND,
    hwnd: Option<HWND>,
    shown: Option<DeviceKey>,
}

impl Card {
    /// No window yet; `owner` (the host window) will own it.
    pub fn new(owner: HWND) -> Card {
        Card {
            owner,
            hwnd: None,
            shown: None,
        }
    }

    /// The device whose card is shown.
    pub fn shown(&self) -> Option<&DeviceKey> {
        self.shown.as_ref()
    }

    /// Paints `lines` ([`card_text`]) and the chart of `samples` (the device's, in time
    /// order) at the DPI of `hover`'s widget, and shows the card above `hover`'s taskbar,
    /// on its monitor ([`place`]). A card that cannot be drawn is hidden.
    pub fn show(&mut self, hover: &Hover, lines: &[String], samples: &[Sample], light: bool) {
        let painter = render::card(lines, samples, CHART_WINDOW, hover.dpi, light);
        let (Some(painter), Some((monitor, work))) = (painter, monitor_of(&hover.bar)) else {
            self.hide();
            return;
        };
        let Some(hwnd) = self.window() else {
            return;
        };
        let size = painter.size();
        let gap = 8 * hover.dpi as i32 / 96;
        let at = place(size, hover.device, hover.bar, monitor, work, gap);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: our own window; every pointer outlives the call; the painter's DC holds
        // the bitmap.
        let painted = unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&at),
                Some(&size),
                Some(painter.dc()),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        };
        if painted.is_err() {
            self.hide();
            return;
        }
        // Topmost again: above topmost windows shown since.
        // SAFETY: our own window.
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        self.shown = Some(hover.key.clone());
    }

    /// Hides the card, if shown.
    pub fn hide(&mut self) {
        if self.shown.take().is_none() {
            return;
        }
        if let Some(hwnd) = self.hwnd {
            // SAFETY: our own window.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
    }

    /// The card's window, created on first use.
    fn window(&mut self) -> Option<HWND> {
        if self.hwnd.is_none() {
            // SAFETY: the class is registered by `ui::run`; literal strings.
            self.hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_TOOLWINDOW
                        | WS_EX_TOPMOST
                        | WS_EX_NOACTIVATE,
                    CARD_CLASS,
                    w!("BatterySight"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    Some(self.owner),
                    None,
                    Some(hinstance()),
                    None,
                )
            }
            .ok();
        }
        self.hwnd
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        if let Some(hwnd) = self.hwnd.take() {
            // SAFETY: our own window (already gone with its owner, the call fails harmlessly).
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }
}

/// The card's window procedure: it is never activated.
pub unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // SAFETY: forwarding this window's own message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// The card's lines: the device's name; its level; charging, or how long ago it was last
/// read (asleep or not); and, unless it is on power, the time left
/// ([`history::time_left_text`]). `now`: the monotonic time of the readings.
pub fn card_text(state: &DeviceState, time_left: Option<Duration>, now: Ms) -> Vec<String> {
    let level = state.level.map_or_else(|| "unknown".to_owned(), level_text);
    let age = age_text(now.saturating_sub(state.updated_ms));
    let status = match (state.presence, state.charge) {
        (Presence::Online, Charge::Charging) => "Charging".to_owned(),
        (Presence::Online, _) => format!("Updated {age}"),
        (Presence::Asleep, _) => format!("Asleep, updated {age}"),
        (Presence::Offline, _) => format!("Disconnected, updated {age}"),
    };
    let mut lines = vec![state.name.clone(), format!("Battery: {level}"), status];
    let on_power = state.presence == Presence::Online
        && matches!(
            state.charge,
            Charge::Charging | Charge::Full | Charge::NotCharging
        );
    if !on_power {
        lines.push(history::time_left_text(time_left));
    }
    lines
}

/// "just now", "N min ago", "N h ago" (from 1 h), "N days ago" (from 48 h).
fn age_text(ms: u64) -> String {
    match ms / 60_000 {
        0 => "just now".to_owned(),
        min @ 1..60 => format!("{min} min ago"),
        min @ 60..2880 => format!("{} h ago", min / 60),
        min => format!("{} days ago", min / (24 * 60)),
    }
}

/// Where a card of `size` goes for the `device` (its columns of a widget on the taskbar
/// `bar`): `gap` above the taskbar and centred on the device (below it when the taskbar is
/// in the top half of its `monitor`); beside a vertical taskbar, on the monitor's side of
/// it, level with the device. Kept inside the monitor's `work` area.
fn place(size: SIZE, device: RECT, bar: RECT, monitor: RECT, work: RECT, gap: i32) -> POINT {
    let vertical = bar.bottom - bar.top > bar.right - bar.left;
    let (x, y) = if vertical {
        let at_left = bar.left + bar.right < monitor.left + monitor.right;
        let x = if at_left {
            bar.right + gap
        } else {
            bar.left - gap - size.cx
        };
        (x, (device.top + device.bottom) / 2 - size.cy / 2)
    } else {
        let at_top = bar.top + bar.bottom < monitor.top + monitor.bottom;
        let y = if at_top {
            bar.bottom + gap
        } else {
            bar.top - gap - size.cy
        };
        ((device.left + device.right) / 2 - size.cx / 2, y)
    };
    POINT {
        x: x.clamp(work.left, (work.right - size.cx).max(work.left)),
        y: y.clamp(work.top, (work.bottom - size.cy).max(work.top)),
    }
}

/// (monitor, work area) of the monitor nearest to `rect`.
fn monitor_of(rect: &RECT) -> Option<(RECT, RECT)> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain queries; `rect` and `info` outlive the calls, `info` has its size set.
    unsafe {
        let monitor = MonitorFromRect(rect, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some((info.rcMonitor, info.rcWork))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{
        Charge, DeviceKey, DeviceKind, Level, Presence, Source, Verification, Via,
    };

    const MIN: u64 = 60_000;
    /// The reading's monotonic time.
    const NOW: Ms = 50 * 60 * MIN;

    fn state(presence: Presence, charge: Charge, updated_ms: Ms) -> DeviceState {
        DeviceState {
            key: DeviceKey("logi:4082:1".into()),
            name: "G305".into(),
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: Some(Level::Percent(80)),
            millivolts: None,
            charge,
            presence,
            updated_ms,
            source: Source::Query,
            verification: Verification::HardwareTested,
        }
    }

    #[test]
    fn card_text_online_discharging() {
        let s = state(Presence::Online, Charge::Discharging, NOW - 3 * MIN);
        let left = Some(Duration::from_secs(20 * 3600));
        assert_eq!(
            card_text(&s, left, NOW),
            [
                "G305",
                "Battery: 80%",
                "Updated 3 min ago",
                "Time left: about 20 h"
            ]
        );
        // Under a minute.
        assert_eq!(
            card_text(&s, left, s.updated_ms + 59_000)[2],
            "Updated just now"
        );
    }

    #[test]
    fn card_text_asleep_shows_age() {
        let s = state(Presence::Asleep, Charge::Discharging, NOW - 12 * MIN);
        assert_eq!(
            card_text(&s, None, NOW),
            [
                "G305",
                "Battery: 80%",
                "Asleep, updated 12 min ago",
                "Time left: not enough data"
            ]
        );
        let at = |ago| card_text(&s, None, s.updated_ms + ago)[2].clone();
        assert_eq!(at(125 * MIN), "Asleep, updated 2 h ago");
        assert_eq!(at(3 * 24 * 60 * MIN), "Asleep, updated 3 days ago");
    }

    #[test]
    fn card_text_charging_has_no_time_left() {
        let mut s = state(Presence::Online, Charge::Charging, NOW);
        s.level = None;
        assert_eq!(
            card_text(&s, None, NOW),
            ["G305", "Battery: unknown", "Charging"]
        );
        // On power, nothing to estimate either.
        s.charge = Charge::Full;
        assert_eq!(card_text(&s, None, NOW).len(), 3);
    }

    #[test]
    fn card_sits_above_the_taskbar_within_the_work_area() {
        let rect = |left, top, right, bottom| RECT {
            left,
            top,
            right,
            bottom,
        };
        let size = SIZE { cx: 300, cy: 200 };
        // Second monitor, right of the first; taskbar at the bottom.
        let monitor = rect(1920, 0, 3840, 1080);
        let bar = rect(1920, 1032, 3840, 1080);
        let work = rect(1920, 0, 3840, 1032);
        // The device's columns of a widget on `bar`, centred on `x`.
        let device = |x| rect(x - 20, 1032, x + 20, 1080);
        let at = |x| place(size, device(x), bar, monitor, work, 8);
        assert_eq!(at(3000), POINT { x: 2850, y: 824 });
        // Kept on the monitor of the taskbar.
        assert_eq!(at(3800), POINT { x: 3540, y: 824 });
        assert_eq!(at(1930), POINT { x: 1920, y: 824 });
        // Auto-hide: the work area is the whole monitor; still above the taskbar.
        assert_eq!(
            place(size, device(3000), bar, monitor, monitor, 8),
            POINT { x: 2850, y: 824 }
        );
        // A taskbar at the top: below it.
        let top = rect(1920, 0, 3840, 48);
        assert_eq!(
            place(
                size,
                rect(2980, 0, 3020, 48),
                top,
                monitor,
                rect(1920, 48, 3840, 1080),
                8
            ),
            POINT { x: 2850, y: 56 }
        );
    }

    #[test]
    fn card_sits_beside_a_vertical_taskbar_next_to_the_device() {
        let rect = |left, top, right, bottom| RECT {
            left,
            top,
            right,
            bottom,
        };
        let size = SIZE { cx: 300, cy: 200 };
        let monitor = rect(0, 0, 1920, 1080);
        // Left: right of the taskbar, centred on the device's height.
        let left = rect(0, 0, 48, 1080);
        let work = rect(48, 0, 1920, 1080);
        assert_eq!(
            place(size, rect(0, 500, 48, 540), left, monitor, work, 8),
            POINT { x: 56, y: 420 }
        );
        // Kept inside the work area near the bottom.
        assert_eq!(
            place(size, rect(0, 1000, 48, 1060), left, monitor, work, 8),
            POINT { x: 56, y: 880 }
        );
        // Right: left of the taskbar.
        let right = rect(1872, 0, 1920, 1080);
        assert_eq!(
            place(
                size,
                rect(1872, 20, 1920, 60),
                right,
                monitor,
                rect(0, 0, 1872, 1080),
                8
            ),
            POINT { x: 1564, y: 0 }
        );
    }
}
