//! The user's options: device order, hidden devices, per-device low-battery thresholds and
//! poll intervals, monitors, display mode and autostart, and their `settings.toml` form.
//!
//! The file is meant to stay hand-editable: keys are device keys, unknown fields are
//! ignored (a newer version's file still loads), and any field of the wrong shape or out
//! of range rejects the whole file, so a hand edit is never half applied.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Write as _};

use toml::Value;

use crate::model::{DeviceKey, DeviceState};
use crate::time::Ms;

/// Low-battery alert level of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threshold {
    Off,
    /// Alert at or below this level, 1..=100.
    Percent(u8),
}

impl Threshold {
    /// `self` with a percent clamped to 1..=100, the range a settings file accepts.
    fn clamped(self) -> Threshold {
        match self {
            Threshold::Off => Threshold::Off,
            Threshold::Percent(p) => Threshold::Percent(p.clamp(1, 100)),
        }
    }
}

/// Threshold of a device without a settings entry.
pub const DEFAULT_THRESHOLD: Threshold = Threshold::Percent(15);

/// Largest poll interval a settings file accepts, in minutes (one day); the smallest is 1.
pub const INTERVAL_MAX_MIN: u16 = 1440;

/// Taskbars that show the widget.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MonitorChoice {
    #[default]
    All,
    Main,
    /// The monitor with this device name (`GetMonitorInfoW`'s `szDevice`).
    Named(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayMode {
    #[default]
    Widget,
    Tray,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// User order of devices; devices not listed follow in snapshot order.
    pub order: Vec<DeviceKey>,
    pub hidden: BTreeSet<DeviceKey>,
    pub thresholds: BTreeMap<DeviceKey, Threshold>,
    /// Minutes between battery checks while the device is in use, 1..=[`INTERVAL_MAX_MIN`];
    /// the device's own minimum still applies.
    pub intervals: BTreeMap<DeviceKey, u16>,
    pub monitors: MonitorChoice,
    pub display: DisplayMode,
    pub autostart: bool,
    /// Each device's last seen name, so the history can name devices not connected now.
    pub names: BTreeMap<DeviceKey, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsError {
    /// Not TOML.
    Toml(String),
    /// A field of the wrong shape or out of range; holds the field and what is wrong.
    Invalid(String),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(msg) => write!(f, "invalid settings: {msg}"),
            Self::Invalid(msg) => write!(f, "invalid settings: {msg}"),
        }
    }
}

impl std::error::Error for SettingsError {}

impl Settings {
    pub fn from_toml(text: &str) -> Result<Settings, SettingsError> {
        let table: toml::Table =
            toml::from_str(text).map_err(|e| SettingsError::Toml(e.message().to_owned()))?;
        let mut settings = Settings::default();
        if let Some(value) = table.get("order") {
            for key in key_list(value, "order")? {
                if !settings.order.contains(&key) {
                    settings.order.push(key);
                }
            }
        }
        if let Some(value) = table.get("hidden") {
            settings.hidden = key_list(value, "hidden")?.into_iter().collect();
        }
        if let Some(value) = table.get("thresholds") {
            let Value::Table(entries) = value else {
                return Err(invalid("thresholds", "expected a table of device keys"));
            };
            for (key, value) in entries {
                let threshold = threshold_of(value)
                    .ok_or_else(|| invalid(&format!("thresholds.{key}"), THRESHOLD_FORM))?;
                settings
                    .thresholds
                    .insert(DeviceKey(key.clone()), threshold);
            }
        }
        if let Some(value) = table.get("intervals") {
            let Value::Table(entries) = value else {
                return Err(invalid("intervals", "expected a table of device keys"));
            };
            for (key, value) in entries {
                let minutes = interval_of(value)
                    .ok_or_else(|| invalid(&format!("intervals.{key}"), INTERVAL_FORM))?;
                settings.intervals.insert(DeviceKey(key.clone()), minutes);
            }
        }
        if let Some(value) = table.get("names") {
            let Value::Table(entries) = value else {
                return Err(invalid("names", "expected a table of device keys"));
            };
            for (key, value) in entries {
                let name = value
                    .as_str()
                    .ok_or_else(|| invalid(&format!("names.{key}"), "expected a string"))?;
                settings
                    .names
                    .insert(DeviceKey(key.clone()), name.to_owned());
            }
        }
        if let Some(value) = table.get("monitors") {
            settings.monitors = match value {
                Value::String(s) if s == "all" => MonitorChoice::All,
                Value::String(s) if s == "main" => MonitorChoice::Main,
                Value::Table(t) => match t.get("named") {
                    Some(Value::String(name)) => MonitorChoice::Named(name.clone()),
                    _ => return Err(invalid("monitors", MONITORS_FORM)),
                },
                _ => return Err(invalid("monitors", MONITORS_FORM)),
            };
        }
        if let Some(value) = table.get("display") {
            settings.display = match value.as_str() {
                Some("widget") => DisplayMode::Widget,
                Some("tray") => DisplayMode::Tray,
                _ => return Err(invalid("display", "expected \"widget\" or \"tray\"")),
            };
        }
        if let Some(value) = table.get("autostart") {
            settings.autostart = value
                .as_bool()
                .ok_or_else(|| invalid("autostart", "expected true or false"))?;
        }
        Ok(settings)
    }

