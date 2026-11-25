//! Xbox controllers on a USB cable or the Xbox Wireless Adapter, from local driver state:
//! Windows.Gaming.Input presence and `TryGetBatteryReport`, with XInput as a fallback.
//!
//! Over Bluetooth both APIs report placeholders (WGI a fixed lowest bucket, XInput a
//! disconnected battery), so Bluetooth pads are left to the cached Bluetooth property
//! (`system::bluetooth`). Nothing here writes to a controller: no rumble, no `XInputSetState`,
//! no GIP power or device-state messages, no adapter I/O and no GameInput.

use std::collections::{BTreeMap, BTreeSet};

use batterysight_core::hub::{HubInput, REMOVAL_GRACE_MS};
use batterysight_core::model::{
    Charge, CoarseLevel, DeviceKey, DeviceKind, DeviceState, DeviceUpdate, Level, Presence,
    Source as SampleSource, SystemObservation, Verification, Via,
};
use batterysight_core::time::Ms;
use windows::Foundation::{EventHandler, IReference};
use windows::Gaming::Input::{Gamepad, IGameController, RawGameController};
use windows::System::Power::BatteryStatus;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CONFIGRET, CR_NO_SUCH_VALUE, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_CompatibleIds, DEVPROP_TYPE_STRING_LIST, DEVPROPTYPE,
};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::UI::Input::XboxController::{
    BATTERY_DEVTYPE_GAMEPAD, BATTERY_LEVEL, BATTERY_LEVEL_EMPTY, BATTERY_LEVEL_FULL,
    BATTERY_LEVEL_LOW, BATTERY_LEVEL_MEDIUM, BATTERY_TYPE, BATTERY_TYPE_ALKALINE,
    BATTERY_TYPE_DISCONNECTED, BATTERY_TYPE_NIMH, BATTERY_TYPE_WIRED, XINPUT_BATTERY_INFORMATION,
    XINPUT_STATE, XInputGetBatteryInformation, XInputGetState,
};
use windows::core::Interface;

use crate::hid_enum::interface_node;
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
    /// WGI also exposes it as a `Gamepad` (a local cast); wheels and joysticks are not.
    pub(crate) gamepad: bool,
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

/// What the HID interface paths say about pad transports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HidProducts {
    /// VID/PID of present HID collections that are not on a USB HID path (Bluetooth and
    /// other buses). A WGI pad sharing one is not identified.
    pub(crate) non_usb: BTreeSet<(u16, u16)>,
    /// A Bluetooth path, or one whose hardware segment mentions Microsoft's VID, could not
    /// be parsed: no Microsoft pad is identified.
    pub(crate) microsoft_unknown: bool,
}

/// One successful look at the controllers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Poll {
    pub(crate) pads: Vec<Pad>,
    pub(crate) hid: HidProducts,
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

/// A Microsoft gamepad whose product never appears on a non-USB HID path. Bluetooth pads
/// are HID devices, so this rules them out; one product on several transports rules out all.
fn identified(pad: &Pad, hid: &HidProducts) -> bool {
    pad.vid == MICROSOFT && !hid.microsoft_unknown && counted(pad, hid)
}

/// A gamepad of any vendor that can hold an XInput slot other than a Bluetooth one.
fn counted(pad: &Pad, hid: &HidProducts) -> bool {
    pad.gamepad && !hid.non_usb.contains(&(pad.vid, pad.pid))
}

/// The hex digits after `tag` (`vid` or `pid`, then `_` or `&`) in a lowercase path segment.
fn hex_after<'a>(segment: &'a str, tag: &str) -> Option<&'a str> {
    let start = segment
        .match_indices(tag)
        .map(|(at, _)| at + tag.len())
        .find(|&at| matches!(segment.as_bytes().get(at), Some(b'_' | b'&')))?
        + 1;
    let rest = &segment[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Microsoft's Bluetooth SIG company identifier, used with VID source `0001`/`01`.
const MICROSOFT_SIG: u16 = 0x0006;

/// What a HID path's hardware segment says about its vendor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathVendor {
    /// A USB-IF VID and PID: `vid_045e&pid_0b12` (USB), `vid&0002045e_pid&02e0`
    /// (Classic) or `vid&02045e_pid&0b13` (LE).
    Product(u16, u16),
    /// A Bluetooth SIG company identifier (source `0001`/`01`).
    SigCompany(u16),
    /// No VID token, a zero or unknown source, or no readable PID.
    Unknown,
}

