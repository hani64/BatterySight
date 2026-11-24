//! Widget watchdog.
//!
//! The widget's thread shares its input queue with Explorer's taskbar thread. The spike
//! showed that a click on the widget while that thread is stalled freezes all mouse input to
//! the taskbar, and that neither `WM_NULL` round trips nor `IsHungAppWindow` notice. So the
//! widget thread stores a heartbeat ([`beat`]) on every message-loop iteration and on a
//! 500 ms timer, and a thread that owns no windows checks it every 500 ms. When the beat is
//! older than 1.5 s while Explorer's taskbar thread still answers (a busy Explorer is not our
//! stall), it starts `--tray-only --takeover` and ends this process with `ExitProcess`,
//! which destroys the widget windows and releases Explorer.

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use batterysight_core::time::Ms;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::ExitProcess;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_NULL,
};
use windows::core::{PCWSTR, w};

/// How often the widget thread's timer beats and the watchdog checks.
pub const INTERVAL_MS: u32 = 500;
/// How long Explorer's taskbar thread gets to answer `WM_NULL`.
const EXPLORER_ANSWER_MS: u32 = 200;
/// Exit code of a process the watchdog ended.
const TRIPPED_EXIT_CODE: u32 = 3;

const WATCHDOG: Watchdog = Watchdog { limit_ms: 1_500 };

/// Last heartbeat of the widget thread, in [`now_ms`] milliseconds.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
/// Checks run only while a widget exists: tray icons do not share Explorer's input queue,
/// and `RuntimeHandle::stop` may block the thread for up to 2 s on exit.
static ARMED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Trip,
}

pub struct Watchdog {
    pub limit_ms: Ms,
}

impl Watchdog {
    pub fn check(&self, now: Ms, last_beat: Ms) -> Verdict {
        if now.saturating_sub(last_beat) > self.limit_ms {
            Verdict::Trip
        } else {
            Verdict::Ok
        }
    }
}

/// Monotonic milliseconds since boot, sleep included.
fn now_ms() -> Ms {
    // SAFETY: no arguments.
    unsafe { GetTickCount64() }
}

/// Records that the widget thread is responsive. Called only on that thread.
pub fn beat() {
    HEARTBEAT.store(now_ms(), Ordering::Relaxed);
}

/// Starts or stops the checks; arming also beats, so the check starts fresh.
pub fn arm(armed: bool) {
    beat();
    ARMED.store(armed, Ordering::Relaxed);
}

/// Starts the watchdog thread. It owns no windows and runs until the process ends.
pub fn spawn() {
    let _ = thread::Builder::new()
        .name("batterysight-watchdog".into())
        .spawn(watch);
}

fn watch() {
    let interval = Ms::from(INTERVAL_MS);
    let mut last_check = now_ms();
    loop {
        thread::sleep(Duration::from_millis(interval));
        let now = now_ms();
        // After a long gap (the PC slept or this thread was starved) the widget thread has
        // not had its chance to beat yet: check again next round.
        let resumed = now - last_check > 2 * interval;
        last_check = now;
        if resumed || !ARMED.load(Ordering::Relaxed) {
            continue;
        }
        if WATCHDOG.check(now, HEARTBEAT.load(Ordering::Relaxed)) == Verdict::Trip
            && explorer_answers()
        {
            trip();
        }
    }
}

/// Whether Explorer's taskbar thread answers a `WM_NULL` within 200 ms. Sent from this
/// thread, which owns no windows, so it cannot be caught in the widget thread's stall.
fn explorer_answers() -> bool {
    // SAFETY: literal class name; `result` outlives the call.
    unsafe {
        let Ok(taskbar) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) else {
            return false;
        };
        let mut result = 0usize;
        SendMessageTimeoutW(
            taskbar,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            EXPLORER_ANSWER_MS,
            Some(&mut result),
        )
        .0 != 0
    }
}

/// Starts the tray-only replacement and ends this process at once.
fn trip() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .args(["--tray-only", "--takeover"])
            .spawn();
    }
    // SAFETY: ends the process; nothing runs afterwards. Explorer's taskbar input matters
    // more than an orderly shutdown, so this also runs when the relaunch failed.
    unsafe { ExitProcess(TRIPPED_EXIT_CODE) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOG: Watchdog = Watchdog { limit_ms: 1_500 };

    #[test]
    fn no_trip_within_limit() {
        assert_eq!(DOG.check(2_000, 600), Verdict::Ok);
        assert_eq!(DOG.check(2_100, 600), Verdict::Ok);
    }

    #[test]
    fn trips_after_limit() {
        assert_eq!(DOG.check(2_101, 600), Verdict::Trip);
    }

    #[test]
    fn beat_after_now_is_ok() {
        assert_eq!(DOG.check(600, 2_000), Verdict::Ok);
    }
}
