//! Widget watchdog.
//!
//! The widget's thread shares its input queue with Explorer's taskbar thread. The spike
//! showed that a click on the widget while that thread is stalled freezes all mouse input to
//! the taskbar, and that neither `WM_NULL` round trips nor `IsHungAppWindow` notice. So the
//! widget thread stores a heartbeat ([`beat`]) on every message-loop iteration and on a
//! 500 ms timer, and a thread that owns no windows checks it every 500 ms. When the beat has
//! not changed for 1.5 s while Explorer's taskbar thread still answers (a busy Explorer is
//! not our stall), or for 5 s whatever Explorer does (its taskbar thread may itself be
//! waiting on our stalled thread), it starts `--tray-only --takeover` and ends this process
//! with `ExitProcess`, which destroys the widget windows and releases Explorer. Only time
//! between checks that ran on time counts ([`Staleness`]), so sleep never trips it.

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
/// The longest gap between two checks that still counts as on time.
const ON_TIME_GAP_MS: Ms = 2 * INTERVAL_MS as Ms;
/// How long Explorer's taskbar thread gets to answer `WM_NULL`.
const EXPLORER_ANSWER_MS: u32 = 200;
/// Exit code of a process the watchdog ended.
const TRIPPED_EXIT_CODE: u32 = 3;

const WATCHDOG: Watchdog = Watchdog {
    limit_ms: 1_500,
    hard_limit_ms: 5_000,
};

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
    pub hard_limit_ms: Ms,
}

impl Watchdog {
    /// `Trip` when the widget thread has not beaten for more than `hard_limit_ms`, or more
    /// than `limit_ms` while `explorer_responsive` says Explorer's taskbar thread answers
    /// (asked only then). `stale_ms` comes from [`Staleness::observe`].
    pub fn check(&self, stale_ms: Ms, explorer_responsive: impl FnOnce() -> bool) -> Verdict {
        if stale_ms > self.hard_limit_ms || (stale_ms > self.limit_ms && explorer_responsive()) {
            Verdict::Trip
        } else {
            Verdict::Ok
        }
    }
}

/// How long the widget thread has not beaten, counted only between checks that ran on time.
///
/// The clock includes sleep, and this thread can be starved like any other. A gap between
/// two checks longer than [`ON_TIME_GAP_MS`] says nothing about the widget thread (after a
/// resume it has not had its chance to beat yet), so such a gap is not counted.
pub struct Staleness {
    last_check: Ms,
    beat: Ms,
    stale_ms: Ms,
}

impl Staleness {
    pub fn new(now: Ms, beat: Ms) -> Self {
        Staleness {
            last_check: now,
            beat,
            stale_ms: now.saturating_sub(beat),
        }
    }

