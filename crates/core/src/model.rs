//! Device state shared by the engines, the hub and the UI.

use serde::Deserialize;

use crate::time::Ms;

/// Stable identity of a device across reconnects.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceKey(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Mouse,
    Keyboard,
    Headset,
    Speaker,
    Controller,
    Other,
}

/// Battery level for devices that only report a bucket, ordered empty to full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoarseLevel {
    Critical,
    Low,
    Good,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    Percent(u8),
    Coarse(CoarseLevel),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Charge {
    Discharging,
    Charging,
    Full,
    NotCharging,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Presence {
    Online,
    Asleep,
    Offline,
}

/// Transport the device is reached through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Via {
    Receiver,
    Usb,
    Bluetooth,
}

/// How well support for a device is verified. `Generic` is runtime-only (no catalog entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verification {
    HardwareTested,
    SourceVerified,
    CommunityTested,
    Generic,
}

/// Where a reading came from: device-initiated report, our query, or the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Push,
    Query,
    Os,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceState {
    pub key: DeviceKey,
    pub name: String,
    pub kind: DeviceKind,
    pub via: Via,
    pub level: Option<Level>,
    pub millivolts: Option<u16>,
    pub charge: Charge,
    pub presence: Presence,
    pub updated_ms: Ms,
    pub source: Source,
    pub verification: Verification,
    /// The engine's minimum for battery checks driven by audio or a device's awake
    /// signal, when an interval setting is available.
    pub poll_min_ms: Option<Ms>,
    /// Whether the device reports battery changes on its own.
    pub reports_changes: bool,
}

/// Partial change to a [`DeviceState`]; `None` fields leave the current value untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceUpdate {
    pub key: DeviceKey,
    pub name: Option<String>,
    pub kind: Option<DeviceKind>,
    pub via: Option<Via>,
    pub level: Option<Level>,
    pub millivolts: Option<u16>,
    pub charge: Option<Charge>,
    pub presence: Option<Presence>,
    pub source: Source,
}

/// A reading from a system source that does not participate in HID grant routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemObservation {
    pub update: DeviceUpdate,
    pub verification: Verification,
    pub reports_changes: bool,
}

impl DeviceState {
    /// Merges `u` into `self`. A sleeping or offline device keeps its last known level.
    pub fn apply(&mut self, u: &DeviceUpdate, now: Ms) {
        if let Some(name) = &u.name {
            self.name.clone_from(name);
        }
        if let Some(kind) = u.kind {
            self.kind = kind;
        }
        if let Some(via) = u.via {
            self.via = via;
        }
        if let Some(level) = u.level {
            self.level = Some(level);
        }
        if let Some(mv) = u.millivolts {
            self.millivolts = Some(mv);
        }
        if let Some(charge) = u.charge {
            self.charge = charge;
        }
        if let Some(presence) = u.presence {
            self.presence = presence;
        }
        self.source = u.source;
        self.updated_ms = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> DeviceState {
        DeviceState {
            key: DeviceKey("hidpp:046d:0aba".into()),
            name: "PRO X".into(),
            kind: DeviceKind::Headset,
            via: Via::Receiver,
            level: Some(Level::Percent(70)),
            millivolts: Some(3900),
            charge: Charge::Discharging,
            presence: Presence::Online,
            updated_ms: 1_000,
            source: Source::Query,
            verification: Verification::SourceVerified,
            poll_min_ms: None,
            reports_changes: false,
        }
    }

    fn update(key: DeviceKey) -> DeviceUpdate {
        DeviceUpdate {
            key,
            name: None,
            kind: None,
            via: None,
            level: None,
            millivolts: None,
            charge: None,
            presence: None,
            source: Source::Push,
        }
    }

    #[test]
    fn asleep_update_keeps_level() {
        let mut s = state();
        let u = DeviceUpdate {
            presence: Some(Presence::Asleep),
            ..update(s.key.clone())
        };
        s.apply(&u, 5_000);
        assert_eq!(s.level, Some(Level::Percent(70)));
        assert_eq!(s.presence, Presence::Asleep);
        assert_eq!(s.updated_ms, 5_000);
    }

    #[test]
    fn offline_update_keeps_level_and_millivolts() {
        let mut s = state();
        let u = DeviceUpdate {
            presence: Some(Presence::Offline),
            ..update(s.key.clone())
        };
        s.apply(&u, 2_000);
        assert_eq!(s.level, Some(Level::Percent(70)));
        assert_eq!(s.millivolts, Some(3900));
        assert_eq!(s.presence, Presence::Offline);
    }

    #[test]
    fn some_fields_overwrite_and_source_is_copied() {
        let mut s = state();
        let u = DeviceUpdate {
            name: Some("PRO X Wireless".into()),
            kind: Some(DeviceKind::Other),
            via: Some(Via::Usb),
            level: Some(Level::Coarse(CoarseLevel::Low)),
            millivolts: Some(3600),
            charge: Some(Charge::Charging),
            presence: Some(Presence::Online),
            ..update(s.key.clone())
        };
        s.apply(&u, 9_000);
        assert_eq!(s.name, "PRO X Wireless");
        assert_eq!(s.kind, DeviceKind::Other);
        assert_eq!(s.via, Via::Usb);
        assert_eq!(s.level, Some(Level::Coarse(CoarseLevel::Low)));
        assert_eq!(s.millivolts, Some(3600));
        assert_eq!(s.charge, Charge::Charging);
        assert_eq!(s.presence, Presence::Online);
        assert_eq!(s.source, Source::Push);
        assert_eq!(s.updated_ms, 9_000);
        assert_eq!(s.verification, Verification::SourceVerified);
    }
}