fn path_vendor(hardware: &str) -> PathVendor {
    let Some(vid) = hex_after(hardware, "vid") else {
        return PathVendor::Unknown;
    };
    let (source, id) = match vid.len() {
        4 => ("0002", vid),
        6 => vid.split_at(2),
        8 => vid.split_at(4),
        _ => return PathVendor::Unknown,
    };
    let Ok(id) = u16::from_str_radix(id, 16) else {
        return PathVendor::Unknown;
    };
    let pid = hex_after(hardware, "pid")
        .filter(|pid| pid.len() == 4)
        .and_then(|pid| u16::from_str_radix(pid, 16).ok());
    match (source, pid) {
        ("0002" | "02", Some(pid)) => PathVendor::Product(id, pid),
        ("0001" | "01", _) => PathVendor::SigCompany(id),
        _ => PathVendor::Unknown,
    }
}

/// What the Configuration Manager list text says; nothing is opened. Instance segments
/// are never inspected. No Microsoft pad is identified while a present path carries
/// Microsoft's SIG company ID, or cannot be parsed and either mentions Microsoft's VID in
/// its hardware segment or, per `game_class`, is a game controller by its cached IDs or
/// cannot be checked.
pub(crate) fn non_usb_products(list: &[u16], game_class: impl Fn(&[u16]) -> bool) -> HidProducts {
    let mut products = HidProducts::default();
    for path in list
        .split(|&unit| unit == 0)
        .filter(|path| !path.is_empty())
    {
        let text = String::from_utf16_lossy(path).to_ascii_lowercase();
        let hardware = text.split('#').nth(1).unwrap_or_default();
        match (hid_bus(path), path_vendor(hardware)) {
            (HidBus::Usb, PathVendor::Product(..)) => {}
            (_, PathVendor::Product(vid, pid)) => {
                products.non_usb.insert((vid, pid));
            }
            (_, PathVendor::SigCompany(company)) => {
                products.microsoft_unknown |= company == MICROSOFT_SIG;
            }
            (_, PathVendor::Unknown) => {
                products.microsoft_unknown |= hardware.contains("045e") || game_class(path);
            }
        }
    }
    products
}

/// Whether the devnode behind `path` lists the `HID_DEVICE_SYSTEM_GAME` compatible ID,
/// read from Configuration Manager's cached properties without opening the device.
fn game_class(path: &[u16]) -> bool {
    let path: Vec<u16> = path.iter().copied().chain([0]).collect();
    game_class_from(interface_node(&path), |node, kind, ids, size| {
        // SAFETY: `ids` is writable for its whole length, which `size` never exceeds.
        unsafe {
            CM_Get_DevNode_PropertyW(
                node,
                &DEVPKEY_Device_CompatibleIds,
                kind,
                Some(ids.as_mut_ptr().cast()),
                size,
                0,
            )
        }
    })
}

/// Maps the devnode lookup and the compatible-ID read to the game-class answer. Only a
/// clean read says "not a game controller": a string list without the ID, or no
/// compatible IDs at all. Anything else cannot tell and fails closed.
fn game_class_from(
    node: Option<u32>,
    read: impl FnOnce(u32, &mut DEVPROPTYPE, &mut [u16], &mut u32) -> CONFIGRET,
) -> bool {
    let Some(node) = node else {
        return true;
    };
    let mut ids = [0u16; 2048];
    let mut kind = DEVPROPTYPE::default();
    // The last unit stays NUL whatever is written, so the list stays terminated.
    let mut size = (size_of_val(&ids) - size_of::<u16>()) as u32;
    match read(node, &mut kind, &mut ids, &mut size) {
        CR_NO_SUCH_VALUE => false,
        CR_SUCCESS if kind == DEVPROP_TYPE_STRING_LIST => ids
            .split(|&unit| unit == 0)
            .any(|id| String::from_utf16_lossy(id).eq_ignore_ascii_case("HID_DEVICE_SYSTEM_GAME")),
        _ => true,
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
        // Windows holds the pad's own pushed status; reading it sends nothing to the pad.
        reports_changes: true,
    })
}

