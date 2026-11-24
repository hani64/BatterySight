//! Low-battery notifications: a warning balloon from a notification-area icon, which
//! Windows 10 and 11 show as a toast. No AppUserModelID or Start-menu shortcut is needed.
//!
//! In tray mode the balloon comes from the alerted device's own icon. Without one (the
//! widget shows the devices, or the device's icon is missing) a temporary icon
//! ([`TEMP_ICON_ID`]) carries it and is removed [`TEMP_ICON_MS`] after the last
//! notification ([`NOTIFY_TIMER`]), on a switch between widget and tray icons, after
//! `TaskbarCreated` and on exit, so it never lingers. When the watchdog ends the process,
//! it deletes the icon first if Explorer answers ([`delete_on_trip`]).
//!
//! Runs on the UI thread, except [`delete_on_trip`]; `Shell_NotifyIconW` is one of the
//! bounded Explorer calls allowed there (see `tray`).

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{HICON, KillTimer, SetTimer};

use super::tray::{Action, NotifyArea, Shell};
use super::{DeviceView, NOTIFY_TIMER};
use crate::alerts::Alert;

/// Id of the temporary icon (the device icons use 1, 2, …).
pub const TEMP_ICON_ID: u32 = 900;
/// How long the temporary icon stays after its last notification.
pub const TEMP_ICON_MS: u32 = 60_000;
/// Title of every notification.
const TITLE: &str = "Low battery";
/// Tooltip of a temporary icon drawn without a device.
const TEMP_TIP: &str = "BatterySight";

/// Whether the notification area has the temporary icon; read by the watchdog's thread.
static TEMP_SHOWN: AtomicBool = AtomicBool::new(false);

/// Shows notifications for the host window; owns the temporary icon.
pub struct Notifier {
    shell: NotifyArea,
    /// The temporary icon while the notification area has it.
    temp: Option<HICON>,
}

impl Notifier {
    pub fn new(host: HWND) -> Notifier {
        Notifier {
            shell: NotifyArea { host },
            temp: None,
        }
    }

    /// Shows `alerts` as one notification from the tray icon `on` or, without it or when
    /// it fails, from the temporary icon, drawn like `view` (the first alerted device).
    pub fn show(
        &mut self,
        alerts: &[Alert],
        on: Option<u32>,
        view: Option<&DeviceView>,
        light: bool,
    ) {
        if show(
            &mut self.temp,
            &mut self.shell,
            &text(alerts),
            on,
            view,
            light,
        ) {
            // SAFETY: our own window; setting an existing timer id restarts it.
            let timer =
                unsafe { SetTimer(Some(self.shell.host), NOTIFY_TIMER, TEMP_ICON_MS, None) };
            // Without its timer nothing would remove the icon: the balloon goes with it.
            if timer == 0 {
                self.remove();
                return;
            }
        }
        TEMP_SHOWN.store(self.temp.is_some(), Ordering::Relaxed);
    }

    /// Removes the temporary icon, if any.
    pub fn remove(&mut self) {
        // SAFETY: our own window and timer (killing one that is not set fails harmlessly).
        let _ = unsafe { KillTimer(Some(self.shell.host), NOTIFY_TIMER) };
        remove(&mut self.temp, &mut self.shell);
        TEMP_SHOWN.store(false, Ordering::Relaxed);
    }
}