    /// The `settings.toml` text, with a header comment describing the accepted values.
    pub fn to_toml(&self) -> String {
        let mut out = String::from(
            "# BatterySight settings. Keys are device keys.\n\
             # thresholds: 1 to 100 (percent) or \"off\"; a device without one uses 15.\n\
             # monitors: \"all\", \"main\" or { named = \"<monitor device name>\" }.\n\
             # display: \"widget\" or \"tray\".\n",
        );
        let list = |keys: &mut dyn Iterator<Item = &DeviceKey>| {
            let quoted: Vec<String> = keys.map(|k| quote(&k.0)).collect();
            format!("[{}]", quoted.join(", "))
        };
        let monitors = match &self.monitors {
            MonitorChoice::All => quote("all"),
            MonitorChoice::Main => quote("main"),
            MonitorChoice::Named(name) => format!("{{ named = {} }}", quote(name)),
        };
        let display = match self.display {
            DisplayMode::Widget => "widget",
            DisplayMode::Tray => "tray",
        };
        // Writing to a `String` cannot fail.
        let _ = writeln!(out, "order = {}", list(&mut self.order.iter()));
        let _ = writeln!(out, "hidden = {}", list(&mut self.hidden.iter()));
        let _ = writeln!(out, "monitors = {monitors}");
        let _ = writeln!(out, "display = {}", quote(display));
        let _ = writeln!(out, "autostart = {}", self.autostart);
        out.push_str("\n[thresholds]\n");
        for (key, threshold) in &self.thresholds {
            let value = match threshold.clamped() {
                Threshold::Off => quote("off"),
                Threshold::Percent(p) => p.to_string(),
            };
            let _ = writeln!(out, "{} = {value}", quote(&key.0));
        }
        out.push_str(
            "\n# intervals: minutes between battery checks while a device is in use; \
             never below the device's minimum\n[intervals]\n",
        );
        for (key, minutes) in &self.intervals {
            let minutes = (*minutes).clamp(1, INTERVAL_MAX_MIN);
            let _ = writeln!(out, "{} = {minutes}", quote(&key.0));
        }
        out.push_str("\n# names: each device's last seen name, for the battery history\n[names]\n");
        for (key, name) in &self.names {
            let _ = writeln!(out, "{} = {}", quote(&key.0), quote(name));
        }
        out
    }

    /// The device's threshold, [`DEFAULT_THRESHOLD`] when it has none.
    pub fn threshold(&self, key: &DeviceKey) -> Threshold {
        self.thresholds
            .get(key)
            .copied()
            .unwrap_or(DEFAULT_THRESHOLD)
    }

    /// The device's poll interval in minutes; `None` when it has none, so the default applies
    /// (10 minutes, or the device's minimum if that is longer).
    pub fn interval(&self, key: &DeviceKey) -> Option<u16> {
        self.intervals.get(key).copied()
    }

