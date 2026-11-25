//! Xbox controllers on a USB cable or the Xbox Wireless Adapter, from local driver state:
//! Windows.Gaming.Input presence and `TryGetBatteryReport`, with XInput as a fallback.
//!
//! Over Bluetooth both APIs report placeholders (WGI a fixed lowest bucket, XInput a
//! disconnected battery), so Bluetooth pads are left to the cached Bluetooth property
//! (`system::bluetooth`). Nothing here writes to a controller: no rumble, no `XInputSetState`,
//! no GIP power or device-state messages, no adapter I/O and no GameInput.

use std::collections::BTreeSet;

use batterysight_core::hub::HubInput;
use batterysight_core::model::{
    Charge, CoarseLevel, DeviceKey, DeviceKind, DeviceState, DeviceUpdate, Level, Presence,
    Source as SampleSource, SystemObservation, Verification, Via,
};
use batterysight_core::time::Ms;
use windows::Foundation::{EventHandler, IReference};
use windows::Gaming::Input::RawGameController;
use windows::System::Power::BatteryStatus;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::UI::Input::XboxController::{
    BATTERY_DEVTYPE_GAMEPAD, BATTERY_LEVEL, BATTERY_LEVEL_EMPTY, BATTERY_LEVEL_FULL,
    BATTERY_LEVEL_LOW, BATTERY_LEVEL_MEDIUM, BATTERY_TYPE, BATTERY_TYPE_ALKALINE,
    BATTERY_TYPE_DISCONNECTED, BATTERY_TYPE_NIMH, BATTERY_TYPE_WIRED, XINPUT_BATTERY_INFORMATION,
    XINPUT_STATE, XInputGetBatteryInformation, XInputGetState,
};

use crate::hid_enum::HidInterface;
use crate::reactor::{OsEvent, Poster};
use crate::system::{HidBus, hid_bus};

pub(crate) const MICROSOFT: u16 = 0x045E;
/// Local re-read while any controller is present; WGI has no battery event.
const REREAD_MS: Ms = 60_000;
const WGI_PREFIX: &str = "pad:wgi:";
const XINPUT_PREFIX: &str = "pad:xinput:";

/// A `BatteryReport` as WGI returned it. For these pads the capacities are quantized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) remaining: Option<i32>,
    pub(crate) full: Option<i32>,
    pub(crate) status: BatteryStatus,
}

/// One WGI `RawGameController`, with an identifier local to this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pad {
    pub(crate) id: u32,
    pub(crate) vid: u16,
    pub(crate) pid: u16,
    /// Reached through the Xbox Wireless Adapter rather than a cable.
    pub(crate) wireless: bool,
    pub(crate) name: String,
    /// Read only for identified pads.
    pub(crate) report: Option<Report>,
}

/// An active XInput slot's gamepad battery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) battery_type: BATTERY_TYPE,
    pub(crate) level: BATTERY_LEVEL,
}

impl Slot {
    /// Only battery types carry a level; wired, unknown and disconnected do not.
    pub(crate) fn bucket(self) -> Option<CoarseLevel> {
        if !matches!(self.battery_type, BATTERY_TYPE_ALKALINE | BATTERY_TYPE_NIMH) {
            return None;
        }
        match self.level {
            BATTERY_LEVEL_EMPTY => Some(CoarseLevel::Critical),
            BATTERY_LEVEL_LOW => Some(CoarseLevel::Low),
            BATTERY_LEVEL_MEDIUM => Some(CoarseLevel::Good),
            BATTERY_LEVEL_FULL => Some(CoarseLevel::Full),
            _ => None,
        }
    }

    /// Bluetooth pads report a disconnected battery through XInput.
    fn usable(self) -> bool {
        self.battery_type != BATTERY_TYPE_DISCONNECTED
    }
}

/// One successful look at the controllers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Poll {
    pub(crate) pads: Vec<Pad>,
    /// VID/PID of present HID collections that are not on a USB HID path (Bluetooth and
    /// other buses). A WGI pad sharing one is not identified.
    pub(crate) non_usb: BTreeSet<(u16, u16)>,
}

