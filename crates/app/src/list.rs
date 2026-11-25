//! `batterysight --list`: prints the devices the runtime finds, one per line.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::sync::PoisonError;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use batterysight_core::catalog::{Catalog, SteelseriesReport};
use batterysight_core::engines::hidpp::engine::DEVICE_TIMEOUT_MS;
use batterysight_core::model::{
    Charge, CoarseLevel, DeviceState, Level, Presence, Verification, Via,
};
use batterysight_win::{hid_enum, runtime};

/// The list is printed once no change arrived for this long... Longer than the
/// HID++ device timeout, so a device whose first answer comes late, or a direct
/// device registered asleep only when its first read times out, still shows.
const QUIET: Duration = Duration::from_millis(DEVICE_TIMEOUT_MS as u64 + 1_000);
/// ...or once this much time has passed since the runtime started.
const MAX_WAIT: Duration = Duration::from_secs(10);
/// While a Nova base station is present, the list is not printed before this: the station
/// is first asked 5 s after it is opened (it needs that long to settle) and its reply is
/// expected within 2 s; the last second covers opening the devices.
const STATION_FIRST_READING: Duration = Duration::from_secs(8);

/// Starts the runtime, waits for the readings to settle and prints one line per device.
/// Returns the process exit code.
pub fn run() -> i32 {
    let catalog = Catalog::builtin();
    // Cached attributes only, as the runtime itself reads them; nothing is sent.
    let station = hid_enum::enumerate().is_some_and(|interfaces| {
        interfaces
            .iter()
            .any(|i| is_station(&catalog, i.collection.vid, i.collection.pid))
    });
    let floor = if station {
        STATION_FIRST_READING
    } else {
        Duration::ZERO
    };
    let (changed, changes) = mpsc::channel();
    let on_change = Box::new(move || {
        let _ = changed.send(());
    });
    let runtime = match runtime::start(on_change) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(io::stderr(), "batterysight: cannot start: {error}");
            return 1;
        }
    };
    let started = Instant::now();
    settle(
        floor,
        || started.elapsed(),
        |timeout| changes.recv_timeout(timeout).is_ok(),
    );
    let states = runtime
        .snapshot
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    runtime.stop();
    let mut out = io::stdout().lock();
    for state in &states {
        let _ = writeln!(out, "{}", format_line(state));
    }
    let _ = out.flush();
    0
}

/// Whether `vid:pid` is a Nova base station, whose first reading comes only after its
/// settle.
fn is_station(catalog: &Catalog, vid: u16, pid: u16) -> bool {
    catalog.devices().iter().any(|entry| {
        entry.vid == vid
            && entry.pids.contains(&pid)
            && matches!(
                entry.steelseries,
                Some(SteelseriesReport::Station06Steps | SteelseriesReport::Station01Percent)
            )
    })
}

/// Waits until no change arrived for [`QUIET`], but not before `floor`, and never past
/// [`MAX_WAIT`]. `elapsed`: the time since the runtime started; `change(timeout)` waits up
/// to `timeout` for a change and says whether one came.
fn settle(
    floor: Duration,
    elapsed: impl Fn() -> Duration,
    mut change: impl FnMut(Duration) -> bool,
) {
    loop {
        let now = elapsed();
        let Some(left) = MAX_WAIT.checked_sub(now) else {
            break;
        };
        let quiet = QUIET.max(floor.saturating_sub(now));
        if !change(quiet.min(left)) {
            break;
        }
    }
}

