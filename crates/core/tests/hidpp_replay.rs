//! Replays HID++ fixture transcripts through a real [`Hub`] with the built-in
//! engines and checks every write against the transcript.
//!
//! Fixture lines are `out <short|long> <hex>` (a frame we write),
//! `in <short|long> <hex>` (a frame we read), `@t+<ms>` (time passes, then the
//! hub gets a `Tick`) or `resume` (the system resumed from sleep); `#` starts a
//! comment. Each `in` and `resume` advances the clock by 1 ms. The `out` lines
//! between two inputs are exactly the writes the first one causes.

use batterysight_core::catalog::Catalog;
use batterysight_core::hid::{Collection, CollectionId, GroupId, HidGroup, ReportKind, ReportOrigin};
use batterysight_core::hub::{Hub, HubInput, HubOutput};
use batterysight_core::model::{
    Charge, CoarseLevel, DeviceKind, Level, Presence, Verification, Via,
};
use batterysight_core::scheduler::Policy;

const CATALOG: &str = r#"
[[family]]
engine = "hidpp"
vid = 0x046D
status = "source-verified"
source = "test"

[[device]]
name = "PRO X Wireless Gaming Headset"
vid = 0x046D
pids = [0x0ABA]
kind = "headset"
engine = "hidpp"
status = "source-verified"
source = "test"
quirks = { voltage_curve = "pro-x-wireless-ghub" }

[[device]]
name = "PRO X Superlight"
vid = 0x046D
wpids = [0x4093]
kind = "mouse"
engine = "hidpp"
status = "hardware-tested"
source = "test"
"#;

const SHORT: CollectionId = CollectionId(1);
const LONG: CollectionId = CollectionId(2);

/// One transcript line.
enum Entry {
    In(CollectionId, Vec<u8>),
    Out(CollectionId, Vec<u8>),
    /// Milliseconds that pass before the next `Tick`.
    Wait(u64),
    /// System resume.
    Resume,
}

fn parse(name: &str, text: &str) -> Vec<(usize, Entry)> {
    let mut entries = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(ms) = line.strip_prefix("@t+") {
            let ms = ms
                .parse()
                .unwrap_or_else(|_| panic!("{name}:{}: bad wait {ms:?}", n + 1));
            entries.push((n + 1, Entry::Wait(ms)));
            continue;
        }
        if line == "resume" {
            entries.push((n + 1, Entry::Resume));
            continue;
        }
        let mut words = line.split_whitespace();
        let dir = words.next();
        let (collection, len) = match words.next() {
            Some("short") => (SHORT, 7),
            Some("long") => (LONG, 20),
            other => panic!("{name}:{}: bad role {other:?}", n + 1),
        };
        let mut bytes: Vec<u8> = words
            .map(|w| u8::from_str_radix(w, 16).unwrap_or_else(|_| panic!("{name}:{}: {w}", n + 1)))
            .collect();
        assert!(bytes.len() <= len, "{name}:{}: frame too long", n + 1);
        bytes.resize(len, 0);
        let entry = match dir {
            Some("in") => Entry::In(collection, bytes),
            Some("out") => Entry::Out(collection, bytes),
            other => panic!("{name}:{}: bad direction {other:?}", n + 1),
        };
        entries.push((n + 1, entry));
    }
    entries
}

fn collection(
    id: CollectionId,
    usage_page: u16,
    usage: u16,
    len: u16,
    pid: u16,
    product: &str,
) -> Collection {
    Collection {
        id,
        vid: 0x046D,
        pid,
        usage_page,
        usage,
        interface: None,
        input_len: len,
        input_report_05: None,
        output_len: len,
        feature_len: 0,
        product: product.into(),
    }
}

fn receiver() -> HidGroup {
    HidGroup {
        id: GroupId(7),
        collections: vec![
            collection(SHORT, 0xFF00, 0x0001, 7, 0xC547, "USB Receiver"),
            collection(LONG, 0xFF00, 0x0002, 20, 0xC547, "USB Receiver"),
        ],
    }
}

fn headset() -> HidGroup {
    HidGroup {
        id: GroupId(8),
        collections: vec![collection(
            LONG,
            0xFF43,
            0x0202,
            20,
            0x0ABA,
            "PRO X Wireless Gaming Headset",
        )],
    }
}

/// Final state of one device after the replay.
struct Expect {
    key: &'static str,
    name: &'static str,
    kind: DeviceKind,
    via: Via,
    level: Level,
    millivolts: Option<u16>,
    charge: Charge,
    presence: Presence,
}

