//! Device catalog: which devices are supported, by which engine, and how well verified.

use std::fmt;

use serde::Deserialize;

use crate::model::{DeviceKind, Verification};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Hidpp,
}

/// Per-device deviations from engine defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quirks {
    /// Named millivolt-to-percent curve for devices that only report voltage.
    pub voltage_curve: Option<String>,
    /// An active battery query wakes the device, so it is only queried on user activity.
    pub query_wakes: bool,
    /// The device reports its battery only when it powers on or off, never while in use,
    /// so it is read on observed activity (audio playing to it), at most every 5 minutes.
    pub no_in_use_push: bool,
}

/// Engine support for every device of a vendor that has no specific entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Family {
    pub engine: EngineKind,
    pub vid: u16,
    pub status: Verification,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    pub name: String,
    pub vid: u16,
    /// USB product ids: the device itself, or the dongle that is the device (a headset's).
    pub pids: Vec<u16>,
    /// Wireless product ids of a device behind a receiver, as the receiver reports them; a
    /// receiver's USB PID says nothing about the devices paired to it.
    pub wpids: Vec<u16>,
    pub kind: DeviceKind,
    pub engine: EngineKind,
    pub status: Verification,
    pub source: String,
    pub quirks: Quirks,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    families: Vec<Family>,
    devices: Vec<DeviceEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// Malformed TOML or a field of the wrong shape.
    Toml(String),
    /// A `status` that is not allowed in a catalog file.
    BadStatus(String),
    /// A family or device without a `source`; holds the device name or family identity.
    EmptySource(String),
    /// A device with neither `pids` nor `wpids`; holds the device name.
    NoIds(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(msg) => write!(f, "invalid catalog: {msg}"),
            Self::BadStatus(s) => write!(f, "invalid catalog status {s:?}"),
            Self::EmptySource(who) => write!(f, "catalog entry {who} has no source"),
            Self::NoIds(who) => write!(f, "catalog entry {who} has neither pids nor wpids"),
        }
    }
}

impl std::error::Error for CatalogError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    #[serde(default)]
    family: Vec<RawFamily>,
    #[serde(default)]
    device: Vec<RawDevice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFamily {
    engine: EngineKind,
    vid: u16,
    status: String,
    source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    name: String,
    vid: u16,
    #[serde(default)]
    pids: Vec<u16>,
    #[serde(default)]
    wpids: Vec<u16>,
    kind: DeviceKind,
    engine: EngineKind,
    status: String,
    source: String,
    #[serde(default)]
    quirks: Quirks,
}

/// Maps a catalog status string. `generic` is rejected: it only describes devices with no entry.
fn parse_status(s: String) -> Result<Verification, CatalogError> {
    match s.as_str() {
        "hardware-tested" => Ok(Verification::HardwareTested),
        "source-verified" => Ok(Verification::SourceVerified),
        "community-tested" => Ok(Verification::CommunityTested),
        _ => Err(CatalogError::BadStatus(s)),
    }
}

/// Every built-in catalog file; adding a vendor file is one line here.
const BUILTIN_FILES: &[&str] = &[include_str!("../catalog/logitech.toml")];

