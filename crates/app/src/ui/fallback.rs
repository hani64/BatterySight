//! Why the session shows tray icons instead of the widget, and whether a taskbar or display
//! change should try the widget again.
//!
//! Each fallback is traced to `sends.log` as `"<unix ms> ui fallback <reason>"`.

use batterysight_win::trace::ui_line;

/// Why tray icons show the devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayReason {
    /// The user chose tray icons.
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

/// Whether the widget should be tried again: on a taskbar or display change, and on the
/// self-retry schedule ([`next_self_retry`]). Only for the failures that may pass (Explorer
/// or the displays were not ready); never after a watchdog relaunch, or a widget that stalls
/// again would loop.
pub fn should_retry_widget(reason: TrayReason) -> bool {
    matches!(reason, TrayReason::CreateFailed | TrayReason::NoAnchor)
}

/// When self-retry number `attempts_done + 1` is due, in milliseconds after the fallback:
/// the widget is tried again 15 s, 60 s and 300 s after it, without waiting for a taskbar
/// or display change, then no more.
pub fn next_self_retry(attempts_done: u8) -> Option<u32> {
    match attempts_done {
        0 => Some(15_000),
        1 => Some(60_000),
        2 => Some(300_000),
        _ => None,
    }
}

/// How long from now the next self-retry is due, `elapsed_ms` after the fallback; 0 when
/// it is already due.
pub fn self_retry_delay(attempts_done: u8, elapsed_ms: u64) -> Option<u32> {
    let elapsed = u32::try_from(elapsed_ms).unwrap_or(u32::MAX);
    next_self_retry(attempts_done).map(|at| at.saturating_sub(elapsed))
}

/// The reason to start in tray mode, from the flags (`--tray-only`, which the watchdog's
/// relaunch also passes along with `--takeover`), then from the user's choice of tray
/// icons (`user_tray`). `None` starts the widget.
pub fn start_reason(tray_only: bool, takeover: bool, user_tray: bool) -> Option<TrayReason> {
    match (tray_only, takeover) {
        (false, _) => user_tray.then_some(TrayReason::UserChoice),
        (true, false) => Some(TrayReason::TrayOnlyFlag),
        (true, true) => Some(TrayReason::Watchdog),
    }
}

/// Traces the fallback to `sends.log`.
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
        assert_eq!(start_reason(false, false, false), None);
        assert_eq!(start_reason(false, true, false), None);
        assert_eq!(
            start_reason(true, false, false),
            Some(TrayReason::TrayOnlyFlag)
        );
        // The watchdog's relaunch: never retried, or a stalled widget could loop.
        assert_eq!(start_reason(true, true, false), Some(TrayReason::Watchdog));
    }

    #[test]
    fn start_reason_from_the_user_choice() {
        assert_eq!(
            start_reason(false, false, true),
            Some(TrayReason::UserChoice)
        );
        // The flags say more about this session; neither is retried either.
        assert_eq!(start_reason(true, true, true), Some(TrayReason::Watchdog));
        assert_eq!(
            start_reason(true, false, true),
            Some(TrayReason::TrayOnlyFlag)
        );
    }

    #[test]
    fn trace_text_names_the_reason() {
        assert_eq!(trace_text(TrayReason::NoAnchor), "fallback NoAnchor");
        assert_eq!(
            trace_text(TrayReason::CreateFailed),
            "fallback CreateFailed"
        );
    }

    #[test]
    fn self_retries_at_15_60_and_300_s_then_stop() {
        assert_eq!(next_self_retry(0), Some(15_000));
        assert_eq!(next_self_retry(1), Some(60_000));
        assert_eq!(next_self_retry(2), Some(300_000));
        assert_eq!(next_self_retry(3), None);
        assert_eq!(next_self_retry(u8::MAX), None);
    }

    #[test]
    fn self_retry_delay_counts_from_the_fallback() {
        assert_eq!(self_retry_delay(0, 0), Some(15_000));
        // Attempt 1 ran 15 s after the fallback: the next is 60 s after it, not after now.
        assert_eq!(self_retry_delay(1, 15_000), Some(45_000));
        assert_eq!(self_retry_delay(2, 61_000), Some(239_000));
        // Already due (the machine slept, or the thread was busy): at once.
        assert_eq!(self_retry_delay(1, 90_000), Some(0));
        assert_eq!(self_retry_delay(2, u64::MAX), Some(0));
        assert_eq!(self_retry_delay(3, 0), None);
    }
}