    /// Sets `key`'s poll interval; minutes outside 1..=[`INTERVAL_MAX_MIN`] are clamped into it.
    pub fn set_interval(&mut self, key: &DeviceKey, minutes: u16) {
        self.intervals
            .insert(key.clone(), minutes.clamp(1, INTERVAL_MAX_MIN));
    }

    /// Remembers the names of `states`; whether any was new or changed (and so needs saving).
    pub fn remember_names(&mut self, states: &[DeviceState]) -> bool {
        let mut changed = false;
        for s in states {
            if self.names.get(&s.key) != Some(&s.name) {
                self.names.insert(s.key.clone(), s.name.clone());
                changed = true;
            }
        }
        changed
    }

    /// Every poll interval in milliseconds, as the scheduler takes them.
    pub fn interval_gaps(&self) -> HashMap<DeviceKey, Ms> {
        self.intervals
            .iter()
            .map(|(key, &minutes)| (key.clone(), Ms::from(minutes) * 60_000))
            .collect()
    }

    /// `states` in the user's order; devices not in it follow, in `states` order.
    pub fn ordered<'a>(&self, states: &'a [DeviceState]) -> Vec<&'a DeviceState> {
        let mut rank: HashMap<&DeviceKey, usize> = HashMap::new();
        for (i, key) in self.order.iter().enumerate() {
            rank.entry(key).or_insert(i);
        }
        let mut out: Vec<&DeviceState> = states.iter().collect();
        // Stable: unknown devices keep their snapshot order behind the known ones.
        out.sort_by_key(|s| rank.get(&s.key).copied().unwrap_or(usize::MAX));
        out
    }

    /// Swaps `key` with the nearest device before it in the order that is in `visible`
    /// (the devices shown, in shown order); devices in between keep their places.
    pub fn move_left(&mut self, key: &DeviceKey, visible: &[DeviceKey]) {
        self.shift(key, visible, true);
    }

    /// Like [`Settings::move_left`], towards the end.
    pub fn move_right(&mut self, key: &DeviceKey, visible: &[DeviceKey]) {
        self.shift(key, visible, false);
    }

    pub fn set_hidden(&mut self, key: &DeviceKey, hidden: bool) {
        if hidden {
            self.hidden.insert(key.clone());
        } else {
            self.hidden.remove(key);
        }
    }

    /// Sets `key`'s threshold; a percent outside 1..=100 is clamped into it.
    pub fn set_threshold(&mut self, key: &DeviceKey, threshold: Threshold) {
        self.thresholds.insert(key.clone(), threshold.clamped());
    }

    fn shift(&mut self, key: &DeviceKey, visible: &[DeviceKey], left: bool) {
        // Shown devices the order does not list yet take their shown places at its end.
        for k in visible.iter().chain([key]) {
            if !self.order.contains(k) {
                self.order.push(k.clone());
            }
        }
        let Some(at) = self.order.iter().position(|k| k == key) else {
            return;
        };
        let shown = |i: &usize| visible.contains(&self.order[*i]);
        let neighbour = if left {
            (0..at).rev().find(shown)
        } else {
            (at + 1..self.order.len()).find(shown)
        };
        if let Some(n) = neighbour {
            self.order.swap(at, n);
        }
    }
}

const THRESHOLD_FORM: &str = "expected 1 to 100 or \"off\"";
const INTERVAL_FORM: &str = "expected whole minutes 1 to 1440";
const MONITORS_FORM: &str = "expected \"all\", \"main\" or { named = \"<monitor>\" }";

fn invalid(field: &str, what: &str) -> SettingsError {
    SettingsError::Invalid(format!("{field}: {what}"))
}

fn key_list(value: &Value, field: &str) -> Result<Vec<DeviceKey>, SettingsError> {
    let bad = || invalid(field, "expected a list of device keys");
    let Value::Array(items) = value else {
        return Err(bad());
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(|s| DeviceKey(s.to_owned()))
                .ok_or_else(bad)
        })
        .collect()
}

