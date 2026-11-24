//! Replays synthesized vendor frames through the real catalog, selection and hub.

use batterysight_core::catalog::Catalog;
use batterysight_core::engines::rapoo::RapooEngine;
use batterysight_core::hid::{
    Collection, CollectionId, GroupId, HidGroup, ReportKind, ReportOrigin,
};
use batterysight_core::hub::{Hub, HubInput, HubOutput};
use batterysight_core::model::{Charge, DeviceKind, Level, Presence, Source, Verification, Via};
use batterysight_core::scheduler::Policy;
use batterysight_core::time::Ms;

const RX: CollectionId = CollectionId(1);
const OTHER: CollectionId = CollectionId(2);

// Source layouts: pspfreak/rapoo-mouse-battery (FF00:0002, 19 bytes), Rapoo A HUB
// VT_CH584 manifest and EB.handleReport (FF00:000F, offsets with report ID),
// and OpenMouse's VT9 Pro receiver (FF00:0002, 7 bytes, beside a 10-byte sibling).
const NORDIC: [u8; 19] = [
    0x07, 0x20, 3, 0x40, 6, 0x40, 6, 2, 65, 0, 1, 1, 0, 0x13, 0x46, 0, 0, 0, 0xCC,
];
const CH584: [u8; 19] = [
    0x08, 0x20, 3, 0x40, 6, 0x40, 6, 1, 42, 0, 1, 1, 0, 0x11, 0x4A, 0, 0, 0, 0xCC,
];
const LEGACY: [u8; 7] = [0xBB, 0xB0, 0x51, 0x20, 3, 1, 98];

fn collection(pid: u16, usage: u16, len: u16) -> Collection {
    Collection {
        id: RX,
        vid: 0x24AE,
        pid,
        usage_page: 0xFF00,
        usage,
        interface: Some(1),
        input_len: len,
        output_len: len,
        feature_len: 0,
        product: String::new(),
    }
}

fn group(rx: Collection) -> HidGroup {
    HidGroup {
        id: GroupId(1),
        collections: vec![rx],
    }
}

fn feed(hub: &mut Hub, input: HubInput, now: Ms) -> Vec<HubOutput> {
    let out = hub.handle(input, now);
    assert!(
        out.iter().all(|event| *event == HubOutput::StateChanged),
        "passive engine emitted {out:?} at {now}"
    );
    out
}

fn report(hub: &mut Hub, bytes: &[u8], now: Ms) -> Vec<HubOutput> {
    feed(
        hub,
        HubInput::Report {
            collection: RX,
            origin: ReportOrigin::Input,
            bytes: bytes.to_vec(),
        },
        now,
    )
}

fn hub() -> Hub {
    Hub::new(Catalog::builtin(), Policy::default())
}

fn replay(rx: Collection, bytes: &[u8], name: &str, level: u8, via: Via) {
    let mut hub = hub();
    assert!(feed(&mut hub, HubInput::GroupArrived(group(rx)), 0).is_empty());
    assert!(hub.snapshot().is_empty());
    assert_eq!(report(&mut hub, bytes, 10), [HubOutput::StateChanged]);
    let snap = hub.snapshot();
    assert_eq!(snap.len(), 1);
    let state = &snap[0];
    assert_eq!(state.name, name);
    assert_eq!(state.kind, DeviceKind::Mouse);
    assert_eq!(state.via, via);
    assert_eq!(state.level, Some(Level::Percent(level)));
    assert_eq!(state.charge, Charge::Unknown);
    assert_eq!(state.presence, Presence::Online);
    assert_eq!(state.source, Source::Push);
    assert_eq!(state.updated_ms, 10);
    assert_eq!(state.verification, Verification::SourceVerified);
    assert!(state.reports_changes);
    assert_eq!(state.poll_min_ms, None);
}

#[test]
fn rapoo_vt7_nordic_07_receiver_and_cable() {
    replay(
        collection(0x1413, 2, 19),
        &NORDIC,
        "Rapoo VT7",
        65,
        Via::Receiver,
    );
    let mut wired = NORDIC;
    wired[1] = 0x22;
    replay(collection(0x4613, 2, 19), &wired, "Rapoo VT7", 65, Via::Usb);
}