impl Catalog {
    pub fn parse(toml: &str) -> Result<Catalog, CatalogError> {
        let raw: RawCatalog =
            toml::from_str(toml).map_err(|e| CatalogError::Toml(e.to_string()))?;

        let families = raw
            .family
            .into_iter()
            .map(|f| {
                let status = parse_status(f.status)?;
                if f.source.trim().is_empty() {
                    return Err(CatalogError::EmptySource(format!(
                        "{:?} family {:#06X}",
                        f.engine, f.vid
                    )));
                }
                Ok(Family {
                    engine: f.engine,
                    vid: f.vid,
                    status,
                    source: f.source,
                })
            })
            .collect::<Result<_, _>>()?;

        let devices = raw
            .device
            .into_iter()
            .map(|d| {
                let status = parse_status(d.status)?;
                if d.source.trim().is_empty() {
                    return Err(CatalogError::EmptySource(d.name));
                }
                if d.pids.is_empty() && d.wpids.is_empty() {
                    return Err(CatalogError::NoIds(d.name));
                }
                Ok(DeviceEntry {
                    name: d.name,
                    vid: d.vid,
                    pids: d.pids,
                    wpids: d.wpids,
                    kind: d.kind,
                    engine: d.engine,
                    status,
                    source: d.source,
                    quirks: d.quirks,
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Catalog { families, devices })
    }

    /// The catalog shipped with the app: every file in `catalog/`, concatenated and parsed.
    ///
    /// A parse error panics in debug builds; release builds fall back to an empty catalog so
    /// bad data can never crash the app (the builtin-catalog test catches it in CI first).
    pub fn builtin() -> Catalog {
        match Catalog::parse(&BUILTIN_FILES.join("\n")) {
            Ok(cat) => cat,
            Err(e) if cfg!(debug_assertions) => panic!("built-in catalog: {e}"),
            Err(_) => Catalog {
                families: Vec::new(),
                devices: Vec::new(),
            },
        }
    }

    pub fn families(&self) -> &[Family] {
        &self.families
    }

    pub fn devices(&self) -> &[DeviceEntry] {
        &self.devices
    }

    /// The first entry listing USB product id `pid` for vendor `vid`.
    pub fn device(&self, vid: u16, pid: u16) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|d| d.vid == vid && d.pids.contains(&pid))
    }

    /// The first entry listing wireless product id `wpid` for vendor `vid`.
    pub fn device_by_wpid(&self, vid: u16, wpid: u16) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|d| d.vid == vid && d.wpids.contains(&wpid))
    }

    pub fn family(&self, vid: u16, engine: EngineKind) -> Option<&Family> {
        self.families
            .iter()
            .find(|f| f.vid == vid && f.engine == engine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceKind, Verification};

    const SAMPLE: &str = r#"
[[family]]
engine = "hidpp"
vid = 0x046D
status = "source-verified"
source = "Linux drivers/hid/hid-logitech-hidpp.c; Solaar lib/logitech_receiver"

[[device]]
name = "PRO X Wireless Gaming Headset"
vid = 0x046D
pids = [0x0AAA, 0x0ABA]
kind = "headset"
engine = "hidpp"
status = "source-verified"
source = "G HUB pro_headset/pro_x_wireless_headset_battery.xml"
quirks = { voltage_curve = "pro-x-wireless-ghub" }
"#;

    #[test]
    fn parses_family_and_device() {
        let cat = Catalog::parse(SAMPLE).unwrap();
        let fam = cat.family(0x046D, EngineKind::Hidpp).unwrap();
        assert_eq!(fam.status, Verification::SourceVerified);
        assert_eq!(fam.vid, 0x046D);

        let dev = cat.device(0x046D, 0x0ABA).unwrap();
        assert_eq!(dev.name, "PRO X Wireless Gaming Headset");
        assert_eq!(dev.kind, DeviceKind::Headset);
        assert_eq!(dev.engine, EngineKind::Hidpp);
        assert_eq!(dev.pids, vec![0x0AAA, 0x0ABA]);
        assert_eq!(
            dev.quirks.voltage_curve.as_deref(),
            Some("pro-x-wireless-ghub")
        );
        assert!(std::ptr::eq(dev, cat.device(0x046D, 0x0AAA).unwrap()));
        assert_eq!(cat.device(0x046D, 0xC547), None);
        assert_eq!(cat.device(0x1532, 0x0ABA), None);
    }

    #[test]
    fn rejects_unverified_status() {
        let toml = SAMPLE.replace("source-verified", "unverified");
        assert_eq!(
            Catalog::parse(&toml).unwrap_err(),
            CatalogError::BadStatus("unverified".into())
        );
    }

    #[test]
    fn rejects_generic_status() {
        let toml = SAMPLE.replace("source-verified", "generic");
        assert_eq!(
            Catalog::parse(&toml).unwrap_err(),
            CatalogError::BadStatus("generic".into())
        );
    }

    #[test]
    fn accepts_all_file_statuses() {
        for (s, v) in [
            ("hardware-tested", Verification::HardwareTested),
            ("source-verified", Verification::SourceVerified),
            ("community-tested", Verification::CommunityTested),
        ] {
            let cat = Catalog::parse(&SAMPLE.replace("source-verified", s)).unwrap();
            assert_eq!(cat.family(0x046D, EngineKind::Hidpp).unwrap().status, v);
            assert_eq!(cat.device(0x046D, 0x0AAA).unwrap().status, v);
        }
    }

    #[test]
    fn rejects_empty_source() {
        let toml = SAMPLE.replace(
            "source = \"G HUB pro_headset/pro_x_wireless_headset_battery.xml\"",
            "source = \"\"",
        );
        assert_eq!(
            Catalog::parse(&toml).unwrap_err(),
            CatalogError::EmptySource("PRO X Wireless Gaming Headset".into())
        );
    }

    #[test]
    fn rejects_entry_without_ids() {
        let toml = SAMPLE.replace("pids = [0x0AAA, 0x0ABA]\n", "");
        assert_eq!(
            Catalog::parse(&toml).unwrap_err(),
            CatalogError::NoIds("PRO X Wireless Gaming Headset".into())
        );
        let empty = SAMPLE.replace("pids = [0x0AAA, 0x0ABA]", "pids = []\nwpids = []");
        assert_eq!(
            Catalog::parse(&empty).unwrap_err(),
            CatalogError::NoIds("PRO X Wireless Gaming Headset".into())
        );
    }

    #[test]
    fn rejects_empty_family_source() {
        let toml = SAMPLE.replace(
            "source = \"Linux drivers/hid/hid-logitech-hidpp.c; Solaar lib/logitech_receiver\"",
            "source = \" \"",
        );
        assert_eq!(
            Catalog::parse(&toml).unwrap_err(),
            CatalogError::EmptySource("Hidpp family 0x046D".into())
        );
    }

    #[test]
    fn quirks_default_when_absent() {
        let toml = SAMPLE.replace("quirks = { voltage_curve = \"pro-x-wireless-ghub\" }", "");
        let cat = Catalog::parse(&toml).unwrap();
        let q = &cat.device(0x046D, 0x0ABA).unwrap().quirks;
        assert!(!q.query_wakes);
        assert!(!q.no_in_use_push);
        assert_eq!(q.voltage_curve, None);
    }

    #[test]
    fn min_query_gap_is_not_a_quirk() {
        let toml = SAMPLE.replace(
            "quirks = { voltage_curve = \"pro-x-wireless-ghub\" }",
            "quirks = { min_query_gap_s = 600 }",
        );
        assert!(Catalog::parse(&toml).is_err());
    }

    #[test]
    fn parses_all_quirks() {
        let toml = SAMPLE.replace(
            "quirks = { voltage_curve = \"pro-x-wireless-ghub\" }",
            "quirks = { query_wakes = true, voltage_curve = \"pro-x-wireless-ghub\" }",
        );
        let cat = Catalog::parse(&toml).unwrap();
        let q = &cat.device(0x046D, 0x0ABA).unwrap().quirks;
        assert!(q.query_wakes);
        assert_eq!(q.voltage_curve.as_deref(), Some("pro-x-wireless-ghub"));
    }

    #[test]
    fn no_in_use_push_parses() {
        let toml = SAMPLE.replace(
            "quirks = { voltage_curve = \"pro-x-wireless-ghub\" }",
            "quirks = { no_in_use_push = true }",
        );
        let cat = Catalog::parse(&toml).unwrap();
        let q = &cat.device(0x046D, 0x0ABA).unwrap().quirks;
        assert!(q.no_in_use_push);
        assert!(!q.query_wakes);
    }

    #[test]
    fn malformed_toml_is_toml_error() {
        assert!(matches!(
            Catalog::parse("[[device]]\nname = ").unwrap_err(),
            CatalogError::Toml(_)
        ));
        let bad_kind = SAMPLE.replace("kind = \"headset\"", "kind = \"toaster\"");
        assert!(matches!(
            Catalog::parse(&bad_kind).unwrap_err(),
            CatalogError::Toml(_)
        ));
    }

    #[test]
    fn empty_catalog_parses() {
        let cat = Catalog::parse("").unwrap();
        assert_eq!(cat.device(0x046D, 0x0ABA), None);
        assert_eq!(cat.family(0x046D, EngineKind::Hidpp), None);
    }

    #[test]
    fn wireless_pids_match_receiver_devices_only_by_wpid() {
        let toml = format!(
            "{SAMPLE}\n{}",
            r#"
[[device]]
name = "PRO X Superlight"
vid = 0x046D
wpids = [0x4093]
kind = "mouse"
engine = "hidpp"
status = "hardware-tested"
source = "test"
"#
        );
        let cat = Catalog::parse(&toml).unwrap();
        let dev = cat.device_by_wpid(0x046D, 0x4093).unwrap();
        assert_eq!(dev.name, "PRO X Superlight");
        assert_eq!(dev.kind, DeviceKind::Mouse);
        assert!(dev.pids.is_empty());
        assert_eq!(dev.wpids, vec![0x4093]);
        // USB and wireless PIDs are separate name spaces.
        assert_eq!(cat.device(0x046D, 0x4093), None);
        assert_eq!(cat.device_by_wpid(0x046D, 0x0ABA), None);
        assert_eq!(cat.device_by_wpid(0x1532, 0x4093), None);
    }

    #[test]
    fn builtin_catalog_parses_and_resolves_owner_devices() {
        let cat = Catalog::builtin();
        assert_eq!(
            cat.family(0x046D, EngineKind::Hidpp).unwrap().status,
            Verification::SourceVerified
        );
        // The owner's headset (0ABA) is hardware-tested; the other PRO X Wireless dongle is not.
        let tested = cat.device(0x046D, 0x0ABA).unwrap();
        let other = cat.device(0x046D, 0x0AAA).unwrap();
        assert!(!std::ptr::eq(tested, other));
        assert_eq!(tested.status, Verification::HardwareTested);
        assert_eq!(other.status, Verification::SourceVerified);
        for dev in [tested, other] {
            assert_eq!(dev.name, "PRO X Wireless Gaming Headset");
            assert_eq!(dev.kind, DeviceKind::Headset);
            assert_eq!(
                dev.quirks.voltage_curve.as_deref(),
                Some("pro-x-wireless-ghub")
            );
            // Reports its battery only at power on and off: read on audio activity.
            assert!(dev.quirks.no_in_use_push);
        }
        // The owner's mouse behind its LIGHTSPEED receiver, by wireless PID only.
        let mouse = cat.device_by_wpid(0x046D, 0x4093).unwrap();
        assert_eq!(mouse.name, "PRO X Superlight");
        assert_eq!(mouse.kind, DeviceKind::Mouse);
        assert_eq!(mouse.status, Verification::HardwareTested);
        assert!(!mouse.quirks.no_in_use_push);
        // The same mouse on its cable: same name, cable path untested.
        let cabled = cat.device(0x046D, 0xC094).unwrap();
        assert_eq!(cabled.name, "PRO X Superlight");
        assert_eq!(cabled.kind, DeviceKind::Mouse);
        assert_eq!(cabled.status, Verification::SourceVerified);
        assert_eq!(cat.device(0x046D, 0xC547), None);

        let mut families = std::collections::HashSet::new();
        for f in cat.families() {
            assert!(!f.source.trim().is_empty(), "{f:?} has no source");
            assert!(families.insert((f.vid, f.engine)), "duplicate family {f:?}");
        }
        let (mut pids, mut wpids) = (
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        );
        for d in cat.devices() {
            assert!(!d.source.trim().is_empty(), "{} has no source", d.name);
            for &pid in &d.pids {
                assert!(
                    pids.insert((d.vid, pid)),
                    "{:#06X}:{pid:#06X} listed twice ({})",
                    d.vid,
                    d.name
                );
            }
            for &wpid in &d.wpids {
                assert!(
                    wpids.insert((d.vid, wpid)),
                    "{:#06X} wireless {wpid:#06X} listed twice ({})",
                    d.vid,
                    d.name
                );
            }
        }
    }
}