/// Runtime state of this source. It takes no HID grant and opens no device.
#[derive(Default)]
pub(crate) struct Source {
    /// Rows sent and still online, with the name and transport they were sent with.
    shown: BTreeMap<DeviceKey, (String, Via)>,
    /// Rows sent `SystemGone` and not yet paired with a successor, while in grace.
    departed: Vec<Departed>,
    /// Rows in grace that a successor row stands for, one-to-one, with their departure.
    replaced: BTreeMap<DeviceKey, Ms>,
    requested: bool,
    periodic: Option<Ms>,
    /// Set once WGI gave an identified pad a valid bucket; XInput is not consulted again.
    wgi_reports: bool,
    /// Controllers seen at the last poll and their local identifiers.
    controllers: Vec<(RawGameController, u32)>,
    next_id: u32,
    /// HID path facts from the last successful interface list; `None` until listed again
    /// after a controller or HID change, Refresh or resume, or after a failed list.
    products: Option<HidProducts>,
}

/// A row sent `SystemGone`, kept for one-to-one pairing during its grace.
struct Departed {
    key: DeviceKey,
    name: String,
    via: Via,
    at: Ms,
}

impl Source {
    pub(crate) fn request(&mut self) {
        self.requested = true;
    }

    /// A controller or HID interface came or went, or Refresh or resume: list the HID
    /// interface paths again at the next poll, which is due now.
    pub(crate) fn hid_changed(&mut self) {
        self.products = None;
        self.requested = true;
    }

    /// The cached HID path facts, listing them first if needed. `None` only while the
    /// Configuration Manager list fails; the next poll then lists again.
    pub(crate) fn products(
        &mut self,
        list: impl FnOnce() -> Option<Vec<u16>>,
    ) -> Option<HidProducts> {
        if self.products.is_none() {
            self.products = list().map(|list| non_usb_products(&list, game_class));
        }
        self.products.clone()
    }