#[test]
fn rapoo_vt2_pro_max_nordic_07_short_status() {
    // renanleonel/windows-taskbar-battery src/rapoo.rs: VT2 Pro Max capture.
    replay(
        collection(0x1423, 2, 13),
        &NORDIC[..13],
        "Rapoo VT2 Pro Max",
        65,
        Via::Receiver,
    );
}

#[test]
fn rapoo_vt9_pro_v2_nordic_07_receiver_and_cable() {
    // k997/rapoo readme.md: 18 data bytes, plus the report ID on Windows.
    let mut status = NORDIC;
    status[13] = 0xA9;
    replay(
        collection(0x14A9, 2, 19),
        &status,
        "Rapoo VT9 Pro V2",
        65,
        Via::Receiver,
    );
    status[1] = 0x22;
    replay(
        collection(0x46A9, 2, 19),
        &status,
        "Rapoo VT9 Pro V2",
        65,
        Via::Usb,
    );
}

#[test]
fn rapoo_vt1_se_ch584_08_receiver_and_cable() {
    replay(
        collection(0x1A11, 0x0F, 19),
        &CH584,
        "Rapoo VT1 SE",
        42,
        Via::Receiver,
    );
    let mut wired = CH584;
    wired[1] = 0x22;
    replay(collection(0x4A11, 0x0F, 19), &wired, "Rapoo VT1 SE", 42, Via::Usb);
}

#[test]
fn rapoo_vt7_se_ch584_08_receiver_and_cable() {
    let mut status = CH584;
    status[13] = 0;
    replay(
        collection(0x1A00, 0x0F, 19),
        &status,
        "Rapoo VT7 SE",
        42,
        Via::Receiver,
    );
    status[1] = 0x22;
    replay(collection(0x4A00, 0x0F, 19), &status, "Rapoo VT7 SE", 42, Via::Usb);
}

#[test]
fn rapoo_vt9_pro_legacy_bb_b0() {
    replay(
        collection(0x1205, 2, 7),
        &LEGACY,
        "Rapoo VT9 Pro (1st gen)",
        98,
        Via::Receiver,
    );
}

#[test]
fn rapoo_vt9_pro_185a_legacy_bb_b0() {
    // wuxx7/rapoo-vt9pro-battery rapoo_proto.py: 0x42 mode, not 0x51.
    let mut status = LEGACY;
    status[2] = 0x42;
    replay(
        collection(0x185A, 2, 7),
        &status,
        "Rapoo VT9 Pro",
        98,
        Via::Receiver,
    );
}

#[test]
fn rapoo_vt3_pro_legacy_unknown_level_does_not_replace_last_percent() {
    // PyFlat RapooProtocol.cs documents 0x7F only with state 2.
    let mut hub = hub();
    feed(&mut hub, HubInput::GroupArrived(group(collection(0x1215, 2, 7))), 0);
    let unavailable = [0xBB, 0xB0, 0x51, 0x20, 3, 2, 0x7F];
    assert_eq!(report(&mut hub, &LEGACY, 1), [HubOutput::StateChanged]);
    assert!(report(&mut hub, &[0xBB, 0xB0, 0x51, 0x20, 3, 1, 0x7F], 2).is_empty());
    let out = report(&mut hub, &unavailable, 3);
    let online = hub.snapshot();
    assert_eq!(online[0].updated_ms, 1, "presence is not a new measurement");
    assert_eq!(online[0].presence, Presence::Online);
    assert_eq!(online[0].level, Some(Level::Percent(98)));
    assert_eq!(online[0].charge, Charge::Unknown);
    assert!(out.is_empty(), "no snapshot change while already online");
    assert_eq!(hub.next_deadline(), Some(60_003));
    assert!(feed(&mut hub, HubInput::Tick, 60_002).is_empty());
    assert_eq!(
        feed(&mut hub, HubInput::Tick, 60_003),
        [HubOutput::StateChanged]
    );
    assert_eq!(hub.snapshot()[0].presence, Presence::Asleep);
    assert_eq!(
        report(&mut hub, &unavailable, 60_004),
        [HubOutput::StateChanged]
    );
    assert_eq!(
        hub.snapshot(),
        online,
        "wake without a new battery measurement"
    );
    assert_eq!(hub.next_deadline(), Some(120_004));

    let mut unread = Hub::new(Catalog::builtin(), Policy::default());
    feed(&mut unread, HubInput::GroupArrived(group(collection(0x1215, 2, 7))), 0);
    assert_eq!(report(&mut unread, &unavailable, 1), [HubOutput::StateChanged]);
    let initial = unread.snapshot();
    assert_eq!(initial[0].presence, Presence::Online);
    assert_eq!(initial[0].level, None);
    assert_eq!(unread.next_deadline(), Some(60_001));
}