/// Replays `text` and returns the hub for state checks.
fn replay(name: &str, text: &str, group: HidGroup) -> Hub {
    let entries = parse(name, text);
    let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
    let mut now = 0;
    let mut sent = sends(hub.handle(HubInput::GroupArrived(group), now));
    let mut expected = Vec::new();
    for (line, entry) in entries {
        let input = match entry {
            Entry::Out(collection, bytes) => {
                expected.push((collection, bytes));
                continue;
            }
            Entry::In(collection, bytes) => {
                now += 1;
                HubInput::Report {
                    collection,
                    origin: ReportOrigin::Input,
                    bytes,
                }
            }
            Entry::Wait(ms) => {
                now += ms;
                HubInput::Tick
            }
            Entry::Resume => {
                now += 1;
                HubInput::Resume
            }
        };
        assert_eq!(sent, expected, "{name}: writes before line {line}");
        sent.extend(sends(hub.handle(input, now)));
    }
    assert_eq!(sent, expected, "{name}: writes at end of transcript");
    hub
}

/// The writes in `out`.
fn sends(out: Vec<HubOutput>) -> Vec<(CollectionId, Vec<u8>)> {
    out.into_iter()
        .filter_map(|o| match o {
            HubOutput::Send {
                collection,
                kind: ReportKind::Output,
                bytes,
                deadline: None,
            } => Some((collection, bytes)),
            HubOutput::Send { kind, .. } => panic!("unexpected HID++ operation: {kind:?}"),
            HubOutput::WriteRefused {
                kind,
                collection,
                reason,
            } => panic!("unexpected HID++ refusal: {kind:?} {collection:?} {reason:?}"),
            HubOutput::StateChanged | HubOutput::DiscoveryFailed { .. } => None,
        })
        .collect()
}

fn check(hub: &Hub, name: &str, want: &Expect) {
    let snap = hub.snapshot();
    assert_eq!(snap.len(), 1, "{name}: {snap:?}");
    let s = &snap[0];
    assert_eq!(s.key.0, want.key, "{name}");
    assert_eq!(s.name, want.name, "{name}");
    assert_eq!(s.kind, want.kind, "{name}");
    assert_eq!(s.via, want.via, "{name}");
    assert_eq!(s.level, Some(want.level), "{name}");
    assert_eq!(s.millivolts, want.millivolts, "{name}");
    assert_eq!(s.charge, want.charge, "{name}");
    assert_eq!(s.presence, want.presence, "{name}");
}

/// The lines of `text` up to and including the first one containing `marker`.
fn until(text: &str, marker: &str) -> String {
    let end = text
        .find(marker)
        .unwrap_or_else(|| panic!("no {marker:?} in fixture"));
    let end = text[end..].find('\n').map_or(text.len(), |n| end + n + 1);
    text[..end].to_owned()
}

#[test]
fn receiver_mouse_discovery() {
    let name = "receiver_mouse_discovery.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/receiver_mouse_discovery.txt"),
        receiver(),
    );
    check(
        &hub,
        name,
        &Expect {
            key: "logi:unit:1A2B3C4D",
            name: "PRO X Superlight",
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: Level::Percent(78),
            millivolts: None,
            charge: Charge::Discharging,
            presence: Presence::Online,
        },
    );
}

#[test]
fn headset_long_only_discovery() {
    let name = "headset_long_only_discovery.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/headset_long_only_discovery.txt"),
        headset(),
    );
    check(
        &hub,
        name,
        &Expect {
            key: "logi:0ABA:255",
            name: "PRO X Wireless Gaming Headset",
            kind: DeviceKind::Headset,
            via: Via::Usb,
            // Charging at 3971 mV (ascending 14 %) after the discovery read 68 % discharging:
            // a charge never reads below the level before it.
            level: Level::Percent(68),
            millivolts: Some(3971),
            charge: Charge::Charging,
            presence: Presence::Online,
        },
    );
}

#[test]
fn receiver_hidpp10_discovery() {
    let name = "receiver_hidpp10_discovery.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/receiver_hidpp10_discovery.txt"),
        receiver(),
    );
    check(
        &hub,
        name,
        &Expect {
            key: "logi:C547:1",
            name: "USB Receiver",
            kind: DeviceKind::Other,
            via: Via::Receiver,
            level: Level::Coarse(CoarseLevel::Low),
            millivolts: None,
            charge: Charge::Discharging,
            presence: Presence::Online,
        },
    );
}

#[test]
fn headset_off_at_start() {
    let name = "0aba-headset-off-at-start.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/0aba-headset-off-at-start.txt"),
        headset(),
    );
    check(
        &hub,
        name,
        &Expect {
            // Unit id FFFFFFFF is a placeholder: pid and index instead.
            key: "logi:0ABA:255",
            name: "PRO X Wireless Gaming Headset",
            kind: DeviceKind::Headset,
            via: Via::Usb,
            level: Level::Percent(68),
            millivolts: Some(3828),
            charge: Charge::Discharging,
            presence: Presence::Online,
        },
    );
}

