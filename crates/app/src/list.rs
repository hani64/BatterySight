//! `batterysight --list`: prints the devices the runtime finds, one per line.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::sync::PoisonError;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use batterysight_core::model::{
    Charge, CoarseLevel, DeviceState, Level, Presence, Verification, Via,
};
use batterysight_win::runtime;

/// The list is printed once no change arrived for this long...
const QUIET: Duration = Duration::from_secs(3);
/// ...or once this much time has passed since the runtime started.
const MAX_WAIT: Duration = Duration::from_secs(10);

/// Starts the runtime, waits for the readings to settle and prints one line per device.
/// Returns the process exit code.
pub fn run() -> i32 {
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
    while let Some(left) = MAX_WAIT.checked_sub(started.elapsed()) {
        if changes.recv_timeout(QUIET.min(left)).is_err() {
            break;
        }
    }
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
}
