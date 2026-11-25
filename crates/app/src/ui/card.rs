//! The hover card: two compact lines with a device's name and level, its status and time
//! left, shown just above the taskbar once the cursor has rested on the device on a widget
//! for the tooltip delay (`hover_delay`). The tray icons' tooltips carry the same text
//! ([`card_text`]).
//!
//! The card is a layered top-level popup of the UI thread (`WS_POPUP`; a tool window,
//! topmost, never activated and click-through), never a child of a taskbar: creating,
//! showing, moving and hiding it are calls on our own window that Explorer takes no part
//! in. It is created when first shown and painted from the snapshot and the history in
//! memory: no device traffic, no disk.

use std::time::Duration;

use batterysight_core::history;
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

    /// Paints `lines` ([`card_text`]) at the DPI of `hover`'s widget and shows the card
    /// above `hover`'s taskbar, on its monitor ([`place`]). A card that cannot be drawn is
    /// hidden.
    pub fn show(&mut self, hover: &Hover, lines: &[String], light: bool) {
        let painter = render::card(lines, hover.dpi, light);
        let (Some(painter), Some((monitor, work))) = (painter, monitor_of(&hover.bar)) else {
            self.hide();
            return;
        };
        let Some(hwnd) = self.window() else {
            return;
        };
        let size = painter.size();
        // Close to the taskbar, like a tooltip.
        let gap = 4 * hover.dpi as i32 / 96;
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

/// The card's two lines: "name · level" (a coarse level as its word, an unknown one as
/// "?"), then the status: "Charging" or "Full" on power; else, awake, "Connected" for a
/// device that reports its battery changes on its own, or how long ago it was last read
/// ("just now", "N min ago"…); after "Asleep · " or "Disconnected · ", how long ago it was
/// last read; and then " · about N h left" when there is an estimate. `now`: the monotonic
/// time of the readings.
pub fn card_text(state: &DeviceState, time_left: Option<Duration>, now: Ms) -> Vec<String> {
    let level = state.level.map_or_else(|| "?".to_owned(), level_text);
    let age = || age_text(now.saturating_sub(state.updated_ms));
    let mut status = match (state.presence, state.charge) {
        (Presence::Online, Charge::Charging) => "Charging".to_owned(),
        (Presence::Online, Charge::Full) => "Full".to_owned(),
        (Presence::Online, _) if state.reports_changes => "Connected".to_owned(),
        (Presence::Online, _) => age(),
        (Presence::Asleep, _) => format!("Asleep · {}", age()),
        (Presence::Offline, _) => format!("Disconnected · {}", age()),
    };
    let on_power = state.presence == Presence::Online
        && matches!(
            state.charge,
            Charge::Charging | Charge::Full | Charge::NotCharging
        );
    if let Some(left) = time_left.filter(|_| !on_power) {
        match history::left_hours(left) {
            0 => status.push_str(" · less than 1 h left"),
            hours => status.push_str(&format!(" · about {hours} h left")),
        }
    }
    vec![format!("{} · {level}", state.name), status]
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
        Charge, CoarseLevel, DeviceKey, DeviceKind, Level, Presence, Source, Verification, Via,
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
            poll_min_ms: None,
            reports_changes: false,
        }
    }

    #[test]
    fn card_text_awake_shows_age_and_time_left() {
        let s = state(Presence::Online, Charge::Discharging, NOW - 3 * MIN);
        let left = Some(Duration::from_secs(20 * 3600));
        assert_eq!(
            card_text(&s, left, NOW),
            ["G305 · 80%", "3 min ago · about 20 h left"]
        );
        let at = |ago, left| card_text(&s, left, s.updated_ms + ago)[1].clone();
        assert_eq!(at(59_000, left), "just now · about 20 h left");
        assert_eq!(at(125 * MIN, left), "2 h ago · about 20 h left");
        // No estimate: no text for it.
        assert_eq!(at(3 * MIN, None), "3 min ago");
        assert_eq!(
            at(3 * MIN, Some(Duration::from_secs(20 * 60))),
            "3 min ago · less than 1 h left"
        );
    }

    #[test]
    fn card_text_asleep_shows_age() {
        let s = state(Presence::Asleep, Charge::Discharging, NOW - 12 * MIN);
        assert_eq!(
            card_text(&s, None, NOW),
            ["G305 · 80%", "Asleep · 12 min ago"]
        );
        let nine = Some(Duration::from_secs(9 * 3600));
        assert_eq!(
            card_text(&s, nine, NOW)[1],
            "Asleep · 12 min ago · about 9 h left"
        );
        let at = |ago| card_text(&s, None, s.updated_ms + ago)[1].clone();
        assert_eq!(at(3 * 24 * 60 * MIN), "Asleep · 3 days ago");
    }

    #[test]
    fn card_text_on_power_has_no_time_left() {
        let left = Some(Duration::from_secs(9 * 3600));
        let line = |charge| {
            card_text(&state(Presence::Online, charge, NOW - 3 * MIN), left, NOW)[1].clone()
        };
        assert_eq!(line(Charge::Charging), "Charging");
        assert_eq!(line(Charge::Full), "Full");
        // Plugged in but not charging: its age, still no estimate.
        assert_eq!(line(Charge::NotCharging), "3 min ago");
    }

    #[test]
    fn card_text_disconnected_shows_age() {
        let s = state(Presence::Offline, Charge::Discharging, NOW - 7 * MIN);
        assert_eq!(card_text(&s, None, NOW)[1], "Disconnected · 7 min ago");
    }

    /// A device that reports its battery changes on its own, such as a Nova headset.
    fn reporting(presence: Presence, charge: Charge, updated_ms: Ms) -> DeviceState {
        DeviceState {
            reports_changes: true,
            ..state(presence, charge, updated_ms)
        }
    }

    #[test]
    fn card_text_reporting_device_shows_connected_and_time_left() {
        let s = reporting(Presence::Online, Charge::Discharging, NOW - 3 * 60 * MIN);
        let left = Some(Duration::from_secs(20 * 3600));
        assert_eq!(
            card_text(&s, left, NOW),
            ["G305 · 80%", "Connected · about 20 h left"]
        );
        assert_eq!(
            card_text(&s, Some(Duration::from_secs(20 * 60)), NOW)[1],
            "Connected · less than 1 h left"
        );
        assert_eq!(card_text(&s, None, NOW)[1], "Connected");
    }

    #[test]
    fn card_text_wake_only_mouse_keeps_its_age() {
        // Read only when it wakes (a HyperX universal mouse): no push, so its age.
        let s = state(Presence::Online, Charge::Discharging, NOW - 42 * MIN);
        assert!(!s.reports_changes);
        assert_eq!(card_text(&s, None, NOW)[1], "42 min ago");
        let left = Some(Duration::from_secs(5 * 3600));
        assert_eq!(card_text(&s, left, NOW)[1], "42 min ago · about 5 h left");
    }

    #[test]
    fn card_text_os_cached_bluetooth_shows_connected_not_its_age() {
        // Windows gives no measurement time: `updated_ms` is when the value was first
        // seen, so an unchanged level looks days old while the device is connected.
        let s = DeviceState {
            key: DeviceKey("bt:00000000000000000000000000000001".into()),
            name: "Buds".into(),
            kind: DeviceKind::Headset,
            via: Via::Bluetooth,
            source: Source::Os,
            verification: Verification::SourceVerified,
            ..reporting(Presence::Online, Charge::Unknown, NOW - 49 * 60 * MIN)
        };
        assert_eq!(card_text(&s, None, NOW), ["Buds · 80%", "Connected"]);
    }

    #[test]
    fn card_text_reporting_device_keeps_other_statuses() {
        let left = Some(Duration::from_secs(9 * 3600));
        let line = |presence, charge| {
            card_text(&reporting(presence, charge, NOW - 12 * MIN), left, NOW)[1].clone()
        };
        assert_eq!(line(Presence::Online, Charge::Charging), "Charging");
        assert_eq!(line(Presence::Online, Charge::Full), "Full");
        // On power but not charging: connected, still no estimate.
        assert_eq!(line(Presence::Online, Charge::NotCharging), "Connected");
        assert_eq!(
            line(Presence::Asleep, Charge::Discharging),
            "Asleep · 12 min ago · about 9 h left"
        );
        assert_eq!(
            card_text(
                &reporting(Presence::Offline, Charge::Charging, NOW - 7 * MIN),
                None,
                NOW
            )[1],
            "Disconnected · 7 min ago"
        );
    }

    #[test]
    fn card_text_levels() {
        let mut s = state(Presence::Online, Charge::Discharging, NOW);
        s.level = Some(Level::Coarse(CoarseLevel::Good));
        assert_eq!(card_text(&s, None, NOW)[0], "G305 · Good");
        s.level = None;
        assert_eq!(card_text(&s, None, NOW)[0], "G305 · ?");
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