/// The mouse of the c547 fixtures, registered at 80 %.
fn c547_mouse() -> Expect {
    Expect {
        key: "logi:unit:1A2B3C4D",
        name: "PRO X Superlight",
        kind: DeviceKind::Mouse,
        via: Via::Receiver,
        level: Level::Percent(80),
        millivolts: None,
        charge: Charge::Discharging,
        presence: Presence::Online,
    }
}

#[test]
fn c547_slow_ping() {
    let name = "c547-slow-ping.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/c547-slow-ping.txt"),
        receiver(),
    );
    check(&hub, name, &c547_mouse());
}

#[test]
fn c547_late_ping_reply() {
    let name = "c547-late-ping-reply.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/c547-late-ping-reply.txt"),
        receiver(),
    );
    check(&hub, name, &c547_mouse());
}

/// The Superlight of the owner captures, named by its catalog entry (wireless PID 0x4093 from
/// the receiver's 0x41); it calls itself "PRO X Wireless". Its unit id is a synthetic one.
fn superlight(level: u8, presence: Presence) -> Expect {
    Expect {
        key: "logi:unit:1A2B3C4D",
        name: "PRO X Superlight",
        kind: DeviceKind::Mouse,
        via: Via::Receiver,
        level: Level::Percent(level),
        millivolts: None,
        charge: Charge::Discharging,
        presence,
    }
}

/// The PRO X Wireless headset of the owner captures at `millivolts`, discharging.
fn pro_x_headset(level: u8, millivolts: u16, presence: Presence) -> Expect {
    Expect {
        key: "logi:0ABA:255",
        name: "PRO X Wireless Gaming Headset",
        kind: DeviceKind::Headset,
        via: Via::Usb,
        level: Level::Percent(level),
        millivolts: Some(millivolts),
        charge: Charge::Discharging,
        presence,
    }
}

#[test]
fn c547_superlight_arrival() {
    let name = "c547-superlight-arrival.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/c547-superlight-arrival.txt"),
        receiver(),
    );
    check(&hub, name, &superlight(80, Presence::Online));
    assert_eq!(hub.snapshot()[0].verification, Verification::HardwareTested);
}

#[test]
fn c547_superlight_resume() {
    let name = "c547-superlight-resume.txt";
    let text = include_str!("fixtures/hidpp/c547-superlight-resume.txt");
    // Link lost after the 80 % read and push: asleep, level kept.
    let hub = replay(name, &until(text, "# +2669.168 s"), receiver());
    check(&hub, name, &superlight(80, Presence::Asleep));
    let hub = replay(name, text, receiver());
    check(&hub, name, &superlight(79, Presence::Online));
}

#[test]
fn headset_arrival() {
    let name = "0aba-headset-arrival.txt";
    let text = include_str!("fixtures/hidpp/0aba-headset-arrival.txt");
    // Switched on: 0x0F81 = 3969 mV on the PRO X curve: 85 + 5 * (3969 - 3950) / (3993 - 3950),
    // truncated.
    let hub = replay(name, &until(text, "# +45.038 s"), headset());
    check(&hub, name, &pro_x_headset(87, 3969, Presence::Online));
    // Last switched on at 0x0F7E = 3966 mV (86 %), then off: asleep, level kept.
    let hub = replay(name, text, headset());
    check(&hub, name, &pro_x_headset(86, 3966, Presence::Asleep));
}

#[test]
fn headset_resume() {
    let name = "0aba-headset-resume.txt";
    let hub = replay(
        name,
        include_str!("fixtures/hidpp/0aba-headset-resume.txt"),
        headset(),
    );
    // 0x0F62 = 3938 mV: 80 + 5 * (3938 - 3915) / (3950 - 3915), truncated.
    check(&hub, name, &pro_x_headset(83, 3938, Presence::Online));
}

#[test]
fn headset_stalled_discovery_is_retried() {
    let name = "0aba-headset-stalled-discovery.txt";
    let text = include_str!("fixtures/hidpp/0aba-headset-stalled-discovery.txt");
    // Before the retry nothing is registered, the broadcast included.
    let hub = replay(name, &until(text, "11 FF 06 00 0E B2 01"), headset());
    assert!(hub.snapshot().is_empty(), "{:?}", hub.snapshot());
    // 0x0EB2 = 3762 mV on the PRO X curve: 50 + 5 * (3762 - 3758) / (3772 - 3758), truncated.
    let hub = replay(name, text, headset());
    check(&hub, name, &pro_x_headset(51, 3762, Presence::Online));
}
