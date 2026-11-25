//! Device catalog: which devices are supported, by which engine, and how well verified.

use std::fmt;

use serde::Deserialize;

use crate::hid::Collection;
use crate::model::{DeviceKind, Verification};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Hidpp,
    Rapoo,
    Steelseries,
    Hyperx,
    Sonix,
}

/// The input status format and any known wired PID for a Rapoo VT model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RapooConfig {
    pub report: RapooReport,
    pub wired_pid: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RapooReport {
    Nordic,
    Ch584,
    Legacy,
    LegacyWithUnknownLevel,
}

/// The status layout, battery scale and report ID of a SteelSeries USB PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteelseriesReport {
    Nova7Steps,
    Nova7Percent,
    Nova5Percent,
    Arctis7Plus,
    GameBuds,
    Station06Steps,
    Station01Percent,
}

/// The exact control report format used by a HyperX dongle or base station.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HyperxReport {
    Hx66,
    Alpha21bb,
    Universal,
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
    /// so it is read on observed activity (audio playing to it), never more often than every
    /// 5 minutes.
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

/// All specified fields must match the same HID top-level collection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollectionSelector {
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
    pub interface: Option<u8>,
    pub input_len: Option<u16>,
    pub output_len: Option<u16>,
    pub feature_len: Option<u16>,
    /// An ASCII case-insensitive substring of the OS-cached product name.
    pub product: Option<String>,
}

impl CollectionSelector {
    pub fn matches(&self, collection: &Collection) -> bool {
        self.matches_descriptor(collection) && self.matches_product(&collection.product)
    }

    pub(crate) fn matches_descriptor(&self, collection: &Collection) -> bool {
        self.usage_page.is_none_or(|v| v == collection.usage_page)
            && self.usage.is_none_or(|v| v == collection.usage)
            && self
                .interface
                .is_none_or(|v| Some(v) == collection.interface)
            && self.input_len.is_none_or(|v| v == collection.input_len)
            && self.output_len.is_none_or(|v| v == collection.output_len)
            && self.feature_len.is_none_or(|v| v == collection.feature_len)
    }

    pub fn matches_product(&self, name: &str) -> bool {
        self.product.as_ref().is_none_or(|needle| {
            !needle.is_empty()
                && name
                    .as_bytes()
                    .windows(needle.len())
                    .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        })
    }
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
    pub receive: Option<CollectionSelector>,
    pub send: Option<CollectionSelector>,
    pub rapoo: Option<RapooConfig>,
    pub steelseries: Option<SteelseriesReport>,
    pub hyperx: Option<HyperxReport>,
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
    receive: Option<CollectionSelector>,
    send: Option<CollectionSelector>,
    rapoo: Option<RapooConfig>,
    steelseries: Option<SteelseriesReport>,
    hyperx: Option<HyperxReport>,
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
const BUILTIN_FILES: &[&str] = &[
    include_str!("../catalog/logitech.toml"),
    include_str!("../catalog/rapoo.toml"),
    include_str!("../catalog/steelseries.toml"),
    include_str!("../catalog/hyperx.toml"),
    include_str!("../catalog/pulsar.toml"),
];

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
                    receive: d.receive,
                    send: d.send,
                    rapoo: d.rapoo,
                    steelseries: d.steelseries,
                    hyperx: d.hyperx,
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

    /// Candidates in the direct USB ID namespace, before collection matching.
    pub(crate) fn direct_entries(
        &self,
        vid: u16,
        pid: u16,
    ) -> impl Iterator<Item = (usize, &DeviceEntry)> {
        self.devices
            .iter()
            .enumerate()
            .filter(move |(_, d)| d.vid == vid && d.pids.contains(&pid))
    }

    #[cfg(test)]
    fn device(&self, vid: u16, pid: u16) -> Option<&DeviceEntry> {
        self.direct_entries(vid, pid).next().map(|(_, entry)| entry)
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
    fn parses_distinct_receive_and_send_selectors() {
        let cat = Catalog::parse(
            r#"
[[device]]
name = "Synthetic HID fixture"
vid = 0x1234
pids = [0x5678]
kind = "headset"
engine = "hidpp"
status = "source-verified"
source = "synthetic selector fixture"
receive = { usage_page = 0xFF00, usage = 2, interface = 3, input_len = 20 }
send = { usage_page = 0xFF00, usage = 1, interface = 4, output_len = 7, feature_len = 0, product = "FiXtUrE" }
"#,
        )
        .unwrap();
        let entry = &cat.devices()[0];
        let mut rx = Collection {
            id: crate::hid::CollectionId(1),
            vid: 0x1234,
            pid: 0x5678,
            usage_page: 0xFF00,
            usage: 2,
            interface: Some(3),
            input_len: 20,
            input_report_05: None,
            output_len: 0,
            feature_len: 0,
            product: String::new(),
        };
        let receive = entry.receive.as_ref().unwrap();
        let send = entry.send.as_ref().unwrap();
        assert!(receive.matches(&rx));
        assert!(!send.matches(&rx));
        rx.interface = Some(4);
        rx.usage = 1;
        rx.output_len = 7;
        assert!(!send.matches(&rx), "missing required name");
        rx.product = "USB fixture protocol".into();
        assert!(send.matches(&rx));
        assert!(!receive.matches(&rx));
        rx.product = "USB fİxture protocol".into();
        assert!(!send.matches(&rx), "case-insensitive only for ASCII");
    }

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
                    pids.insert((d.vid, pid, &d.receive, &d.send)),
                    "{:#06X}:{pid:#06X} listed twice with the same selectors ({})",
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