#[test]
fn rapoo_vt3_pro_legacy_bb_b0_receiver_and_cable() {
    // PyFlat RapooProtocol.cs: exact 7-byte collection and both USB IDs.
    let mut status = LEGACY;
    status[5] = 2;
    replay(
        collection(0x1215, 2, 7),
        &status,
        "Rapoo VT3 Pro",
        98,
        Via::Receiver,
    );
    replay(collection(0x4415, 2, 7), &status, "Rapoo VT3 Pro", 98, Via::Usb);
}

#[test]
fn rapoo_rejects_wrong_roles_and_models() {
    let cases = [
        (
            "consumer page",
            Collection {
                usage_page: 0x000C,
                ..collection(0x1205, 2, 7)
            },
        ),
        ("mailbox usage", collection(0x1205, 0x0E, 7)),
        ("same-usage legacy sibling", collection(0x1205, 2, 10)),
        ("office model", collection(0x1870, 2, 7)),
        ("conflicting PID", collection(0x1417, 2, 7)),
        ("unverified legacy model", collection(0x1233, 2, 7)),
        (
            "foreign VID",
            Collection {
                vid: 0x1234,
                ..collection(0x1205, 2, 7)
            },
        ),
    ];
    let hub = hub();
    for (label, rx) in cases {
        assert!(
            hub.select_group(&group(rx), |_, _| None).is_none(),
            "{label}"
        );
    }
    assert!(
        hub.select_group(&group(collection(0x1A11, 2, 19)), |_, _| None)
            .is_none()
    );
    let mut g = group(collection(0x1205, 2, 7));
    g.collections.push(Collection {
        id: OTHER,
        ..g.collections[0].clone()
    });
    assert!(
        hub.select_group(&g, |_, _| None).is_none(),
        "ambiguous exact RX"
    );
}

#[test]
fn rapoo_passive_selection_requires_only_the_preselected_receive_path() {
    let catalog = Catalog::builtin();
    let entry = catalog
        .devices()
        .iter()
        .find(|entry| entry.vid == 0x24AE && entry.pids.contains(&0x1205))
        .unwrap();
    let mut g = group(collection(0x1205, 2, 7));
    g.collections.push(Collection {
        id: OTHER,
        ..collection(0x1205, 2, 10)
    });
    assert!(RapooEngine::new(&g, entry).is_some());
    let mut hub = Hub::new(catalog, Policy::default());
    let selection = hub.select_group(&g, |_, _| None).expect("select passive RX");
    assert_eq!(selection.command_report_id(), None);
    assert!(selection.opens(RX));
    assert!(selection.reads(RX));
    assert!(
        !selection.sends(RX),
        "output-capable descriptor must still open read-only"
    );
    assert!(!selection.opens(OTHER));
    assert!(!selection.sends(OTHER));
    assert!(selection.ready(&g));
    assert!(!selection.ready(&HidGroup {
        id: g.id,
        collections: vec![g.collections[1].clone()],
    }));

    feed(&mut hub, HubInput::MatchedGroupArrived(g, selection), 0);
    for (collection, origin) in [(OTHER, ReportOrigin::Input), (RX, ReportOrigin::FeatureReply)] {
        assert!(
            feed(
                &mut hub,
                HubInput::Report {
                    collection,
                    origin,
                    bytes: LEGACY.to_vec(),
                },
                1,
            )
            .is_empty()
        );
        assert!(hub.snapshot().is_empty());
    }
    assert_eq!(report(&mut hub, &LEGACY, 2), [HubOutput::StateChanged]);
}

