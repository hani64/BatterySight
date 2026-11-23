//! Replays HID++ fixture transcripts through a real [`Hub`] with the built-in
//! engines and checks every write against the transcript.
//!
//! Fixture lines are `out <short|long> <hex>` (a frame we write) or
//! `in <short|long> <hex>` (a frame we read), `#` starts a comment. The `out`
//! lines between two `in` lines are exactly the writes the first one causes.

use batterysight_core::catalog::Catalog;
use batterysight_core::hid::{Collection, CollectionId, GroupId, HidGroup};
use batterysight_core::hub::{Hub, HubInput, HubOutput};
use batterysight_core::model::{Charge, CoarseLevel, DeviceKind, Level, Presence, Via};
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
"#;

const SHORT: CollectionId = CollectionId(1);
const LONG: CollectionId = CollectionId(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    In,
    Out,
}

struct Frame {
    line: usize,
    dir: Dir,
    collection: CollectionId,
    bytes: Vec<u8>,
}

fn parse(name: &str, text: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let dir = match words.next() {
            Some("in") => Dir::In,
            Some("out") => Dir::Out,
            other => panic!("{name}:{}: bad direction {other:?}", n + 1),
        };
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
        frames.push(Frame {
            line: n + 1,
            dir,
            collection,
            bytes,
        });
    }
    frames
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
        input_len: len,
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
}

/// Replays `text` and returns the hub for state checks.
fn replay(name: &str, text: &str, group: HidGroup) -> Hub {
    let frames = parse(name, text);
    let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
    let mut now = 0;
    let mut sent = sends(hub.handle(HubInput::GroupArrived(group), now));
    let mut expected = Vec::new();
    for frame in frames {
        match frame.dir {
            Dir::Out => expected.push((frame.collection, frame.bytes)),
            Dir::In => {
                assert_eq!(sent, expected, "{name}: writes before line {}", frame.line);
                now += 1;
                sent.extend(sends(hub.handle(
                    HubInput::Report {
                        collection: frame.collection,
                        bytes: frame.bytes,
                    },
                    now,
                )));
            }
        }
    }
    assert_eq!(sent, expected, "{name}: writes at end of transcript");
    hub
}

fn sends(out: Vec<HubOutput>) -> Vec<(CollectionId, Vec<u8>)> {
    out.into_iter()
        .filter_map(|o| match o {
            HubOutput::Send {
                collection,
                bytes,
                timeout_ms,
            } => {
                assert_eq!(timeout_ms, 1_000);
                Some((collection, bytes))
            }
            HubOutput::StateChanged => None,
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
    assert_eq!(s.presence, Presence::Online, "{name}");
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
            level: Level::Percent(87),
            millivolts: Some(3971),
            charge: Charge::Charging,
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
            key: "logi:unit:FFFFFFFF",
            name: "PRO X Wireless Gaming Headset",
            kind: DeviceKind::Headset,
            via: Via::Usb,
            level: Level::Percent(68),
            millivolts: Some(3828),
            charge: Charge::Discharging,
        },
    );
}
