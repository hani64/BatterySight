//! Low-battery alert rules: which updates raise a notification.
//!
//! A device alerts once when a fresh update (its `updated_ms` changed since the last check;
//! an update without a new level counts, with the level the device kept) from an awake,
//! shown device that is neither charging nor full is at or below its threshold. It can
//! alert again only after it charges, rises [`REARM_POINTS`] above its threshold or gets a
//! new threshold ([`Alerts::rearm`]), so a level hovering at the threshold alerts once. The
//! state lives in memory only: after a restart a device that is still low alerts once more.

use std::collections::HashMap;

use batterysight_core::model::{Charge, CoarseLevel, DeviceKey, DeviceState, Level, Presence};
use batterysight_core::settings::{Settings, Threshold};
use batterysight_core::time::Ms;

/// Points above its threshold a device must rise to before it can alert again.
const REARM_POINTS: u8 = 10;

/// Which devices may alert, and which readings were already checked.
#[derive(Debug, Default)]
pub struct Alerts {
    /// Whether a device may alert; a device not in it may.
    armed: HashMap<DeviceKey, bool>,
    /// The `updated_ms` of each device's reading last checked.
    seen: HashMap<DeviceKey, Ms>,
}

/// A device that reached its low-battery threshold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub key: DeviceKey,
    pub name: String,
    /// Its level; `None` for a device that only reports a coarse level (low or critical).
    pub percent: Option<u8>,
}

impl Alert {
    /// The notification text.
    pub fn text(&self) -> String {
        match self.percent {
            Some(p) => format!("{} is at {p} %", self.name),
            None => format!("{} is low", self.name),
        }
    }
}

impl Alerts {
    /// The device's threshold changed: its next low update alerts again.
    pub fn rearm(&mut self, key: &DeviceKey) {
        self.armed.remove(key);
    }

    /// The devices whose fresh readings in `states` alert now (see the module doc).
    pub fn check(&mut self, states: &[DeviceState], settings: &Settings) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for s in states {
            if self.seen.insert(s.key.clone(), s.updated_ms) == Some(s.updated_ms) {
                continue;
            }
            let armed = self.armed.entry(s.key.clone()).or_insert(true);
            // On the charger: no alert, and the next drop alerts again.
            if matches!(s.charge, Charge::Charging | Charge::Full) {
                *armed = true;
                continue;
            }
            let Threshold::Percent(threshold) = settings.threshold(&s.key) else {
                continue;
            };
            // Coarse levels: low or critical is at the threshold, good or full re-arms.
            let (low, rearm, percent) = match s.level {
                Some(Level::Percent(p)) => (
                    p <= threshold,
                    p >= threshold.saturating_add(REARM_POINTS),
                    Some(p),
                ),
                Some(Level::Coarse(c)) => (c <= CoarseLevel::Low, c >= CoarseLevel::Good, None),
                None => continue,
            };
            if rearm {
                *armed = true;
            }
            let shown = s.presence == Presence::Online && !settings.hidden.contains(&s.key);
            if low && shown && *armed {
                *armed = false;
                alerts.push(Alert {
                    key: s.key.clone(),
                    name: s.name.clone(),
                    percent,
                });
            }
        }
        alerts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{
        Charge, CoarseLevel, DeviceKind, Level, Presence, Source, Verification, Via,
    };
    use batterysight_core::settings::Threshold;

    /// A reading of device "M" at `updated_ms`.
    fn reading(level: Level, updated_ms: Ms) -> DeviceState {
        DeviceState {
            key: DeviceKey("M".into()),
            name: "Mouse".into(),
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: Some(level),
            millivolts: None,
            charge: Charge::Discharging,
            presence: Presence::Online,
            updated_ms,
            source: Source::Query,
            verification: Verification::HardwareTested,
            poll_min_ms: None,
        }
    }

    fn percent(p: u8, updated_ms: Ms) -> DeviceState {
        reading(Level::Percent(p), updated_ms)
    }

    fn charging(p: u8, updated_ms: Ms) -> DeviceState {
        DeviceState {
            charge: Charge::Charging,
            ..percent(p, updated_ms)
        }
    }