#[test]
fn rapoo_rejects_cross_model_malformed_and_cybercafe_frames() {
    for (rx, frame, wrong_id, offset) in [
        (collection(0x1413, 2, 19), &NORDIC[..], 0x08, 8),
        (collection(0x1A11, 0x0F, 19), &CH584[..], 0x07, 8),
        (collection(0x1205, 2, 7), &LEGACY[..], 0xBC, 6),
    ] {
        let mut hub = hub();
        feed(&mut hub, HubInput::GroupArrived(group(rx)), 0);
        assert_eq!(report(&mut hub, frame, 100), [HubOutput::StateChanged]);
        let before = hub.snapshot();
        let mut other_id = frame.to_vec();
        other_id[0] = wrong_id;
        let mut too_large = frame.to_vec();
        too_large.push(0);
        let mut out_of_range = frame.to_vec();
        out_of_range[offset] = 101;
        let mut bad_status = frame.to_vec();
        bad_status[offset - 1] = 0xFF;
        for invalid in [
            &other_id[..],
            &too_large,
            &frame[..frame.len() - 1],
            &out_of_range,
            &bad_status,
            &[],
        ] {
            assert!(report(&mut hub, invalid, 5_099).is_empty(), "{invalid:02X?}");
        }
        for event in [0xB1, 0xB2, 0xB3] {
            let mut invalid = frame.to_vec();
            invalid[1] = event;
            assert!(report(&mut hub, &invalid, 5_099).is_empty(), "{invalid:02X?}");
        }
        if offset == 8 {
            let mut foreign = frame.to_vec();
            foreign[13..15].copy_from_slice(&0x466Du16.to_le_bytes());
            assert!(
                report(&mut hub, &foreign, 5_099).is_empty(),
                "foreign embedded model PID"
            );
            foreign.copy_from_slice(frame);
            foreign[1] = 0xB0; // Cybercafe status wrapper is not the cataloged retail body.
            assert!(report(&mut hub, &foreign, 5_099).is_empty());
        }
        assert_eq!(hub.snapshot(), before);
        assert_eq!(hub.next_deadline(), Some(60_100));
    }
}

#[test]
fn rapoo_refresh_preserves_last_reading_and_natural_reports_wake_after_idle() {
    let mut hub = hub();
    feed(&mut hub, HubInput::GroupArrived(group(collection(0x1413, 2, 19))), 0);
    assert!(feed(&mut hub, HubInput::RefreshNow, 1).is_empty());
    assert!(feed(&mut hub, HubInput::Resume, 5_001).is_empty());
    assert!(hub.snapshot().is_empty());
    assert_eq!(hub.next_deadline(), None);

    assert_eq!(report(&mut hub, &NORDIC, 10_000), [HubOutput::StateChanged]);
    let online = hub.snapshot();
    assert_eq!(online[0].presence, Presence::Online);
    assert_eq!(hub.next_deadline(), Some(70_000));
    assert!(feed(&mut hub, HubInput::RefreshNow, 10_001).is_empty());
    assert!(feed(&mut hub, HubInput::Tick, 15_001).is_empty());
    assert!(
        feed(&mut hub, HubInput::Activity(online[0].key.clone()), 69_998).is_empty()
    );
    assert!(feed(&mut hub, HubInput::Tick, 69_999).is_empty());
    assert_eq!(hub.snapshot(), online);
    assert_eq!(hub.next_deadline(), Some(70_000));

    assert_eq!(
        feed(&mut hub, HubInput::Tick, 70_000),
        [HubOutput::StateChanged]
    );
    let asleep = hub.snapshot();
    assert_eq!(asleep[0].presence, Presence::Asleep);
    assert_eq!(asleep[0].level, Some(Level::Percent(65)));
    assert_eq!(asleep[0].updated_ms, 10_000);
    assert_eq!(hub.next_deadline(), None);
    assert!(feed(&mut hub, HubInput::RefreshNow, 70_001).is_empty());
    assert!(feed(&mut hub, HubInput::Resume, 75_001).is_empty());
    assert!(feed(&mut hub, HubInput::Tick, 75_001).is_empty());
    assert_eq!(hub.snapshot(), asleep);

    let mut wake = NORDIC;
    wake[8] = 0;
    assert_eq!(report(&mut hub, &wake, 75_002), [HubOutput::StateChanged]);
    let fresh = hub.snapshot();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].presence, Presence::Online);
    assert_eq!(fresh[0].level, Some(Level::Percent(0)));
    assert_eq!(fresh[0].updated_ms, 75_002);
    assert_eq!(hub.next_deadline(), Some(135_002));
}