    /// Records a check at `now` that read `beat` and returns the staleness it saw.
    pub fn observe(&mut self, now: Ms, beat: Ms) -> Ms {
        let gap = now.saturating_sub(self.last_check);
        self.last_check = now;
        let on_time = gap <= ON_TIME_GAP_MS;
        if beat != self.beat {
            self.beat = beat;
            // The beat came within the gap; after a long one, when is unknown.
            self.stale_ms = if on_time {
                now.saturating_sub(beat).min(gap)
            } else {
                0
            };
        } else if on_time {
            self.stale_ms += gap;
        }
        self.stale_ms
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
    let mut staleness = Staleness::new(now_ms(), HEARTBEAT.load(Ordering::Relaxed));
    loop {
        thread::sleep(Duration::from_millis(Ms::from(INTERVAL_MS)));
        // Observed even while disarmed, so arming starts from an up-to-date state.
        let stale = staleness.observe(now_ms(), HEARTBEAT.load(Ordering::Relaxed));
        if ARMED.load(Ordering::Relaxed) && WATCHDOG.check(stale, explorer_answers) == Verdict::Trip
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

    const DOG: Watchdog = Watchdog {
        limit_ms: 1_500,
        hard_limit_ms: 5_000,
    };

    fn responsive() -> bool {
        true
    }

    fn unresponsive() -> bool {
        false
    }

    #[test]
    fn no_trip_within_limit() {
        assert_eq!(DOG.check(1_400, responsive), Verdict::Ok);
        assert_eq!(DOG.check(1_500, responsive), Verdict::Ok);
    }

    #[test]
    fn trips_after_limit() {
        assert_eq!(DOG.check(1_501, responsive), Verdict::Trip);
    }

    #[test]
    fn stale_but_explorer_unresponsive_is_ok() {
        assert_eq!(DOG.check(1_501, unresponsive), Verdict::Ok);
        assert_eq!(DOG.check(5_000, unresponsive), Verdict::Ok);
    }

    #[test]
    fn trips_after_hard_limit_whatever_explorer_does() {
        assert_eq!(DOG.check(5_001, unresponsive), Verdict::Trip);
    }

    #[test]
    fn explorer_is_asked_only_when_stale() {
        let ask = || -> bool { panic!("asked Explorer while the beat is fresh") };
        assert_eq!(DOG.check(1_500, ask), Verdict::Ok);
        assert_eq!(DOG.check(5_001, ask), Verdict::Trip);
    }

    #[test]
    fn beat_after_now_is_fresh() {
        assert_eq!(Staleness::new(0, 0).observe(600, 2_000), 0);
    }

    /// The staleness seen by checks at each of `checks` while the beat stays at `beat`.
    fn run(staleness: &mut Staleness, checks: &[Ms], beat: Ms) -> Vec<Ms> {
        checks
            .iter()
            .map(|&now| staleness.observe(now, beat))
            .collect()
    }

    #[test]
    fn on_time_checks_count_the_unchanged_beat_as_its_age() {
        let mut s = Staleness::new(1_000, 600);
        assert_eq!(
            run(&mut s, &[1_500, 2_000, 2_500], 600),
            [900, 1_400, 1_900]
        );
        assert_eq!(DOG.check(1_900, responsive), Verdict::Trip);
    }

    #[test]
    fn a_new_beat_restarts_the_count() {
        let mut s = Staleness::new(1_000, 600);
        assert_eq!(s.observe(1_500, 600), 900);
        assert_eq!(s.observe(2_000, 1_800), 200);
        assert_eq!(s.observe(2_500, 1_800), 700);
    }

    #[test]
    fn sleep_between_checks_is_not_counted() {
        // Beat at 500, checks at 500 and 1 000; then the PC sleeps for an hour.
        let mut s = Staleness::new(500, 500);
        assert_eq!(s.observe(1_000, 500), 500);
        let resumed = 3_601_000;
        assert_eq!(s.observe(resumed, 500), 500);
        assert_eq!(DOG.check(500, unresponsive), Verdict::Ok);
        // The widget thread misses a round after resume: still no trip.
        assert_eq!(s.observe(resumed + 500, 500), 1_000);
        assert_eq!(DOG.check(1_000, responsive), Verdict::Ok);
        // A real stall after resume still trips, once on-time checks add up.
        let later = run(&mut s, &[resumed + 1_000, resumed + 1_500], 500);
        assert_eq!(later, [1_500, 2_000]);
        assert_eq!(DOG.check(2_000, responsive), Verdict::Trip);
    }

    #[test]
    fn starved_checks_are_not_counted() {
        let mut s = Staleness::new(0, 0);
        assert_eq!(s.observe(500, 0), 500);
        // This thread ran 1.2 s late: the gap says nothing about the widget thread.
        assert_eq!(s.observe(1_700, 0), 500);
        assert_eq!(s.observe(2_200, 0), 1_000);
    }

    #[test]
    fn a_beat_during_a_long_gap_restarts_from_zero() {
        let mut s = Staleness::new(0, 0);
        // Beat just before the PC slept, first check after resume.
        assert_eq!(s.observe(3_600_000, 100), 0);
        assert_eq!(s.observe(3_600_500, 100), 500);
    }
}
