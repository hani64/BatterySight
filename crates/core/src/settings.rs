//! The user's options: device order, hidden devices, per-device low-battery thresholds,
//! monitors, display mode and autostart, and their `settings.toml` form.
//!
//! The file is meant to stay hand-editable: keys are device keys, unknown fields are
//! ignored (a newer version's file still loads), and any field of the wrong shape or out
//! of range rejects the whole file, so a hand edit is never half applied.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Write as _};

use toml::Value;

use crate::model::{DeviceKey, DeviceState};

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
    pub monitors: MonitorChoice,
    pub display: DisplayMode,
    pub autostart: bool,
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
        out
    }

    /// The device's threshold, [`DEFAULT_THRESHOLD`] when it has none.
    pub fn threshold(&self, key: &DeviceKey) -> Threshold {
        self.thresholds
            .get(key)
            .copied()
            .unwrap_or(DEFAULT_THRESHOLD)
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
            monitors: MonitorChoice::Named(r"\\.\DISPLAY2".into()),
            display: DisplayMode::Tray,
            autostart: true,
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

        assert_eq!(Settings::from_toml(""), Ok(Settings::default()));
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
}