    /// Hides rows in their removal grace that a successor row stands for: a replugged WGI
    /// pad's earlier key, or a row of the provider that was replaced, each paired one-to-one.
    pub(crate) fn hide_replaced(&self, states: &mut Vec<DeviceState>) {
        states.retain(|state| {
            state.presence == Presence::Online || !self.replaced.contains_key(&state.key)
        });
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
    /// WGI or the HID interface list fails, which says nothing about removals.
    pub(crate) fn poll(&mut self, interfaces: impl FnOnce() -> Option<Vec<u16>>) -> Option<Poll> {
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
                // A local cast; it sends nothing to the controller.
                gamepad: controller
                    .cast::<IGameController>()
                    .and_then(|controller| Gamepad::FromGameController(&controller))
                    .is_ok(),
                name: String::new(),
                report: None,
            });
            controllers.push((controller, id));
        }
        self.controllers = controllers;
        let hid = if pads.is_empty() {
            HidProducts::default()
        } else {
            self.products(interfaces)?
        };
        for (pad, (controller, _)) in pads.iter_mut().zip(&self.controllers) {
            if identified(pad, &hid) {
                pad.wireless = controller.IsWireless().unwrap_or(false);
                pad.name = controller
                    .DisplayName()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default();
                pad.report = read_report(controller);
            }
        }
        Some(Poll { pads, hid })
    }

    /// Turns a poll into observations. `xinput` is called at most once, and only while WGI
    /// has not produced a bucket for any identified pad.
    pub(crate) fn reconcile(
        &mut self,
        poll: Option<Poll>,
        xinput: impl FnOnce() -> [Option<Slot>; 4],
        now: Ms,
    ) -> Vec<HubInput> {
        let Some(Poll { pads, hid }) = poll else {
            return Vec::new();
        };
        self.periodic = if pads.is_empty() {
            None
        } else {
            Some(self.periodic.unwrap_or(now.saturating_add(REREAD_MS)))
        };
        // Without identifiers, slots can only stand for the identified pads one-to-one, and
        // only if every gamepad that could hold a slot is one of them.
        let slot_holders = pads.iter().filter(|pad| counted(pad, &hid)).count();
        let pads: Vec<_> = pads
            .into_iter()
            .filter(|pad| identified(pad, &hid))
            .collect();
        self.wgi_reports |= pads
            .iter()
            .any(|pad| pad.report.as_ref().and_then(bucket).is_some());
        let mut inputs = Vec::new();
        let slots =
            (!self.wgi_reports && !pads.is_empty() && slot_holders == pads.len()).then(xinput);
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
        self.pair(&mut inputs, now);
        inputs
    }

    /// Sends `SystemGone` for rows that went away and pairs each new row with at most one
    /// departed row it stands for: a replugged WGI pad of the same name and transport, or
    /// a row of the other provider that departed in this same poll.
    fn pair(&mut self, inputs: &mut Vec<HubInput>, now: Ms) {
        let shown: BTreeMap<_, _> = inputs
            .iter()
            .filter_map(|input| match input {
                HubInput::System(observation) => Some((
                    observation.update.key.clone(),
                    (
                        observation.update.name.clone().unwrap_or_default(),
                        observation.update.via.unwrap_or(Via::Usb),
                    ),
                )),
                _ => None,
            })
            .collect();
        let in_grace = |at: Ms| now < at.saturating_add(REMOVAL_GRACE_MS);
        self.departed.retain(|row| in_grace(row.at));
        // The hub drops a grace row only on its next Tick; keep hiding well past that.
        self.replaced
            .retain(|_, at| now < at.saturating_add(2 * REMOVAL_GRACE_MS));
        let previous = std::mem::take(&mut self.shown);
        for (key, (name, via)) in &previous {
            if !shown.contains_key(key) {
                inputs.push(HubInput::SystemGone(key.clone()));
                self.departed.push(Departed {
                    key: key.clone(),
                    name: name.clone(),
                    via: *via,
                    at: now,
                });
            }
        }
        let wgi = |key: &DeviceKey| key.0.starts_with(WGI_PREFIX);
        for (key, (name, via)) in shown.iter().filter(|(key, _)| !previous.contains_key(key)) {
            self.replaced.remove(key);
            self.departed.retain(|row| row.key != *key);
            let found = self
                .departed
                .iter()
                .position(|row| wgi(key) && wgi(&row.key) && row.name == *name && row.via == *via)
                .or_else(|| {
                    self.departed
                        .iter()
                        .position(|row| wgi(&row.key) != wgi(key) && row.at == now)
                });
            if let Some(index) = found {
                let row = self.departed.remove(index);
                self.replaced.insert(row.key, row.at);
            }
        }
        self.shown = shown;
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
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CR_BUFFER_SMALL, CR_FAILURE, CR_INVALID_DEVNODE,
    };
    use windows::Win32::Devices::Properties::DEVPROP_TYPE_STRING;
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
            gamepad: true,
            name: "Example Controller".into(),
            report,
        }
    }

    fn poll(pads: Vec<Pad>, non_usb: &[u16]) -> Option<Poll> {
        Some(Poll {
            pads,
            hid: HidProducts {
                non_usb: non_usb.iter().map(|&pid| (MICROSOFT, pid)).collect(),
                microsoft_unknown: false,
            },
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
        source.hide_replaced(&mut states);
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

    #[test]
    fn a_wgi_row_does_not_linger_when_xinput_takes_over() {
        let mut source = Source::default();
        let mut hub = hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            || [None; 4],
            0,
        );
        assert_eq!(rows(&states), [("pad:wgi:1", None)]);
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None)], &[]),
            || [slot(BATTERY_TYPE_NIMH, BATTERY_LEVEL_LOW), None, None, None],
            60_000,
        );
        assert_eq!(
            rows(&states),
            [("pad:xinput:0", Some(Level::Coarse(CoarseLevel::Low)))]
        );
    }

    #[test]
    fn wgi_and_xinput_rows_report_their_own_changes() {
        // Windows holds the pad's own pushed status; reading it sends nothing to the pad.
        let mut source = Source::default();
        let mut first = hub();
        let states = apply(
            &mut source,
            &mut first,
            poll(vec![pad(1, USB, Some(report(100, 100)))], &[]),
            no_xinput,
            0,
        );
        assert_eq!(states[0].key.0, "pad:wgi:1");
        assert!(states[0].reports_changes);
        let mut source = Source::default();
        let mut fresh = hub();
        let states = apply(
            &mut source,
            &mut fresh,
            poll(vec![pad(1, USB, None)], &[]),
            || [slot(BATTERY_TYPE_NIMH, BATTERY_LEVEL_LOW), None, None, None],
            0,
        );
        assert_eq!(states[0].key.0, "pad:xinput:0");
        assert!(states[0].reports_changes);
    }

    #[test]
    fn a_replugged_pad_shows_one_row() {
        let mut source = Source::default();
        let mut hub = hub();
        let full = || Some(report(100, 100));
        apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, full())], &[]),
            no_xinput,
            0,
        );
        // WGI hands the same pad back as a new controller object.
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(2, USB, full())], &[]),
            no_xinput,
            1_000,
        );
        assert_eq!(
            rows(&states),
            [("pad:wgi:2", Some(Level::Coarse(CoarseLevel::Full)))]
        );
        // A different pad that left is still shown in its grace.
        let other = Pad {
            name: "Other Controller".into(),
            ..pad(3, USB, full())
        };
        apply(
            &mut source,
            &mut hub,
            poll(vec![pad(2, USB, full()), other], &[]),
            no_xinput,
            2_000,
        );
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(2, USB, full())], &[]),
            no_xinput,
            3_000,
        );
        assert_eq!(states.len(), 2);
        assert_eq!(states[1].key.0, "pad:wgi:3");
        assert_eq!(states[1].presence, Presence::Offline);
    }

    #[test]
    fn third_party_xinput_pads_are_never_shown_as_xbox() {
        let third_party = |id| Pad {
            vid: 0x3537,
            ..pad(id, 0x1000, None)
        };
        // A Microsoft wheel or joystick is not a gamepad and is never identified.
        let wheel = Pad {
            gamepad: false,
            ..pad(1, 0x0001, None)
        };
        let mut source = Source::default();
        let mut hub = hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![wheel, third_party(2)], &[]),
            no_xinput,
            0,
        );
        assert!(states.is_empty());
        // Two USB gamepads, one third-party: the slots cannot be attributed.
        let mut source = Source::default();
        let mut hub = self::hub();
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, None), third_party(2)], &[]),
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

    fn list(paths: &[&str]) -> Vec<u16> {
        let mut list: Vec<u16> = Vec::new();
        for path in paths {
            list.extend(path.encode_utf16().chain([0]));
        }
        list.push(0);
        list
    }

    const CLASSIC: &str = r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0002045e_pid&02e0&col01#1&2&0&0000#{4d1e55b2}";
    const LE: &str = r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&02045e_PID&0b13_REV&0509_000000000000#1&2&0&0000#{4d1e55b2}";

    fn known(products: &[(u16, u16)]) -> HidProducts {
        HidProducts {
            non_usb: products.iter().copied().collect(),
            microsoft_unknown: false,
        }
    }

    const UNKNOWN: HidProducts = HidProducts {
        non_usb: BTreeSet::new(),
        microsoft_unknown: true,
    };

    fn no_game_class(_: &[u16]) -> bool {
        false
    }

    /// A compatible-ID read that answers `status` with `kind` and the NUL-separated `ids`.
    fn cached(
        status: CONFIGRET,
        kind: DEVPROPTYPE,
        ids: &str,
    ) -> impl FnOnce(u32, &mut DEVPROPTYPE, &mut [u16], &mut u32) -> CONFIGRET {
        move |node, out_kind, out, size| {
            assert_eq!(node, 7);
            let units: Vec<u16> = ids
                .encode_utf16()
                .map(|unit| if unit == u16::from(b'|') { 0 } else { unit })
                .collect();
            assert!(units.len() * 2 <= *size as usize);
            out[..units.len()].copy_from_slice(&units);
            *out_kind = kind;
            *size = (units.len() * 2) as u32;
            status
        }
    }

    const GAME: &str = "HID\\VID_1234|HID_DEVICE_SYSTEM_GAME|HID_DEVICE|";
    const OTHER: &str = "HID\\VID_1234|HID_DEVICE_UP:0001_U:0002|HID_DEVICE|";

    #[test]
    fn game_class_answers_only_from_a_clean_read() {
        let list = DEVPROP_TYPE_STRING_LIST;
        let none = DEVPROPTYPE::default();
        assert!(!game_class_from(
            Some(7),
            cached(CR_NO_SUCH_VALUE, none, "")
        ));
        assert!(!game_class_from(Some(7), cached(CR_SUCCESS, list, OTHER)));
        assert!(game_class_from(Some(7), cached(CR_SUCCESS, list, GAME)));
    }

    #[test]
    fn game_class_lookup_errors_fail_closed() {
        let never = |_: u32, _: &mut DEVPROPTYPE, _: &mut [u16], _: &mut u32| -> CONFIGRET {
            panic!("no devnode to read")
        };
        let list = DEVPROP_TYPE_STRING_LIST;
        let cases = [
            ("node lookup failure", game_class_from(None, never)),
            (
                "other error",
                game_class_from(Some(7), cached(CR_FAILURE, list, OTHER)),
            ),
            (
                "stale devnode",
                game_class_from(Some(7), cached(CR_INVALID_DEVNODE, list, "")),
            ),
            (
                "wrong type",
                game_class_from(Some(7), cached(CR_SUCCESS, DEVPROP_TYPE_STRING, OTHER)),
            ),
            (
                "buffer too small",
                game_class_from(Some(7), cached(CR_BUFFER_SMALL, list, "")),
            ),
        ];
        let open: Vec<&str> = cases
            .iter()
            .filter(|(_, closed)| !closed)
            .map(|(case, _)| *case)
            .collect();
        assert!(open.is_empty(), "failed open: {open:?}");
    }

    #[test]
    fn non_usb_products_come_from_path_text_and_fail_closed() {
        let products = non_usb_products(
            &list(&[
                r"\\?\HID#VID_045E&PID_0B12&IG_00#1&2&0&0000#{4d1e55b2}",
                CLASSIC,
                LE,
                r"\\?\HID#ConvertedDevice&Col01#1&2&0&0000#{4d1e55b2}",
                // The instance segment is never read, even when it looks like a VID.
                r"\\?\HID#ConvertedDevice&Col02#5&2d8b045e&0&0000#{4d1e55b2}",
            ]),
            no_game_class,
        );
        assert_eq!(products, known(&[(MICROSOFT, 0x02E0), (MICROSOFT, 0x0B13)]));
        // Microsoft's Bluetooth SIG company ID (0x0006), or a path that cannot be parsed
        // but mentions Microsoft's USB VID, rules out every Microsoft pad.
        for unparsable in [
            r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0002045e#1&2&0&0000#{4d1e55b2}",
            r"\\?\HID#VID_045E&PID_XYZ#1&2&0&0000#{4d1e55b2}",
            r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&00010006_pid&02e0#1&2&0&0000#{4d1e55b2}",
            r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&010006_PID&0b13_REV&0509#1&2&0&0000#{4d1e55b2}",
        ] {
            let products = non_usb_products(&list(&[CLASSIC, unparsable]), no_game_class);
            assert!(products.microsoft_unknown, "{unparsable}");
        }
    }

    #[test]
    fn other_bluetooth_vendors_do_not_hide_xbox_pads() {
        const APPLE: &str = r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0001004c_pid&0267&col01#1&2&0&0000#{4d1e55b2}";
        const ZERO: &str = r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&000000_PID&0000_REV&0000_000000000000#1&2&0&0000#{4d1e55b2}";
        // Another SIG company, and a zero source without a game-class compatible ID.
        for path in [APPLE, ZERO] {
            let products = non_usb_products(&list(&[path]), no_game_class);
            assert!(!products.microsoft_unknown, "{path}");
            assert!(products.non_usb.is_empty());
        }
        // A game-class device whose VID cannot be read still fails closed.
        let game = |path: &[u16]| String::from_utf16_lossy(path) == ZERO;
        assert!(non_usb_products(&list(&[APPLE, ZERO]), game).microsoft_unknown);
        // A parsed product or a SIG company never needs the devnode lookup.
        let unread = |_: &[u16]| -> bool { unreachable!("parsed paths need no lookup") };
        assert!(!non_usb_products(&list(&[APPLE, CLASSIC, LE]), unread).microsoft_unknown);
    }

    #[test]
    fn an_unparsable_microsoft_path_ends_rows_instead_of_freezing_them() {
        let mut source = Source::default();
        let mut hub = hub();
        let full = || Some(report(100, 100));
        apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, full())], &[]),
            no_xinput,
            0,
        );
        let unknown = Some(Poll {
            pads: vec![pad(1, USB, full())],
            hid: UNKNOWN,
        });
        let states = apply(&mut source, &mut hub, unknown, no_xinput, 10);
        assert_eq!(states[0].key.0, "pad:wgi:1");
        assert_eq!(states[0].presence, Presence::Offline);
    }

    #[test]
    fn hid_products_are_relisted_only_on_triggers_and_after_failures() {
        let mut source = Source::default();
        let listed = || Some(list(&[LE]));
        let unlisted = || -> Option<Vec<u16>> { unreachable!("not re-listed") };
        let expected = Some(known(&[(MICROSOFT, BLE)]));
        assert_eq!(source.products(listed), expected);
        assert_eq!(source.products(unlisted), expected);
        // Only a failed Configuration Manager list is retried at the next poll.
        source.hid_changed();
        assert_eq!(source.deadline(), Some(0));
        assert_eq!(source.products(|| None), None);
        assert_eq!(source.products(listed), expected);
        assert_eq!(source.products(unlisted), expected);
        // An unparsable path is a successful list that identifies no Microsoft pad.
        source.hid_changed();
        let unparsable = || Some(list(&[r"\\?\HID#VID_045E#1&2&0&0000#{4d1e55b2}"]));
        assert_eq!(source.products(unparsable), Some(UNKNOWN));
        assert_eq!(source.products(unlisted), Some(UNKNOWN));
    }

    #[test]
    fn two_identical_pads_keep_the_removed_pads_row() {
        let mut source = Source::default();
        let mut hub = hub();
        let full = || Some(report(100, 100));
        apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, full()), pad(2, USB, full())], &[]),
            no_xinput,
            0,
        );
        // Unplugging one of two identical pads leaves its offline row in grace.
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, full())], &[]),
            no_xinput,
            1_000,
        );
        assert_eq!(
            states
                .iter()
                .map(|state| (state.key.0.as_str(), state.presence))
                .collect::<Vec<_>>(),
            [
                ("pad:wgi:1", Presence::Online),
                ("pad:wgi:2", Presence::Offline)
            ]
        );
        // Replugged, it comes back under a new key and stands for that one row only.
        let states = apply(
            &mut source,
            &mut hub,
            poll(vec![pad(1, USB, full()), pad(3, USB, full())], &[]),
            no_xinput,
            2_000,
        );
        assert_eq!(
            states
                .iter()
                .map(|state| state.key.0.as_str())
                .collect::<Vec<_>>(),
            ["pad:wgi:1", "pad:wgi:3"]
        );
    }
}