/// One device as `{name}  [{via}]  {level}{charge}  ({key})  {status}`, with ` asleep`
/// appended for a sleeping device.
pub fn format_line(s: &DeviceState) -> String {
    let via = match s.via {
        Via::Receiver => "receiver",
        Via::Usb => "USB",
        Via::Bluetooth => "Bluetooth",
    };
    let mut line = format!("{}  [{via}]  ", s.name);
    match s.level {
        Some(Level::Percent(percent)) => {
            let _ = write!(line, "{percent}%");
        }
        Some(Level::Coarse(coarse)) => line.push_str(match coarse {
            CoarseLevel::Critical => "Critical",
            CoarseLevel::Low => "Low",
            CoarseLevel::Good => "Good",
            CoarseLevel::Full => "Full",
        }),
        None => line.push('?'),
    }
    line.push_str(match s.charge {
        Charge::Charging => " charging",
        Charge::Full => " full",
        Charge::Discharging | Charge::NotCharging | Charge::Unknown => "",
    });
    let status = match s.verification {
        Verification::HardwareTested => "hardware-tested",
        Verification::SourceVerified => "source-verified",
        Verification::CommunityTested => "community-tested",
        Verification::Generic => "generic",
    };
    let _ = write!(line, "  ({})  {status}", s.key.0);
    if s.presence == Presence::Asleep {
        line.push_str(" asleep");
    }
    line
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use batterysight_core::model::{DeviceKey, DeviceKind, Source};

    fn state(name: &str, via: Via, level: Level, charge: Charge, key: &str) -> DeviceState {
        DeviceState {
            key: DeviceKey(key.into()),
            name: name.into(),
            kind: DeviceKind::Other,
            via,
            level: Some(level),
            millivolts: None,
            charge,
            presence: Presence::Online,
            updated_ms: 0,
            source: Source::Query,
            verification: Verification::SourceVerified,
            poll_min_ms: None,
            reports_changes: false,
        }
    }

    #[test]
    fn format_line_percent_charging() {
        let s = state(
            "PRO X Wireless Gaming Headset",
            Via::Usb,
            Level::Percent(68),
            Charge::Charging,
            "logi:0ABA:255",
        );
        assert_eq!(
            format_line(&s),
            "PRO X Wireless Gaming Headset  [USB]  68% charging  (logi:0ABA:255)  source-verified"
        );
    }

    #[test]
    fn format_line_asleep_coarse() {
        let s = DeviceState {
            presence: Presence::Asleep,
            ..state(
                "PRO X",
                Via::Receiver,
                Level::Coarse(CoarseLevel::Low),
                Charge::Discharging,
                "logi:unit:1A2B3C4D",
            )
        };
        assert_eq!(
            format_line(&s),
            "PRO X  [receiver]  Low  (logi:unit:1A2B3C4D)  source-verified asleep"
        );
    }

    #[test]
    fn format_line_unknown_level_full_bluetooth() {
        let s = DeviceState {
            level: None,
            verification: Verification::Generic,
            ..state(
                "Pad",
                Via::Bluetooth,
                Level::Percent(0),
                Charge::Full,
                "bt:1",
            )
        };
        assert_eq!(format_line(&s), "Pad  [Bluetooth]  ? full  (bt:1)  generic");
    }

    #[test]
    fn format_line_statuses() {
        for (verification, status) in [
            (Verification::HardwareTested, "hardware-tested"),
            (Verification::CommunityTested, "community-tested"),
        ] {
            let s = DeviceState {
                verification,
                ..state(
                    "M",
                    Via::Receiver,
                    Level::Coarse(CoarseLevel::Full),
                    Charge::NotCharging,
                    "k",
                )
            };
            assert_eq!(
                format_line(&s),
                format!("M  [receiver]  Full  (k)  {status}")
            );
        }
    }

    #[test]
    fn format_line_new_sources() {
        let line = |name, via, level, charge, key| format_line(&state(name, via, level, charge, key));
        let rows = [
            (
                line(
                    "SteelSeries Arctis Nova Pro Wireless",
                    Via::Receiver,
                    Level::Percent(87),
                    Charge::Charging,
                    "steelseries:12e0",
                ),
                "SteelSeries Arctis Nova Pro Wireless  [receiver]  87% charging  (steelseries:12e0)  source-verified",
            ),
            (
                line(
                    "HyperX Pulsefire Haste 2 Wireless",
                    Via::Receiver,
                    Level::Percent(54),
                    Charge::Discharging,
                    "hyperx:0f98",
                ),
                "HyperX Pulsefire Haste 2 Wireless  [receiver]  54%  (hyperx:0f98)  source-verified",
            ),
            (
                line(
                    "Pulsar Fusion2 1K Dongle",
                    Via::Receiver,
                    Level::Percent(70),
                    Charge::Unknown,
                    "sonix:5402",
                ),
                "Pulsar Fusion2 1K Dongle  [receiver]  70%  (sonix:5402)  source-verified",
            ),
            (
                line(
                    "Rapoo VT7",
                    Via::Usb,
                    Level::Percent(65),
                    Charge::Unknown,
                    "rapoo:4613",
                ),
                "Rapoo VT7  [USB]  65%  (rapoo:4613)  source-verified",
            ),
            (
                line(
                    "Buds",
                    Via::Bluetooth,
                    Level::Percent(60),
                    Charge::Unknown,
                    "bt:00000000000000000000000000000001",
                ),
                "Buds  [Bluetooth]  60%  (bt:00000000000000000000000000000001)  source-verified",
            ),
            (
                line(
                    "Xbox Wireless Controller",
                    Via::Receiver,
                    Level::Coarse(CoarseLevel::Good),
                    Charge::Discharging,
                    "pad:wgi:1",
                ),
                "Xbox Wireless Controller  [receiver]  Good  (pad:wgi:1)  source-verified",
            ),
            (
                line(
                    "Xbox Controller",
                    Via::Usb,
                    Level::Coarse(CoarseLevel::Low),
                    Charge::Charging,
                    "pad:xinput:0",
                ),
                "Xbox Controller  [USB]  Low charging  (pad:xinput:0)  source-verified",
            ),
        ];
        for (got, want) in rows {
            assert_eq!(got, want);
        }
    }

    #[test]
    fn only_nova_base_stations_wait_for_their_first_reading() {
        let catalog = Catalog::builtin();
        for pid in [0x12E0, 0x12E5, 0x225D, 0x2290, 0x2244, 0x2246, 0x2270] {
            assert!(is_station(&catalog, 0x1038, pid), "{pid:04X}");
        }
        // A Nova dongle settles within the quiet period; other vendors' ids are no station.
        assert!(!is_station(&catalog, 0x1038, 0x2202));
        assert!(!is_station(&catalog, 0x1038, 0x230A));
        assert!(!is_station(&catalog, 0x046D, 0xC547));
        assert!(!is_station(&catalog, 0x046D, 0x12E0));
    }

    /// Runs [`settle`] on a virtual clock where changes arrive at `at` (ms after the start).
    /// Returns the changes it waited for and when it stopped.
    fn simulate(floor: Duration, at: &[u64]) -> (Vec<u64>, Duration) {
        let now = Cell::new(Duration::ZERO);
        let mut pending = at.iter().copied().peekable();
        let mut seen = Vec::new();
        settle(
            floor,
            || now.get(),
            |timeout| {
                let deadline = now.get() + timeout;
                match pending.peek() {
                    Some(&ms) if Duration::from_millis(ms) <= deadline => {
                        now.set(now.get().max(Duration::from_millis(ms)));
                        pending.next();
                        seen.push(ms);
                        true
                    }
                    _ => {
                        now.set(deadline);
                        false
                    }
                }
            },
        );
        (seen, now.get())
    }

    #[test]
    fn ordinary_scan_stops_after_the_quiet_period() {
        assert_eq!(QUIET, Duration::from_secs(5));
        assert_eq!(MAX_WAIT, Duration::from_secs(10));
        assert_eq!(simulate(Duration::ZERO, &[]), (vec![], QUIET));
        assert_eq!(
            simulate(Duration::ZERO, &[300, 1_200]),
            (vec![300, 1_200], Duration::from_millis(6_200))
        );
        // Without a station the original boundary still applies: a first reading after
        // 5 s quiet would be missed.
        assert_eq!(simulate(Duration::ZERO, &[5_400]), (vec![], QUIET));
    }

    #[test]
    fn station_first_reading_after_the_quiet_period_is_listed() {
        // Settle 5 s, then its reply arrives; nothing changed before it.
        let (seen, stopped) = simulate(STATION_FIRST_READING, &[5_400]);
        assert_eq!(seen, [5_400]);
        assert_eq!(stopped, MAX_WAIT);
        // A reply at the end of its 2 s attribution window is still waited for.
        let (seen, stopped) = simulate(STATION_FIRST_READING, &[1_000, 6_900]);
        assert_eq!(seen, [1_000, 6_900]);
        assert_eq!(stopped, MAX_WAIT);
    }

    #[test]
    fn silent_station_and_busy_sources_stop_within_the_hard_cap() {
        assert!(STATION_FIRST_READING <= MAX_WAIT);
        // A station whose headset never answers: its window, not longer.
        assert_eq!(simulate(STATION_FIRST_READING, &[]), (vec![], STATION_FIRST_READING));
        // Changes that never go quiet end at the hard cap, station or not.
        let busy: Vec<u64> = (1..=30).map(|s| s * 1_000).collect();
        for floor in [Duration::ZERO, STATION_FIRST_READING] {
            let (seen, stopped) = simulate(floor, &busy);
            assert_eq!(stopped, MAX_WAIT);
            assert_eq!(seen.last(), Some(&10_000));
        }
    }
}