    /// Checks each state in turn (one snapshot each); returns the percent of every alert.
    fn run(alerts: &mut Alerts, settings: &Settings, states: &[DeviceState]) -> Vec<Option<u8>> {
        states
            .iter()
            .flat_map(|s| alerts.check(std::slice::from_ref(s), settings))
            .map(|a| a.percent)
            .collect()
    }

    #[test]
    fn fires_once_at_threshold() {
        let mut alerts = Alerts::default();
        let settings = Settings::default();
        // 15 % is the default threshold; 16 is above it.
        assert!(alerts.check(&[percent(16, 1)], &settings).is_empty());
        let fired = alerts.check(&[percent(15, 2)], &settings);
        assert_eq!(
            fired,
            [Alert {
                key: DeviceKey("M".into()),
                name: "Mouse".into(),
                percent: Some(15),
            }]
        );
        assert_eq!(fired[0].text(), "Mouse is at 15 %");
        // Lower readings, and the same reading checked again, stay quiet.
        assert!(alerts.check(&[percent(15, 2)], &settings).is_empty());
        assert_eq!(
            run(&mut alerts, &settings, &[percent(12, 3), percent(5, 4)]),
            []
        );
    }

    #[test]
    fn first_reading_below_threshold_fires() {
        let mut alerts = Alerts::default();
        assert_eq!(
            run(&mut alerts, &Settings::default(), &[percent(8, 0)]),
            [Some(8)]
        );
    }

    #[test]
    fn jitter_at_threshold_alerts_once() {
        let mut alerts = Alerts::default();
        let readings = [
            percent(16, 1),
            percent(15, 2),
            percent(14, 3),
            percent(15, 4),
            percent(14, 5),
            percent(16, 6),
            percent(15, 7),
            percent(14, 8),
        ];
        assert_eq!(
            run(&mut alerts, &Settings::default(), &readings),
            [Some(15)]
        );
    }

    #[test]
    fn rearms_on_charging() {
        let mut alerts = Alerts::default();
        let readings = [
            percent(14, 1),
            charging(14, 2),
            charging(16, 3),
            // Unplugged at 16: back down to 15 alerts again.
            percent(16, 4),
            percent(15, 5),
        ];
        assert_eq!(
            run(&mut alerts, &Settings::default(), &readings),
            [Some(14), Some(15)]
        );
        // A charging device at its threshold does not alert.
        let mut alerts = Alerts::default();
        assert_eq!(
            run(&mut alerts, &Settings::default(), &[charging(5, 1)]),
            []
        );
    }

    #[test]
    fn full_rearms_and_other_charge_states_alert() {
        let full = |p, updated_ms| DeviceState {
            charge: Charge::Full,
            ..percent(p, updated_ms)
        };
        let mut alerts = Alerts::default();
        let readings = [percent(14, 1), full(14, 2), percent(14, 3)];
        assert_eq!(
            run(&mut alerts, &Settings::default(), &readings),
            [Some(14), Some(14)]
        );
        for charge in [Charge::NotCharging, Charge::Unknown] {
            let mut alerts = Alerts::default();
            let s = DeviceState {
                charge,
                ..percent(14, 1)
            };
            assert_eq!(run(&mut alerts, &Settings::default(), &[s]), [Some(14)]);
        }
    }

    #[test]
    fn rearms_when_threshold_changes() {
        let key = DeviceKey("M".into());
        // Lowered from 20 to 15 after an alert at 20: alerts again at 15.
        let mut settings = Settings::default();
        settings.set_threshold(&key, Threshold::Percent(20));
        let mut alerts = Alerts::default();
        assert_eq!(run(&mut alerts, &settings, &[percent(20, 1)]), [Some(20)]);
        settings.set_threshold(&key, Threshold::Percent(15));
        alerts.rearm(&key);
        let readings = [
            percent(19, 2),
            percent(15, 3),
            percent(10, 4),
            percent(5, 5),
        ];
        assert_eq!(run(&mut alerts, &settings, &readings), [Some(15)]);

        // Lowered from 15 to 5 after an alert at 15: alerts again at 5.
        let mut settings = Settings::default();
        let mut alerts = Alerts::default();
        assert_eq!(run(&mut alerts, &settings, &[percent(15, 1)]), [Some(15)]);
        settings.set_threshold(&key, Threshold::Percent(5));
        alerts.rearm(&key);
        let readings = [percent(14, 2), percent(6, 3), percent(5, 4)];
        assert_eq!(run(&mut alerts, &settings, &readings), [Some(5)]);
    }