fn steelseries_group(pid: u16, interface: u8, width: u16) -> HidGroup {
    let command = Collection {
        id: RX,
        vid: 0x1038,
        pid,
        interface: Some(interface),
        usage_page: 0xFFC0,
        usage: 1,
        input_len: width,
        output_len: width,
        feature_len: 0,
        product: String::new(),
    };
    let event = Collection {
        id: OTHER,
        interface: Some(if width == 65 { 5 } else { interface }),
        usage_page: 0xFF00,
        output_len: 0,
        ..command.clone()
    };
    HidGroup {
        id: GroupId(1),
        collections: vec![command, event],
    }
}

fn steelseries_input(
    hub: &mut Hub,
    collection: CollectionId,
    bytes: &[u8],
    now: Ms,
) -> Vec<HubOutput> {
    hub.handle(
        HubInput::Report {
            collection,
            origin: ReportOrigin::Input,
            bytes: bytes.to_vec(),
        },
        now,
    )
}

fn steelseries_sent(out: Vec<HubOutput>, expected: Option<(u8, usize)>) {
    let sends: Vec<_> = out
        .into_iter()
        .filter(|output| *output != HubOutput::StateChanged)
        .collect();
    let wanted: Vec<_> = expected
        .into_iter()
        .map(|(id, width)| {
            let mut bytes = vec![0; width];
            bytes[..2].copy_from_slice(&[id, 0xB0]);
            HubOutput::Send {
                collection: RX,
                kind: ReportKind::Output,
                bytes,
                deadline: None,
            }
        })
        .collect();
    assert_eq!(sends, wanted);
}

fn steelseries_state(hub: &Hub, presence: Presence, level: Option<u8>, charge: Charge) {
    let states = hub.snapshot();
    assert_eq!(states.len(), 1, "only the headset, never a spare or case");
    let state = &states[0];
    assert_eq!(state.kind, DeviceKind::Headset);
    assert_eq!(state.via, Via::Receiver);
    assert_eq!(state.presence, presence);
    assert_eq!(state.level, level.map(Level::Percent));
    assert_eq!(state.charge, charge);
    assert_eq!(state.verification, Verification::SourceVerified);
    assert!(state.reports_changes);
    assert_eq!(state.poll_min_ms, None);
}

#[test]
fn steelseries_nova_status_uses_exact_pid_scale_and_windows_report_id() {
    // Linux hid-steelseries-arctis.c's separate Nova 7 scales and GG capture
    // in Linux-Arctis-Manager nova_7_gen2.yaml; Nova 5X offset: CachyOS #628.
    for (pid, frame, level, charge) in [
        (0x2202, [0, 0xB0, 3, 3, 1, 0], 75, Charge::Charging),
        (0x22A1, [0, 0xB0, 3, 3, 3, 0], 3, Charge::Discharging),
        (0x2253, [0, 0xB0, 3, 3, 97, 3], 97, Charge::Discharging),
    ] {
        let mut hub = hub();
        let group = steelseries_group(pid, 3, 65);
        let selected = hub
            .select_group(&group, |_, _| panic!("no metadata gate"))
            .expect("a supported PID has an exact selection");
        assert_eq!(selected.command_report_id(), Some(0));
        assert!(selected.reads(RX) && selected.sends(RX));
        assert!(selected.reads(OTHER) && !selected.sends(OTHER));
        steelseries_sent(hub.handle(HubInput::GroupArrived(group), 0), None);
        assert!(steelseries_input(&mut hub, RX, &frame[1..], 1).is_empty());
        assert!(steelseries_input(&mut hub, OTHER, &frame, 2).is_empty());
        steelseries_sent(steelseries_input(&mut hub, RX, &frame, 3), None);
        steelseries_state(&hub, Presence::Online, Some(level), charge);
        assert_eq!(hub.snapshot()[0].source, Source::Push);
        assert!(hub.snapshot()[0].name.starts_with("SteelSeries "));
    }
}

