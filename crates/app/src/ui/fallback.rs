//! Why the session shows tray icons instead of the widget, and whether a taskbar or display
//! change should try the widget again.
//!
//! Each fallback is traced (`BATTERYSIGHT_TRACE=1`) as `"<unix ms> ui fallback <reason>"`.

use batterysight_win::trace::ui_line;

/// Why tray icons show the devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayReason {
    /// The user chose tray icons.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no setting offers tray icons yet")
    )]
    UserChoice,
    /// Started with `--tray-only`.
    TrayOnlyFlag,
    /// Started by the widget watchdog after the widget thread stalled.
    Watchdog,
    /// Windows refused a widget window.
    CreateFailed,
    /// A taskbar exists but no widget was shown on any.
    NoAnchor,
}

/// Whether a taskbar or display change should try the widget again. Only for the failures
/// that may pass (Explorer or the displays were not ready); never after a watchdog relaunch,
/// or a widget that stalls again would loop.
pub fn should_retry_widget(reason: TrayReason) -> bool {
    matches!(reason, TrayReason::CreateFailed | TrayReason::NoAnchor)
}

/// The reason to start in tray mode, from the flags: `--tray-only`, which the watchdog's
/// relaunch also passes along with `--takeover`. `None` starts the widget.
pub fn start_reason(tray_only: bool, takeover: bool) -> Option<TrayReason> {
    match (tray_only, takeover) {
        (false, _) => None,
        (true, false) => Some(TrayReason::TrayOnlyFlag),
        (true, true) => Some(TrayReason::Watchdog),
    }
}

/// Traces the fallback (`BATTERYSIGHT_TRACE=1` only).
pub fn trace(reason: TrayReason) {
    ui_line(&trace_text(reason));
}

fn trace_text(reason: TrayReason) -> String {
    format!("fallback {reason:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_only_after_create_failed_or_no_anchor() {
        assert!(should_retry_widget(TrayReason::CreateFailed));
        assert!(should_retry_widget(TrayReason::NoAnchor));
        assert!(!should_retry_widget(TrayReason::UserChoice));
        assert!(!should_retry_widget(TrayReason::TrayOnlyFlag));
        assert!(!should_retry_widget(TrayReason::Watchdog));
    }

    #[test]
    fn start_reason_from_flags() {
        assert_eq!(start_reason(false, false), None);
        assert_eq!(start_reason(false, true), None);
        assert_eq!(start_reason(true, false), Some(TrayReason::TrayOnlyFlag));
        // The watchdog's relaunch: never retried, or a stalled widget could loop.
        assert_eq!(start_reason(true, true), Some(TrayReason::Watchdog));
    }

    #[test]
    fn trace_text_names_the_reason() {
        assert_eq!(trace_text(TrayReason::NoAnchor), "fallback NoAnchor");
        assert_eq!(
            trace_text(TrayReason::CreateFailed),
            "fallback CreateFailed"
        );
    }
}