/// The documented 100/70/40/10 % quantization; anything else is not trusted.
pub(crate) fn bucket(report: &Report) -> Option<CoarseLevel> {
    if report.status == BatteryStatus::NotPresent {
        return None;
    }
    let (remaining, full) = (i64::from(report.remaining?), i64::from(report.full?));
    if full <= 0 || remaining < 0 {
        return None;
    }
    [
        (100, CoarseLevel::Full),
        (70, CoarseLevel::Good),
        (40, CoarseLevel::Low),
        (10, CoarseLevel::Critical),
    ]
    .into_iter()
    .find(|&(percent, _)| remaining * 100 == full * percent)
    .map(|(_, level)| level)
}

fn charge(report: &Report) -> Option<Charge> {
    match report.status {
        BatteryStatus::Charging => Some(Charge::Charging),
        BatteryStatus::Discharging => Some(Charge::Discharging),
        BatteryStatus::Idle => Some(Charge::NotCharging),
        _ => None,
    }
}

/// A Microsoft pad whose product never appears on a non-USB HID path. Bluetooth pads are
/// HID devices, so this rules them out; one product on several transports rules out all.
fn identified(pad: &Pad, non_usb: &BTreeSet<(u16, u16)>) -> bool {
    pad.vid == MICROSOFT && !non_usb.contains(&(pad.vid, pad.pid))
}

/// Hides an XInput row in its removal grace once WGI rows replaced the XInput provider.
pub(crate) fn hide_replaced(states: &mut Vec<DeviceState>) {
    if states
        .iter()
        .any(|state| state.key.0.starts_with(WGI_PREFIX))
    {
        states.retain(|state| {
            !state.key.0.starts_with(XINPUT_PREFIX) || state.presence == Presence::Online
        });
    }
}

fn observation(
    key: String,
    name: String,
    via: Via,
    level: Option<CoarseLevel>,
    charge: Option<Charge>,
) -> HubInput {
    HubInput::System(SystemObservation {
        update: DeviceUpdate {
            key: DeviceKey(key),
            name: Some(name),
            kind: Some(DeviceKind::Controller),
            via: Some(via),
            level: level.map(Level::Coarse),
            millivolts: None,
            charge,
            presence: Some(Presence::Online),
            source: SampleSource::Os,
        },
        verification: Verification::SourceVerified,
        reports_changes: false,
    })
}

/// Runtime state of this source. It takes no HID grant and opens no device.
#[derive(Default)]
pub(crate) struct Source {
    active: BTreeSet<DeviceKey>,
    requested: bool,
    periodic: Option<Ms>,
    /// Set once WGI gave an identified pad a valid bucket; XInput is not consulted again.
    wgi_reports: bool,
    /// Controllers seen at the last poll and their local identifiers.
    controllers: Vec<(RawGameController, u32)>,
    next_id: u32,
}

impl Source {
    pub(crate) fn request(&mut self) {
        self.requested = true;
    }

    pub(crate) fn deadline(&self) -> Option<Ms> {
        self.requested
            .then_some(0)
            .into_iter()
            .chain(self.periodic)
            .min()
    }

    pub(crate) fn take_due(&mut self, now: Ms) -> bool {
        let mut due = std::mem::take(&mut self.requested);
        if self.periodic.is_some_and(|at| at <= now) {
            // Kept if the next poll fails.
            self.periodic = Some(now.saturating_add(REREAD_MS));
            due = true;
        }
        due
    }

    /// Lists the WGI controllers and reads batteries for identified pads only. `None` when
    /// WGI or the HID enumeration fails, which says nothing about removals.
    pub(crate) fn poll(
        &mut self,
        enumerate: impl FnOnce() -> Option<Vec<HidInterface>>,
    ) -> Option<Poll> {
        let list = RawGameController::RawGameControllers().ok()?;
        let mut controllers = Vec::new();
        let mut pads = Vec::new();
        for index in 0..list.Size().ok()? {
            let controller = list.GetAt(index).ok()?;
            let (Ok(vid), Ok(pid)) = (
                controller.HardwareVendorId(),
                controller.HardwareProductId(),
            ) else {
                return None;
            };
            let id = if let Some(&(_, id)) = self
                .controllers
                .iter()
                .find(|(known, _)| *known == controller)
            {
                id
            } else {
                self.next_id += 1;
                self.next_id
            };
            pads.push(Pad {
                id,
                vid,
                pid,
                wireless: false,
                name: String::new(),
                report: None,
            });
            controllers.push((controller, id));
        }
        self.controllers = controllers;
        let non_usb = if pads.iter().any(|pad| pad.vid == MICROSOFT) {
            enumerate()?
                .iter()
                .filter(|interface| hid_bus(&interface.path) != HidBus::Usb)
                .map(|interface| (interface.collection.vid, interface.collection.pid))
                .collect()
        } else {
            BTreeSet::new()
        };
        for (pad, (controller, _)) in pads.iter_mut().zip(&self.controllers) {
            if identified(pad, &non_usb) {
                pad.wireless = controller.IsWireless().unwrap_or(false);
                pad.name = controller
                    .DisplayName()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                pad.report = read_report(controller);
            }
        }
        Some(Poll { pads, non_usb })
    }