#[test]
fn steelseries_dongle_only_sends_once_per_proven_link_edge_and_one_shot() {
    let mut hub = hub();
    let request = Some((0, 65));
    steelseries_sent(
        hub.handle(HubInput::GroupArrived(steelseries_group(0x2232, 3, 65)), 0),
        None,
    );
    let due = hub.next_deadline().expect("initial settle");
    assert!((1_000..=2_000).contains(&due));
    steelseries_sent(hub.handle(HubInput::RefreshNow, 2), None);
    steelseries_sent(hub.handle(HubInput::Resume, 3), None);
    steelseries_sent(hub.handle(HubInput::Tick, due - 1), None);
    steelseries_sent(hub.handle(HubInput::Tick, due), request);
    // The first up notification, even after settling, is not a down→up edge.
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 3, 2], due + 1),
        None,
    );
    steelseries_sent(
        steelseries_input(&mut hub, RX, &[0, 0xB0, 3, 0, 80, 3], due + 2),
        None,
    );
    assert_eq!(hub.snapshot()[0].source, Source::Query);
    assert_eq!(hub.next_deadline(), None);

    // Arctis-Sound-Manager #348, a Nova 5; trailing 02 is not the state byte.
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB7, 79, 2], 2_100),
        None,
    );
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xBB, 1, 2], 2_101),
        None,
    );
    steelseries_state(&hub, Presence::Online, Some(79), Charge::Charging);
    let key = hub.snapshot()[0].key.clone();
    hub.handle(HubInput::SetIntervals([(key.clone(), 1)].into()), 2_102);
    steelseries_sent(hub.handle(HubInput::Tick, 900_000), None);
    steelseries_sent(hub.handle(HubInput::Activity(key), 900_001), None);
    assert_eq!(hub.next_deadline(), None);

    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 3, 2], 900_002),
        None,
    );
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 2, 2], 900_003),
        None,
    );
    steelseries_state(&hub, Presence::Offline, Some(79), Charge::Unknown);
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB7, 0, 2], 900_004),
        None,
    );
    steelseries_state(&hub, Presence::Offline, Some(79), Charge::Unknown);
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 3, 2], 900_005),
        request,
    );
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 3, 2], 900_006),
        None,
    );
    steelseries_sent(hub.handle(HubInput::Tick, 910_000), None);
    steelseries_sent(hub.handle(HubInput::Resume, 910_001), request);
    steelseries_sent(hub.handle(HubInput::RefreshNow, 910_002), request);
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 2, 2], 910_003),
        None,
    );
    // The dongle can answer an explicit read even when its radio link is down.
    steelseries_sent(hub.handle(HubInput::RefreshNow, 910_004), request);
    steelseries_sent(
        steelseries_input(&mut hub, RX, &[0, 0xB0, 3, 0, 65, 2], 910_005),
        None,
    );
    steelseries_state(&hub, Presence::Online, Some(65), Charge::NotCharging);
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB9, 3, 2], 910_006),
        None,
    );
}

#[test]
fn steelseries_gamebuds_link_uses_b5_and_only_active_buds() {
    // Linux-Arctis-Manager base_arctis_gamebuds_dongle.yaml (Windows fields),
    // HeadsetControl steelseries_arctis_gamebuds.hpp (lower active-bud level).
    let mut hub = hub();
    steelseries_sent(
        hub.handle(HubInput::GroupArrived(steelseries_group(0x230A, 3, 65)), 0),
        None,
    );
    steelseries_sent(hub.handle(HubInput::Tick, 2_000), Some((0, 65)));
    assert!(steelseries_input(&mut hub, RX, &[0, 0xB0, 3, 3, 2, 2, 0, 0], 2_001).is_empty());
    // On this PID B9 and BB are transparency and microphone mute, not link/charge.
    assert!(steelseries_input(&mut hub, OTHER, &[0, 0xB9, 3], 2_002).is_empty());
    assert!(steelseries_input(&mut hub, OTHER, &[0, 0xBB, 1], 2_003).is_empty());
    let request = Some((0, 65));
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB5, 0, 3, 3, 2], 2_004),
        request,
    );
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB5, 0, 3, 3, 3], 2_005),
        None,
    );
    steelseries_sent(
        steelseries_input(&mut hub, RX, &[0, 0xB0, 0, 0, 3, 3, 83, 32], 2_006),
        None,
    );
    steelseries_state(&hub, Presence::Online, Some(32), Charge::Unknown);
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB5, 3, 3, 3, 2], 2_007),
        None,
    );
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB7, 82, 0], 2_008),
        None,
    );
    steelseries_state(&hub, Presence::Online, Some(82), Charge::Unknown);
    steelseries_sent(
        steelseries_input(&mut hub, OTHER, &[0, 0xB5, 3, 3, 2, 2], 2_009),
        None,
    );
    steelseries_state(&hub, Presence::Offline, Some(82), Charge::Unknown);
}

