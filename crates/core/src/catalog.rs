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
    /// Minimum seconds between active queries.
    pub min_query_gap_s: Option<u32>,
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
    pub pids: Vec<u16>,
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
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(msg) => write!(f, "invalid catalog: {msg}"),
            Self::BadStatus(s) => write!(f, "invalid catalog status {s:?}"),
            Self::EmptySource(who) => write!(f, "catalog entry {who} has no source"),
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
    pids: Vec<u16>,
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
                Ok(DeviceEntry {
                    name: d.name,
                    vid: d.vid,
                    pids: d.pids,
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

    /// The first entry listing `pid` for vendor `vid`.
    pub fn device(&self, vid: u16, pid: u16) -> Option<&DeviceEntry> {
        self.devices
            .iter()
            .find(|d| d.vid == vid && d.pids.contains(&pid))
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
source = "G HUB pro_headset/pro_x_wireless_headset_battery.xml (BatterySight issue notes); BatteryHub PR #3470"
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
            "source = \"G HUB pro_headset/pro_x_wireless_headset_battery.xml (BatterySight issue notes); BatteryHub PR #3470\"",
            "source = \"\"",
        );
        assert_eq!(
            Catalog::parse(&toml).unwrap_err(),
            CatalogError::EmptySource("PRO X Wireless Gaming Headset".into())
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
        assert_eq!(q.voltage_curve, None);
        assert_eq!(q.min_query_gap_s, None);
    }

    #[test]
    fn parses_all_quirks() {
        let toml = SAMPLE.replace(
            "quirks = { voltage_curve = \"pro-x-wireless-ghub\" }",
            "quirks = { query_wakes = true, min_query_gap_s = 600 }",
        );
        let cat = Catalog::parse(&toml).unwrap();
        let q = &cat.device(0x046D, 0x0ABA).unwrap().quirks;
        assert!(q.query_wakes);
        assert_eq!(q.min_query_gap_s, Some(600));
        assert_eq!(q.voltage_curve, None);
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
}