fn threshold_of(value: &Value) -> Option<Threshold> {
    match value {
        Value::String(s) if s == "off" => Some(Threshold::Off),
        Value::Integer(n) => u8::try_from(*n)
            .ok()
            .filter(|p| (1..=100).contains(p))
            .map(Threshold::Percent),
        _ => None,
    }
}

fn interval_of(value: &Value) -> Option<u16> {
    match value {
        Value::Integer(n) => u16::try_from(*n)
            .ok()
            .filter(|m| (1..=INTERVAL_MAX_MIN).contains(m)),
        _ => None,
    }
}

/// `s` as a TOML string, escaped as needed; also valid as a key.
fn quote(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Charge, DeviceKind, Presence, Source, Verification, Via};

    fn key(s: &str) -> DeviceKey {
        DeviceKey(s.into())
    }

    fn keys(list: &[&str]) -> Vec<DeviceKey> {
        list.iter().map(|s| key(s)).collect()
    }

    fn state(k: &str) -> DeviceState {
        DeviceState {
            key: key(k),
            name: k.into(),
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: None,
            millivolts: None,
            charge: Charge::Unknown,
            presence: Presence::Online,
            updated_ms: 0,
            source: Source::Query,
            verification: Verification::Generic,
            poll_min_ms: None,
            reports_changes: false,
        }
    }

    fn order_of(settings: &Settings, states: &[DeviceState]) -> Vec<DeviceKey> {
        settings
            .ordered(states)
            .into_iter()
            .map(|s| s.key.clone())
            .collect()
    }

    #[test]
    fn round_trip_settings() {
        let settings = Settings {
            order: keys(&["logi:unit:1A2B3C4D", "logi:0ABA:255", "logi:0ABA:255#2"]),
            hidden: keys(&["logi:0ABA:255#2"]).into_iter().collect(),
            thresholds: [
                (key("logi:unit:1A2B3C4D"), Threshold::Percent(40)),
                (key("logi:0ABA:255"), Threshold::Off),
                (key("odd \"key\" \\ with ünïcode"), Threshold::Percent(1)),
            ]
            .into_iter()
            .collect(),
            intervals: [(key("logi:0ABA:255"), 30)].into_iter().collect(),
            monitors: MonitorChoice::Named(r"\\.\DISPLAY2".into()),
            display: DisplayMode::Tray,
            autostart: true,
            names: [
                (key("bt:0000000000000000000000000000000A"), "Buds".into()),
                (key("pad:wgi:1"), "Pad \"one\" \\ ünï".into()),
            ]
            .into_iter()
            .collect(),
        };
        let text = settings.to_toml();
        assert_eq!(Settings::from_toml(&text), Ok(settings), "{text}");

        for (monitors, display) in [
            (MonitorChoice::All, DisplayMode::Widget),
            (MonitorChoice::Main, DisplayMode::Tray),
        ] {
            let settings = Settings {
                monitors,
                display,
                ..Settings::default()
            };
            assert_eq!(Settings::from_toml(&settings.to_toml()), Ok(settings));
        }
    }

    #[test]
    fn hand_written_file_parses() {
        let text = r#"
            order = ["b", "a", "b"]
            hidden = ["c"]
            monitors = { named = '\\.\DISPLAY1' }
            display = "tray"
            autostart = true
            future_option = 3

            [thresholds]
            a = 5
            b = "off"
            c = 100
        "#;
        let settings = Settings::from_toml(text).unwrap();
        assert_eq!(settings.order, keys(&["b", "a"]), "duplicates dropped");
        assert_eq!(settings.hidden, keys(&["c"]).into_iter().collect());
        assert_eq!(settings.threshold(&key("a")), Threshold::Percent(5));
        assert_eq!(settings.threshold(&key("b")), Threshold::Off);
        assert_eq!(settings.threshold(&key("c")), Threshold::Percent(100));
        assert_eq!(
            settings.monitors,
            MonitorChoice::Named(r"\\.\DISPLAY1".into())
        );
        assert_eq!(settings.display, DisplayMode::Tray);
        assert!(settings.autostart);
        // A file from before names were kept.
        assert!(settings.names.is_empty());

        assert_eq!(Settings::from_toml(""), Ok(Settings::default()));
    }

    #[test]
    fn names_are_remembered_only_when_new_or_changed() {
        let mut settings = Settings::default();
        let mut buds = state("bt:0000000000000000000000000000000A");
        buds.name = "Galaxy Buds2".into();
        let pad = state("pad:wgi:1");
        assert!(settings.remember_names(&[buds.clone(), pad.clone()]));
        assert_eq!(settings.names[&buds.key], "Galaxy Buds2");
        // Unchanged: nothing to write.
        assert!(!settings.remember_names(&[buds.clone(), pad.clone()]));
        assert!(!settings.remember_names(&[]));
        buds.name = "My Buds".into();
        assert!(settings.remember_names(&[buds.clone()]));
        assert_eq!(settings.names[&buds.key], "My Buds");
        assert_eq!(settings.names[&pad.key], "pad:wgi:1");
    }

    #[test]
    fn names_must_be_strings() {
        assert!(Settings::from_toml("[names]\na = 3\n").is_err());
        assert!(Settings::from_toml("names = 3\n").is_err());
    }

    #[test]
    fn bad_entries_are_errors() {
        for text in [
            "order = [",
            "order = \"a\"",
            "order = [1]",
            "hidden = [true]",
            "autostart = \"yes\"",
            "display = \"tiles\"",
            "monitors = \"left\"",
            "monitors = { name = \"x\" }",
            "[thresholds]\na = 0",
            "[thresholds]\na = 101",
            "[thresholds]\na = -5",
            "[thresholds]\na = \"low\"",
            "[thresholds]\na = 15.5",
            "thresholds = 15",
        ] {
            assert!(Settings::from_toml(text).is_err(), "{text}");
        }
    }

    #[test]
    fn unknown_device_is_shown_last_with_default_threshold() {
        let settings = Settings {
            order: keys(&["b", "gone", "a"]),
            thresholds: [(key("a"), Threshold::Percent(30))].into_iter().collect(),
            ..Settings::default()
        };
        let states = [state("new-1"), state("a"), state("new-2"), state("b")];

        assert_eq!(
            order_of(&settings, &states),
            keys(&["b", "a", "new-1", "new-2"])
        );
        assert_eq!(settings.threshold(&key("new-1")), DEFAULT_THRESHOLD);
        assert_eq!(DEFAULT_THRESHOLD, Threshold::Percent(15));
        assert_eq!(settings.threshold(&key("a")), Threshold::Percent(30));
    }

    #[test]
    fn move_left_swaps_with_visible_neighbour() {
        let mut settings = Settings {
            order: keys(&["a", "h", "b", "c"]),
            hidden: keys(&["h"]).into_iter().collect(),
            ..Settings::default()
        };
        let visible = keys(&["a", "b", "c"]);

        // `h` is hidden: `b` swaps with `a`, `h` keeps its place.
        settings.move_left(&key("b"), &visible);
        assert_eq!(settings.order, keys(&["b", "h", "a", "c"]));

        // Already first: nothing moves.
        let visible = keys(&["b", "a", "c"]);
        settings.move_left(&key("b"), &visible);
        assert_eq!(settings.order, keys(&["b", "h", "a", "c"]));

        settings.move_right(&key("a"), &visible);
        assert_eq!(settings.order, keys(&["b", "h", "c", "a"]));
        // Already last.
        let visible = keys(&["b", "c", "a"]);
        settings.move_right(&key("a"), &visible);
        assert_eq!(settings.order, keys(&["b", "h", "c", "a"]));
    }

    #[test]
    fn moving_a_device_not_in_the_order_places_the_visible_ones() {
        let mut settings = Settings {
            order: keys(&["a"]),
            ..Settings::default()
        };
        // `x` and `y` are new: shown after `a`, in snapshot order.
        settings.move_left(&key("y"), &keys(&["a", "x", "y"]));
        assert_eq!(settings.order, keys(&["a", "y", "x"]));
    }

    #[test]
    fn hidden_and_threshold_setters() {
        let mut settings = Settings::default();
        settings.set_hidden(&key("a"), true);
        settings.set_threshold(&key("a"), Threshold::Off);
        assert!(settings.hidden.contains(&key("a")));
        assert_eq!(settings.threshold(&key("a")), Threshold::Off);

        settings.set_hidden(&key("a"), false);
        settings.set_threshold(&key("a"), Threshold::Percent(25));
        assert!(settings.hidden.is_empty());
        assert_eq!(settings.threshold(&key("a")), Threshold::Percent(25));
    }

    #[test]
    fn thresholds_out_of_range_are_clamped_never_saved() {
        let mut settings = Settings::default();
        settings.set_threshold(&key("low"), Threshold::Percent(0));
        settings.set_threshold(&key("high"), Threshold::Percent(200));
        assert_eq!(settings.threshold(&key("low")), Threshold::Percent(1));
        assert_eq!(settings.threshold(&key("high")), Threshold::Percent(100));

        // Written directly into the map: the file still loads.
        settings
            .thresholds
            .insert(key("direct"), Threshold::Percent(0));
        let loaded = Settings::from_toml(&settings.to_toml()).unwrap();
        assert_eq!(loaded.threshold(&key("direct")), Threshold::Percent(1));
        assert_eq!(loaded.threshold(&key("high")), Threshold::Percent(100));
    }

    #[test]
    fn intervals_round_trip() {
        let settings = Settings {
            intervals: [(key("logi:0ABA:255"), 10)].into_iter().collect(),
            ..Settings::default()
        };
        let text = settings.to_toml();
        assert!(text.contains("[intervals]"), "{text}");
        assert_eq!(Settings::from_toml(&text), Ok(settings), "{text}");
    }

    #[test]
    fn hand_written_intervals_parse() {
        let settings = Settings::from_toml("[intervals]\n\"logi:0ABA:255\" = 30\n").unwrap();
        assert_eq!(settings.interval(&key("logi:0ABA:255")), Some(30));
        assert_eq!(settings.interval(&key("other")), None);
    }

    #[test]
    fn bad_intervals_are_errors() {
        for value in ["0", "-5", "1441", "\"10\"", "2.5"] {
            let text = format!("[intervals]\n\"logi:0ABA:255\" = {value}\n");
            match Settings::from_toml(&text) {
                Err(SettingsError::Invalid(msg)) => {
                    assert!(msg.contains("intervals.logi:0ABA:255"), "{value}: {msg}");
                }
                other => panic!("{value}: {other:?}"),
            }
        }
        assert!(Settings::from_toml("intervals = 10").is_err());
    }

    #[test]
    fn set_interval_clamps() {
        let mut settings = Settings::default();
        let k = key("logi:0ABA:255");
        settings.set_interval(&k, 0);
        assert_eq!(settings.interval(&k), Some(1));
        settings.set_interval(&k, 5000);
        assert_eq!(settings.interval(&k), Some(INTERVAL_MAX_MIN));
        assert_eq!(INTERVAL_MAX_MIN, 1440);
    }

    #[test]
    fn intervals_out_of_range_are_clamped_never_saved() {
        // Written directly into the map: the file still loads.
        let mut settings = Settings::default();
        settings.intervals.insert(key("low"), 0);
        settings.intervals.insert(key("high"), 5000);
        let loaded = Settings::from_toml(&settings.to_toml()).unwrap();
        assert_eq!(loaded.interval(&key("low")), Some(1));
        assert_eq!(loaded.interval(&key("high")), Some(INTERVAL_MAX_MIN));
    }

    #[test]
    fn interval_gaps_are_milliseconds() {
        let mut settings = Settings::default();
        let k = key("logi:0ABA:255");
        settings.set_interval(&k, 10);
        assert_eq!(settings.interval_gaps()[&k], 600_000);
    }
}