#[test]
fn steelseries_stations_keep_numbered_framing_and_only_track_headset() {
    // arctis-omni-linux README Protocol; Linux-Arctis-Manager
    // base_arctis_nova_pro_wireless.yaml; HeadsetControl #481 (Elite push).
    for (pid, interface, rid, raw, level, push_raw, push_level) in [
        (0x12E5, 4, 0x06, 5, 62, 7, 87),
        (0x2244, 3, 0x01, 99, 99, 95, 95),
    ] {
        let mut hub = hub();
        let group = steelseries_group(pid, interface, 64);
        let selection = hub.select_group(&group, |_, _| None).expect("station role");
        assert_eq!(selection.command_report_id(), Some(rid));
        assert!(selection.reads(RX) && selection.sends(RX));
        assert!(selection.reads(OTHER) && !selection.sends(OTHER));
        steelseries_sent(hub.handle(HubInput::GroupArrived(group), 0), None);
        let due = hub.next_deadline().expect("station must settle");
        assert!(due >= 5_000);
        steelseries_sent(hub.handle(HubInput::Tick, due - 1), None);
        steelseries_sent(hub.handle(HubInput::Tick, due), Some((rid, 64)));
        let frame = [rid, 0xB0, 0, 0, 1, 0, raw, 0xFF, 8, 1, 2, 10, 3, 0, 8, 2];
        assert!(steelseries_input(&mut hub, OTHER, &frame, due + 1).is_empty());
        assert!(steelseries_input(&mut hub, RX, &[0x07, 0x19, 0, 0], due + 2).is_empty());
        steelseries_sent(steelseries_input(&mut hub, RX, &frame, due + 3), None);
        steelseries_state(&hub, Presence::Online, Some(level), Charge::Charging);
        assert_eq!(hub.snapshot()[0].source, Source::Query);
        steelseries_sent(
            steelseries_input(&mut hub, OTHER, &[0x07, 0xB7, push_raw, 3, 4], due + 4),
            None,
        );
        steelseries_state(&hub, Presence::Online, Some(push_level), Charge::Full);
        steelseries_sent(
            steelseries_input(&mut hub, RX, &[0x07, 0xB5, 0, 0, 4], due + 5),
            None,
        );
        steelseries_state(&hub, Presence::Offline, Some(push_level), Charge::Unknown);
        steelseries_sent(
            steelseries_input(&mut hub, OTHER, &[0x07, 0xB5, 0, 0, 8], due + 6),
            Some((rid, 64)),
        );
        steelseries_sent(
            steelseries_input(&mut hub, RX, &[0x07, 0xB5, 0, 0, 8], due + 7),
            None,
        );
        steelseries_sent(hub.handle(HubInput::Tick, 1_000_000), None);
        assert_eq!(hub.next_deadline(), None);
    }
}