/// The watchdog is about to end the process: deletes the temporary icon of `host`, if
/// shown and `explorer_answers`, so it does not linger beside the replacement's icons until
/// hovered. Best effort, from the watchdog's thread; the icon's handle goes with the
/// process.
pub fn delete_on_trip(host: HWND, explorer_answers: impl FnOnce() -> bool) {
    if TEMP_SHOWN.load(Ordering::Relaxed) && explorer_answers() {
        NotifyArea { host }.notify(Action::Delete, TEMP_ICON_ID, None);
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The notification text: one line per alert.
fn text(alerts: &[Alert]) -> String {
    let lines: Vec<String> = alerts.iter().map(Alert::text).collect();
    lines.join("\n")
}

/// Shows `text` from icon `on`, else from the temporary icon (added once, drawn like
/// `view`). Returns whether the temporary icon carries it.
fn show(
    temp: &mut Option<HICON>,
    shell: &mut impl Shell,
    text: &str,
    on: Option<u32>,
    view: Option<&DeviceView>,
    light: bool,
) -> bool {
    if on.is_some_and(|id| shell.balloon(id, TITLE, text)) {
        return false;
    }
    if temp.is_none() {
        let Some(icon) = shell.render(view, light) else {
            return false;
        };
        let tip = view.map_or(TEMP_TIP, |v| v.tooltip.as_str());
        if !shell.notify(Action::Add, TEMP_ICON_ID, Some((icon, tip))) {
            // The add may have half succeeded (icon added, version refused).
            shell.notify(Action::Delete, TEMP_ICON_ID, None);
            shell.destroy(icon);
            return false;
        }
        *temp = Some(icon);
    }
    // Refused or not, the icon is removed on schedule.
    shell.balloon(TEMP_ICON_ID, TITLE, text);
    true
}

/// Removes the temporary icon from the notification area and destroys it.
fn remove(temp: &mut Option<HICON>, shell: &mut impl Shell) {
    if let Some(icon) = temp.take() {
        shell.notify(Action::Delete, TEMP_ICON_ID, None);
        shell.destroy(icon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tray::tests::{FakeShell, keyed};
    use batterysight_core::model::DeviceKey;

    fn alert(name: &str, percent: Option<u8>) -> Alert {
        Alert {
            key: DeviceKey(name.into()),
            name: name.into(),
            percent,
        }
    }

    #[test]
    fn one_line_per_alert() {
        let alerts = [alert("Mouse", Some(10)), alert("Keyboard", None)];
        assert_eq!(text(&alerts), "Mouse is at 10 %\nKeyboard is low");
    }

    #[test]
    fn a_device_icon_carries_the_balloon() {
        let mut shell = FakeShell::default();
        let mut temp = None;
        let view = keyed("M", "10%");
        assert!(!show(
            &mut temp,
            &mut shell,
            "M is at 10 %",
            Some(2),
            Some(&view),
            false
        ));
        assert_eq!(shell.balloons, [(2, "M is at 10 %".to_owned())]);
        assert!(shell.calls.is_empty());
        assert!(temp.is_none());
    }

    #[test]
    fn temporary_icon_is_added_once_and_removed() {
        let mut shell = FakeShell::default();
        let mut temp = None;
        let view = keyed("M", "10%");
        assert!(show(
            &mut temp,
            &mut shell,
            "first",
            None,
            Some(&view),
            false
        ));
        assert!(show(
            &mut temp,
            &mut shell,
            "second",
            None,
            Some(&view),
            false
        ));
        assert_eq!(
            shell.calls,
            [(Action::Add, TEMP_ICON_ID, Some("M — 10%".to_owned()))]
        );
        assert_eq!(
            shell.balloons,
            [
                (TEMP_ICON_ID, "first".to_owned()),
                (TEMP_ICON_ID, "second".to_owned())
            ]
        );
        remove(&mut temp, &mut shell);
        remove(&mut temp, &mut shell);
        assert_eq!(shell.calls[1..], [(Action::Delete, TEMP_ICON_ID, None)]);
        assert_eq!(shell.destroyed, [1]);
        assert!(temp.is_none());
    }

    #[test]
    fn refused_device_icon_falls_back_to_the_temporary_icon() {
        let mut shell = FakeShell {
            refused: vec![2],
            ..FakeShell::default()
        };
        let mut temp = None;
        assert!(show(&mut temp, &mut shell, "low", Some(2), None, false));
        assert_eq!(
            shell.calls,
            [(Action::Add, TEMP_ICON_ID, Some(TEMP_TIP.to_owned()))]
        );
        assert_eq!(shell.balloons, [(TEMP_ICON_ID, "low".to_owned())]);
    }

    #[test]
    fn refused_temporary_icon_leaves_nothing_behind() {
        let mut shell = FakeShell {
            refused: vec![TEMP_ICON_ID],
            ..FakeShell::default()
        };
        let mut temp = None;
        assert!(!show(&mut temp, &mut shell, "low", None, None, false));
        // A half-done add (icon added, version refused) is deleted too.
        assert_eq!(
            shell.calls,
            [
                (Action::Add, TEMP_ICON_ID, Some(TEMP_TIP.to_owned())),
                (Action::Delete, TEMP_ICON_ID, None)
            ]
        );
        assert_eq!(shell.destroyed, [1]);
        assert!(temp.is_none());
    }
}