    /// Turns a poll into observations. `xinput` is called at most once, and only while WGI
    /// has not produced a bucket for any identified pad.
    pub(crate) fn reconcile(
        &mut self,
        poll: Option<Poll>,
        xinput: impl FnOnce() -> [Option<Slot>; 4],
        now: Ms,
    ) -> Vec<HubInput> {
        let Some(Poll { pads, non_usb }) = poll else {
            return Vec::new();
        };
        self.periodic = if pads.is_empty() {
            None
        } else {
            Some(self.periodic.unwrap_or(now.saturating_add(REREAD_MS)))
        };
        let pads: Vec<_> = pads
            .into_iter()
            .filter(|pad| identified(pad, &non_usb))
            .collect();
        self.wgi_reports |= pads
            .iter()
            .any(|pad| pad.report.as_ref().and_then(bucket).is_some());
        let mut inputs = Vec::new();
        let slots = (!self.wgi_reports && !pads.is_empty()).then(xinput);
        // Without identifiers, slots can only stand for the identified pads one-to-one.
        let usable: Vec<_> = slots
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, slot)| slot.filter(|slot| slot.usable()).map(|slot| (index, slot)))
            .collect();
        if !usable.is_empty() && usable.len() == pads.len() {
            for (index, slot) in usable {
                let via = if slot.battery_type == BATTERY_TYPE_WIRED {
                    Via::Usb
                } else {
                    Via::Receiver
                };
                inputs.push(observation(
                    format!("{XINPUT_PREFIX}{index}"),
                    "Xbox controller".into(),
                    via,
                    slot.bucket(),
                    None,
                ));
            }
        } else {
            for pad in pads {
                let name = if pad.name.trim().is_empty() {
                    "Xbox controller".into()
                } else {
                    pad.name
                };
                let via = if pad.wireless {
                    Via::Receiver
                } else {
                    Via::Usb
                };
                inputs.push(observation(
                    format!("{WGI_PREFIX}{}", pad.id),
                    name,
                    via,
                    pad.report.as_ref().and_then(bucket),
                    pad.report.as_ref().and_then(charge),
                ));
            }
        }
        let mut active = BTreeSet::new();
        for input in &inputs {
            if let HubInput::System(observation) = input {
                self.active.remove(&observation.update.key);
                active.insert(observation.update.key.clone());
            }
        }
        inputs.extend(
            std::mem::replace(&mut self.active, active)
                .into_iter()
                .map(HubInput::SystemGone),
        );
        inputs
    }
}

fn read_report(controller: &RawGameController) -> Option<Report> {
    let report = controller.TryGetBatteryReport().ok()?;
    let value = |reference: windows::core::Result<IReference<i32>>| {
        reference.ok().and_then(|reference| reference.Value().ok())
    };
    Some(Report {
        remaining: value(report.RemainingCapacityInMilliwattHours()),
        full: value(report.FullChargeCapacityInMilliwattHours()),
        status: report.Status().ok()?,
    })
}

/// XInput, status first: a slot whose `XInputGetState` fails is inactive and is never asked
/// for its battery. Only the gamepad battery is read; the adapter can echo pads as headsets.
pub(crate) fn read_xinput() -> [Option<Slot>; 4] {
    std::array::from_fn(|index| {
        let index = index as u32;
        let mut state = XINPUT_STATE::default();
        // SAFETY: `state` is writable for the call.
        if unsafe { XInputGetState(index, &mut state) } != ERROR_SUCCESS.0 {
            return None;
        }
        let mut battery = XINPUT_BATTERY_INFORMATION::default();
        // SAFETY: `battery` is writable for the call.
        let status =
            unsafe { XInputGetBatteryInformation(index, BATTERY_DEVTYPE_GAMEPAD, &mut battery) };
        (status == ERROR_SUCCESS.0).then_some(Slot {
            battery_type: battery.BatteryType,
            level: battery.BatteryLevel,
        })
    })
}