#[test]
fn steelseries_rejects_malformed_or_foreign_frames_and_unmatched_roles() {
    let mut hub = hub();
    for (label, pid, interface, width) in [
        ("unknown PID", 0x22FF, 3, 65),
        ("station bootloader", 0x2245, 3, 64),
        ("wired GameDAC", 0x12CB, 4, 64),
        ("wrong interface", 0x2202, 4, 65),
        ("wrong width", 0x2202, 3, 64),
    ] {
        assert!(
            hub.select_group(&steelseries_group(pid, interface, width), |_, _| None)
                .is_none(),
            "{label}"
        );
    }
    let mut group = steelseries_group(0x2202, 3, 65);
    group.collections.push(Collection {
        id: CollectionId(3),
        ..group.collections[0].clone()
    });
    assert!(hub.select_group(&group, |_, _| None).is_none());
    group.collections[2] = Collection {
        id: CollectionId(3),
        ..group.collections[1].clone()
    };
    assert!(hub.select_group(&group, |_, _| None).is_none());

    steelseries_sent(
        hub.handle(HubInput::GroupArrived(steelseries_group(0x2202, 3, 65)), 0),
        None,
    );
    steelseries_sent(
        steelseries_input(&mut hub, RX, &[0, 0xB0, 3, 4, 3, 0], 1),
        None,
    );
    steelseries_state(&hub, Presence::Online, Some(100), Charge::Discharging);
    let original = hub.snapshot();
    for (collection, bytes) in [
        (RX, &[0, 0xB0, 3, 1, 3][..]), // Missing the fifth payload byte.
        (RX, &[0, 0xB0, 3, 5, 1, 0]),
        (RX, &[0, 0xB0, 9, 2, 1, 0]),
        (RX, &[0, 0xB0, 3, 2, 4, 0]),
        (RX, &[0, 0xB1, 3, 2, 1, 0]),
        (RX, &[1, 0xB0, 3, 2, 1, 0]),
        (RX, &[0, 0xB9, 2]), // Events are not accepted on regular Nova command paths.
        (OTHER, &[0, 0xB7, 5]),
        (OTHER, &[0, 0xB9]),
        (OTHER, &[0, 0xBB, 0xFF]),
        (OTHER, &[0, 0xB5, 0, 0, 2, 2]), // Bluetooth event, not the 2.4 GHz link.
    ] {
        assert!(
            steelseries_input(&mut hub, collection, bytes, 2).is_empty(),
            "{bytes:02x?}"
        );
    }
    assert!(
        hub.handle(
            HubInput::Report {
                collection: RX,
                origin: ReportOrigin::FeatureReply,
                bytes: vec![0, 0xB0, 3, 2, 1, 0],
            },
            2,
        )
        .is_empty()
    );
    assert_eq!(hub.snapshot(), original);

    for (pid, interface, rid, bad_level) in [(0x12E5, 4, 6, 9), (0x2290, 3, 1, 101)] {
        let mut hub = self::hub();
        hub.handle(
            HubInput::GroupArrived(steelseries_group(pid, interface, 64)),
            0,
        );
        let good = [rid, 0xB0, 0, 0, 1, 0, 4, 8, 8, 1, 2, 10, 3, 0, 8, 8];
        steelseries_sent(steelseries_input(&mut hub, RX, &good, 1), None);
        let original = hub.snapshot();
        assert_eq!(original.len(), 1);
        assert!(steelseries_input(&mut hub, RX, &good[..15], 2).is_empty());
        for (offset, byte) in [
            (0, 7),
            (1, 0xB1),
            (6, bad_level),
            (6, 0xFF),
            (14, 3),
            (15, 3),
        ] {
            let mut bad = good;
            bad[offset] = byte;
            assert!(
                steelseries_input(&mut hub, RX, &bad, 2).is_empty(),
                "{bad:02x?}"
            );
        }
        for bytes in [
            &[0x07, 0xB7, 4, 0][..],
            &[0x07, 0xB7, bad_level, 0, 4],
            &[0x07, 0xB5, 0, 0],
            &[0x07, 0xB5, 0, 0, 0xFF],
            &[0x01, 0xB7, 4, 0, 2],
        ] {
            assert!(
                steelseries_input(&mut hub, OTHER, bytes, 2).is_empty(),
                "{bytes:02x?}"
            );
        }
        assert_eq!(hub.snapshot(), original);
    }
}

#[test]
fn steelseries_7_plus_passively_reads_both_documented_event_paths() {
    // LAM arctis_7_plus.yaml kept if 3 from real hardware; GG declares if 5.
    let mut hub = hub();
    let group = steelseries_group(0x2212, 3, 65);
    steelseries_sent(hub.handle(HubInput::GroupArrived(group), 0), None);
    steelseries_sent(hub.handle(HubInput::Tick, 1_500), Some((0, 65)));
    let push = [0, 0xB7, 3];
    steelseries_sent(steelseries_input(&mut hub, RX, &push, 1_501), None);
    steelseries_state(&hub, Presence::Online, Some(75), Charge::Unknown);
    let push = [0, 0xB7, 2];
    steelseries_sent(steelseries_input(&mut hub, OTHER, &push, 1_502), None);
    steelseries_state(&hub, Presence::Online, Some(50), Charge::Unknown);
    steelseries_sent(hub.handle(HubInput::Tick, 999_999), None);
    assert_eq!(hub.next_deadline(), None);
}

#[test]
fn steelseries_an_expired_arrival_grant_is_not_retried() {
    let mut hub = hub();
    let group = steelseries_group(0x12E5, 4, 64);
    steelseries_sent(hub.handle(HubInput::GroupArrived(group), 0), None);
    // The synthetic loop missed the default 10-second discovery grant.
    steelseries_sent(hub.handle(HubInput::Tick, 10_001), None);
    assert_eq!(hub.next_deadline(), None);
    steelseries_sent(hub.handle(HubInput::Tick, 100_000), None);
    steelseries_sent(hub.handle(HubInput::RefreshNow, 100_001), Some((6, 64)));
}