    #[test]
    fn rearms_10_points_above() {
        let mut settings = Settings::default();
        settings.set_threshold(&DeviceKey("M".into()), Threshold::Percent(20));
        let mut alerts = Alerts::default();
        // 29 is not 10 points above 20: no new alert at 20.
        let readings = [percent(20, 1), percent(29, 2), percent(20, 3)];
        assert_eq!(run(&mut alerts, &settings, &readings), [Some(20)]);
        // 30 is: the next drop to 20 alerts again.
        let readings = [percent(30, 4), percent(21, 5), percent(19, 6)];
        assert_eq!(run(&mut alerts, &settings, &readings), [Some(19)]);
    }

    #[test]
    fn hidden_or_asleep_never_fires() {
        let mut hidden = Settings::default();
        hidden.set_hidden(&DeviceKey("M".into()), true);
        let mut alerts = Alerts::default();
        assert_eq!(
            run(&mut alerts, &hidden, &[percent(10, 1), percent(9, 2)]),
            []
        );
        // Shown again, its last reading is not fresh; the next one alerts.
        let shown = Settings::default();
        assert_eq!(run(&mut alerts, &shown, &[percent(9, 2)]), []);
        assert_eq!(run(&mut alerts, &shown, &[percent(9, 3)]), [Some(9)]);

        for presence in [Presence::Asleep, Presence::Offline] {
            let away = |updated_ms| DeviceState {
                presence,
                ..percent(10, updated_ms)
            };
            let mut alerts = Alerts::default();
            assert_eq!(run(&mut alerts, &shown, &[away(1), away(2)]), []);
            // Awake again with a fresh reading: it alerts.
            assert_eq!(run(&mut alerts, &shown, &[percent(10, 3)]), [Some(10)]);
        }
    }

    #[test]
    fn off_threshold_never_fires() {
        let mut settings = Settings::default();
        settings.set_threshold(&DeviceKey("M".into()), Threshold::Off);
        let mut alerts = Alerts::default();
        let readings = [
            percent(50, 1),
            percent(15, 2),
            percent(1, 3),
            reading(Level::Coarse(CoarseLevel::Critical), 4),
        ];
        assert_eq!(run(&mut alerts, &settings, &readings), []);
        // The same readings at the default threshold alert.
        let mut alerts = Alerts::default();
        assert_eq!(
            run(&mut alerts, &Settings::default(), &readings),
            [Some(15)]
        );
    }

    #[test]
    fn coarse_low_alerts_once_and_says_low() {
        let mut alerts = Alerts::default();
        let settings = Settings::default();
        let coarse = |c, updated_ms| reading(Level::Coarse(c), updated_ms);
        assert!(
            alerts
                .check(&[coarse(CoarseLevel::Good, 1)], &settings)
                .is_empty()
        );
        let fired = alerts.check(&[coarse(CoarseLevel::Low, 2)], &settings);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].percent, None);
        assert_eq!(fired[0].text(), "Mouse is low");
        // Critical after Low: still the same low spell.
        assert_eq!(
            run(&mut alerts, &settings, &[coarse(CoarseLevel::Critical, 3)]),
            []
        );
        // Good again re-arms it.
        let readings = [coarse(CoarseLevel::Good, 4), coarse(CoarseLevel::Low, 5)];
        assert_eq!(run(&mut alerts, &settings, &readings), [None]);
    }

    #[test]
    fn devices_alert_independently() {
        let mut alerts = Alerts::default();
        let other = |p, updated_ms| DeviceState {
            key: DeviceKey("K".into()),
            name: "Keyboard".into(),
            ..percent(p, updated_ms)
        };
        let fired = alerts.check(&[percent(10, 1), other(50, 1)], &Settings::default());
        assert_eq!(fired.len(), 1);
        // "M" unchanged (not fresh), "K" drops: only "K" alerts.
        let fired = alerts.check(&[percent(10, 1), other(12, 2)], &Settings::default());
        assert_eq!(
            fired.iter().map(|a| a.text()).collect::<Vec<_>>(),
            ["Keyboard is at 12 %"]
        );
    }
}