/// WGI arrival and removal callbacks; each only wakes the device thread.
pub(crate) struct Watch {
    added: i64,
    removed: i64,
}

impl Watch {
    pub(crate) fn start(poster: &Poster) -> windows::core::Result<Self> {
        let handler = |poster: Poster| {
            EventHandler::<RawGameController>::new(move |_, _| {
                poster.post(OsEvent::GameControllers);
                Ok(())
            })
        };
        let added = RawGameController::RawGameControllerAdded(&handler(poster.clone()))?;
        let removed = RawGameController::RawGameControllerRemoved(&handler(poster.clone()))
            .inspect_err(|_| {
                let _ = RawGameController::RemoveRawGameControllerAdded(added);
            })?;
        Ok(Self { added, removed })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = RawGameController::RemoveRawGameControllerAdded(self.added);
        let _ = RawGameController::RemoveRawGameControllerRemoved(self.removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::catalog::Catalog;
    use batterysight_core::hub::Hub;
    use batterysight_core::model::{CoarseLevel, DeviceState, Level, Presence};
    use batterysight_core::scheduler::Policy;
    use windows::System::Power::BatteryStatus;
    use windows::Win32::UI::Input::XboxController::{
        BATTERY_LEVEL, BATTERY_LEVEL_EMPTY, BATTERY_LEVEL_FULL, BATTERY_LEVEL_LOW,
        BATTERY_LEVEL_MEDIUM, BATTERY_TYPE, BATTERY_TYPE_ALKALINE, BATTERY_TYPE_DISCONNECTED,
        BATTERY_TYPE_NIMH, BATTERY_TYPE_UNKNOWN, BATTERY_TYPE_WIRED,
    };

    // Public product IDs of one pad model: USB cable and Bluetooth LE.
    const USB: u16 = 0x0B12;
    const BLE: u16 = 0x0B13;

    fn report(remaining: i32, full: i32) -> Report {
        Report {
            remaining: Some(remaining),
            full: Some(full),
            status: BatteryStatus::Discharging,
        }
    }

    fn pad(id: u32, pid: u16, report: Option<Report>) -> Pad {
        Pad {
            id,
            vid: MICROSOFT,
            pid,
            wireless: true,
            name: "Example Controller".into(),
            report,
        }
    }

    fn poll(pads: Vec<Pad>, non_usb: &[u16]) -> Option<Poll> {
        Some(Poll {
            pads,
            non_usb: non_usb.iter().map(|&pid| (MICROSOFT, pid)).collect(),
        })
    }

    fn slot(battery_type: BATTERY_TYPE, level: BATTERY_LEVEL) -> Option<Slot> {
        Some(Slot {
            battery_type,
            level,
        })
    }

    fn no_xinput() -> [Option<Slot>; 4] {
        unreachable!("XInput must not be consulted")
    }

    /// Applies one poll and returns the visible rows as the runtime would publish them.
    fn apply(
        source: &mut Source,
        hub: &mut Hub,
        poll: Option<Poll>,
        slots: impl FnOnce() -> [Option<Slot>; 4],
        now: Ms,
    ) -> Vec<DeviceState> {
        for input in source.reconcile(poll, slots, now) {
            hub.handle(input, now);
        }
        let mut states = hub.snapshot();
        hide_replaced(&mut states);
        states
    }

    fn rows(states: &[DeviceState]) -> Vec<(&str, Option<Level>)> {
        states
            .iter()
            .map(|state| (state.key.0.as_str(), state.level))
            .collect()
    }

    fn hub() -> Hub {
        Hub::new(Catalog::builtin(), Policy::default())
    }

    #[test]
    fn wgi_capacity_ratios_map_to_coarse_buckets() {
        for ((remaining, full), level) in [
            ((100, 100), CoarseLevel::Full),
            ((70, 100), CoarseLevel::Good),
            ((40, 100), CoarseLevel::Low),
            ((10, 100), CoarseLevel::Critical),
            ((700, 1000), CoarseLevel::Good),
        ] {
            assert_eq!(bucket(&report(remaining, full)), Some(level));
        }
    }

    #[test]
    fn missing_or_unreliable_wgi_reports_are_unknown() {
        let partial = |remaining, full| Report {
            remaining,
            full,
            status: BatteryStatus::Discharging,
        };
        for report in [
            partial(None, Some(100)),
            partial(Some(70), None),
            partial(Some(70), Some(0)),
            partial(Some(55), Some(100)),
            partial(Some(120), Some(100)),
            partial(Some(-10), Some(100)),
            Report {
                status: BatteryStatus::NotPresent,
                ..report(100, 100)
            },
        ] {
            assert_eq!(bucket(&report), None);
        }
        let mut source = Source::default();
        let mut hub = hub();
        let absent = Report {
            status: BatteryStatus::NotPresent,
            ..report(100, 100)
        };
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None), pad(2, 0x02EA, Some(absent))], &[]),
            || [None; 4],
            0,
        );
        assert_eq!(rows(&states), [("pad:wgi:1", None), ("pad:wgi:2", None)]);
        assert!(
            states
                .iter()
                .all(|state| state.presence == Presence::Online)
        );
    }

    #[test]
    fn xinput_battery_levels_use_the_same_buckets() {
        for battery_type in [BATTERY_TYPE_NIMH, BATTERY_TYPE_ALKALINE] {
            for (level, coarse) in [
                (BATTERY_LEVEL_EMPTY, CoarseLevel::Critical),
                (BATTERY_LEVEL_LOW, CoarseLevel::Low),
                (BATTERY_LEVEL_MEDIUM, CoarseLevel::Good),
                (BATTERY_LEVEL_FULL, CoarseLevel::Full),
            ] {
                let slot = Slot {
                    battery_type,
                    level,
                };
                assert_eq!(slot.bucket(), Some(coarse));
            }
        }
        for battery_type in [
            BATTERY_TYPE_WIRED,
            BATTERY_TYPE_UNKNOWN,
            BATTERY_TYPE_DISCONNECTED,
        ] {
            let slot = Slot {
                battery_type,
                level: BATTERY_LEVEL_FULL,
            };
            assert_eq!(slot.bucket(), None);
        }
    }

    #[test]
    fn inactive_xinput_slots_are_never_emitted() {
        let mut source = Source::default();
        let mut hub = hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            || {
                [
                    None,
                    slot(BATTERY_TYPE_NIMH, BATTERY_LEVEL_MEDIUM),
                    None,
                    None,
                ]
            },
            0,
        );
        assert_eq!(
            rows(&states),
            [("pad:xinput:1", Some(Level::Coarse(CoarseLevel::Good)))]
        );
        // With no active slot at all the pad stays visible, but without a guessed level.
        let mut source = Source::default();
        let mut hub = self::hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            || [None; 4],
            0,
        );
        assert_eq!(rows(&states), [("pad:wgi:1", None)]);
    }

    #[test]
    fn active_bluetooth_pad_keeps_only_its_property_entry() {
        let mut source = Source::default();
        let mut hub = hub();
        // WGI's placeholder bucket for a Bluetooth pad is never shown, and with no
        // identified USB or adapter pad XInput is not consulted.
        let fake = Some(report(10, 100));
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, BLE, fake)], &[BLE]),
            no_xinput,
            0,
        );
        assert!(states.is_empty());
        // Even after WGI proved itself on a USB pad, the Bluetooth pad stays omitted.
        let states = apply(
            &mut source,
            &mut hub,
            poll(
                vec![pad(1, BLE, fake), pad(2, USB, Some(report(100, 100)))],
                &[BLE],
            ),
            no_xinput,
            10,
        );
        assert_eq!(
            rows(&states),
            [("pad:wgi:2", Some(Level::Coarse(CoarseLevel::Full)))]
        );
    }

    #[test]
    fn a_usb_pad_is_never_emitted_by_both_providers() {
        let mut source = Source::default();
        let mut hub = hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            || {
                [
                    slot(BATTERY_TYPE_NIMH, BATTERY_LEVEL_FULL),
                    None,
                    None,
                    None,
                ]
            },
            0,
        );
        assert_eq!(
            rows(&states),
            [("pad:xinput:0", Some(Level::Coarse(CoarseLevel::Full)))]
        );
        // Once WGI reports, it is the only provider; the XInput row does not linger.
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, Some(report(70, 100)))], &[]),
            no_xinput,
            60_000,
        );
        assert_eq!(
            rows(&states),
            [("pad:wgi:1", Some(Level::Coarse(CoarseLevel::Good)))]
        );
        // A later null report stays on WGI instead of switching back.
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            no_xinput,
            120_000,
        );
        assert_eq!(
            rows(&states),
            [("pad:wgi:1", Some(Level::Coarse(CoarseLevel::Good)))]
        );
    }

    #[test]
    fn simultaneous_usb_and_bluetooth_pads_are_not_associated() {
        // WGI: only the USB pad; a non-Microsoft controller is never identified.
        let mut source = Source::default();
        let mut hub = hub();
        let sony = Pad {
            vid: 0x054C,
            ..pad(3, 0x0CE6, Some(report(100, 100)))
        };
        let states = apply(
            &mut source,
            &mut hub,
            poll(
                vec![
                    pad(1, USB, Some(report(40, 100))),
                    pad(2, BLE, Some(report(10, 100))),
                    sony,
                ],
                &[BLE],
            ),
            no_xinput,
            0,
        );
        assert_eq!(
            rows(&states),
            [("pad:wgi:1", Some(Level::Coarse(CoarseLevel::Low)))]
        );
        // XInput: the Bluetooth pad's disconnected slot is not counted or shown.
        let mut source = Source::default();
        let mut hub = self::hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None), pad(2, BLE, None)], &[BLE]),
            || {
                [
                    slot(BATTERY_TYPE_DISCONNECTED, BATTERY_LEVEL_EMPTY),
                    slot(BATTERY_TYPE_NIMH, BATTERY_LEVEL_LOW),
                    None,
                    None,
                ]
            },
            0,
        );
        assert_eq!(
            rows(&states),
            [("pad:xinput:1", Some(Level::Coarse(CoarseLevel::Low)))]
        );
        // One product on Bluetooth and another transport: neither can be identified.
        let mut source = Source::default();
        let mut hub = self::hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, BLE, None), pad(2, BLE, None)], &[BLE]),
            no_xinput,
            0,
        );
        assert!(states.is_empty());
        // More usable slots than identified pads: no slot can be attributed.
        let mut source = Source::default();
        let mut hub = self::hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            || {
                [
                    slot(BATTERY_TYPE_NIMH, BATTERY_LEVEL_LOW),
                    slot(BATTERY_TYPE_ALKALINE, BATTERY_LEVEL_FULL),
                    None,
                    None,
                ]
            },
            0,
        );
        assert_eq!(rows(&states), [("pad:wgi:1", None)]);
    }

    #[test]
    fn polls_while_present_and_errors_are_not_removals() {
        let mut source = Source::default();
        let mut hub = hub();
        assert_eq!(source.deadline(), None);
        source.request();
        assert_eq!(source.deadline(), Some(0));
        assert!(source.take_due(5));
        let present = || poll(vec![pad(1, USB, Some(report(100, 100)))], &[]);
        apply(&mut source, &mut hub, present(), no_xinput, 5);
        assert_eq!(source.deadline(), Some(60_005));
        assert!(!source.take_due(60_004));
        assert!(source.take_due(60_005));
        // A failed poll keeps the row and the next periodic poll.
        assert!(source.reconcile(None, no_xinput, 60_005).is_empty());
        assert_eq!(source.deadline(), Some(120_005));
        let states = apply(&mut source, &mut hub, poll(vec![], &[]), no_xinput, 70_000);
        assert_eq!(states[0].presence, Presence::Offline);
        assert_eq!(source.deadline(), None);
    }

    #[test]
    fn only_usb_hid_paths_identify_a_pad_transport() {
        use crate::system::{HidBus, hid_bus};
        let wide = |path: &str| path.encode_utf16().chain([0]).collect::<Vec<_>>();
        for (path, bus) in [
            (
                r"\\?\HID#VID_045E&PID_0B12&IG_00#1&2&0&0000#{4d1e55b2}",
                HidBus::Usb,
            ),
            (
                r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0002045e_pid&02e0#1&2&0&0000#{4d1e55b2}",
                HidBus::Bluetooth,
            ),
            (
                r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&02045e_PID&0b13#1&2&0&0000#{4d1e55b2}",
                HidBus::Bluetooth,
            ),
            (
                r"\\?\HID#ConvertedDevice&Col01#1&2&0&0000#{4d1e55b2}",
                HidBus::Other,
            ),
            (r"\\.\pipe\example", HidBus::Other),
        ] {
            assert_eq!(hid_bus(&wide(path)), bus, "{path}");
        }
    }
}
