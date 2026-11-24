//! HID++ engine: event-first discovery and battery reading for Logitech
//! receivers and directly attached devices.
//!
//! One engine serves one HID group. On the group's arrival grant it enables
//! the receiver's connection notifications and asks the receiver to report
//! every paired slot with a 0x41 link notification (falling back to pinging
//! every slot when it will not), or pings a direct device. Each linked device
//! is discovered once (protocol, battery feature, name, kind, unit id, first
//! battery reading). Afterwards the engine lives on pushed reports: battery
//! broadcasts and register notifications become `Update`s, receiver link
//! notifications become `Asleep` or `Connected`. A device is only asked again
//! under a grant the hub hands out for it.
//!
//! Every device index has at most one request in flight; a request
//! unanswered within its timeout (1 s for the receiver itself, 4 s for a
//! device) counts as no answer.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::RangeInclusive;

use crate::catalog::Catalog;
use crate::engine::{Action, Engine};
use crate::hid::{Collection, CollectionId, HidGroup};
use crate::model::{DeviceKey, DeviceKind, DeviceUpdate, Presence, Source, Via};
use crate::scheduler::{Capabilities, Grant, GrantId, Trigger};
use crate::time::Ms;

use super::battery::{self, Reading};
use super::curves::{self, VoltageCurve};
use super::discovery::{
    self, Decoder, ERR_CONNECT_FAIL, ERR_RESOURCE_ERROR, ERR_UNKNOWN_DEVICE, Found, Next,
    REG_BATTERY_CHARGE, REG_BATTERY_STATUS, Request, Step,
};
use super::frame::{LONG, Reply, SHORT, SW_ID};

/// Timeout of a request to the receiver itself.
/// Source: Solaar `base.py` `_RECEIVER_REQUEST_TIMEOUT` (0.9 s).
const RECEIVER_TIMEOUT_MS: u32 = 1_000;
/// Timeout of a request to a device (receiver slot or direct): an idle device
/// may take over a second to wake (owner trace 2026-11-23: 443 to 1 288 ms).
/// Source: Solaar `base.py` `_DEVICE_REQUEST_TIMEOUT` / `_PING_TIMEOUT` (4 s).
pub const DEVICE_TIMEOUT_MS: u32 = 4_000;
/// HID++ 1.0 sub ids from 0x80 up are register access and errors, never a feature reply.
const FIRST_REGISTER_SUB_ID: u8 = 0x80;

const LOGITECH: u16 = 0x046D;

// HID++ collections. Source: Linux kernel hid-logitech-dj.c / hid-logitech-hidpp.c
// (vendor page 0xFF00: usage 1 short 7-byte reports, usage 2 long 20-byte
// reports); HeadsetControl `logitech_g633_g933_935.hpp` (G headsets: 0xFF43/0x0202).
const SHORT_USAGE: (u16, u16) = (0xFF00, 0x0001);
const LONG_USAGES: [(u16, u16); 2] = [(0xFF00, 0x0002), (0xFF43, 0x0202)];
const HEADSET_PAGE: u16 = 0xFF43;
const SHORT_LEN: u16 = 7;
const LONG_LEN: u16 = 20;

/// Device index of a direct device, and of the receiver itself.
const DIRECT: u8 = 0xFF;
/// Device indices behind a receiver. Source: Solaar `receiver.py` (max 6 devices).
const RECEIVER_SLOTS: RangeInclusive<u8> = 1..=6;

/// Receiver sub id reporting a device's link state; `flags & 0x40` means link lost.
// Source: Solaar `notifications.py` (0x41 DJ pairing), `hidpp10_constants.py`.
const LINK_NOTIFICATION: u8 = 0x41;
const LINK_LOST: u8 = 0x40;
/// Receiver sub id for connect/disconnect events; address 0x02 means the device was unpaired.
// Source: Solaar `notifications.py` (CONNECT_DISCONNECT 0x40, address 0x02 un-paired).
const CONNECT_DISCONNECT: u8 = 0x40;
const UNPAIRED: u8 = 0x02;

/// One device index's exchange under a grant.
#[derive(Debug)]
struct Conversation {
    grant: GrantId,
    step: Step,
    found: Found,
    /// The device is registered: the conversation ends in a [`Step::Read`], not a `Register`.
    known: bool,
    /// A timed-out read was already asked again.
    retried: bool,
}

impl Conversation {
    fn new(grant: GrantId, step: Step) -> Self {
        Self {
            grant,
            step,
            found: Found::default(),
            known: false,
            retried: false,
        }
    }
}

#[derive(Debug)]
struct InFlight {
    conversation: Conversation,
    request: Request,
    deadline: Ms,
}

/// A discovered device.
#[derive(Debug)]
struct Device {
    key: DeviceKey,
    decoder: Decoder,
    /// Battery feature index (HID++ 2.0 only).
    battery_fi: u8,
}

/// What is left of a grant we were handed.
#[derive(Debug)]
struct Budget {
    remaining: u16,
    expires_ms: Ms,
}

/// What a battery payload says.
enum Outcome {
    Reading(Reading),
    /// The device answered that it is off (0x1F20 with unknown flags).
    Offline,
    /// Nothing usable (e.g. a truncated payload).
    Nothing,
}

pub struct HidppEngine {
    group_key: DeviceKey,
    pid: u16,
    product: String,
    /// Report id and collection every request is written with.
    report_id: u8,
    out: CollectionId,
    receiver: bool,
    fallback_kind: DeviceKind,
    curve: VoltageCurve,
    query_wakes: bool,
    devices: BTreeMap<u8, Device>,
    in_flight: BTreeMap<u8, InFlight>,
    /// Indices whose link came up while unknown: discovered on the next group grant.
    pending: BTreeSet<u8>,
    /// Slots whose one late-reply rediscovery is used up; re-armed by a 0x41 for
    /// the slot, by `forget` or by a successful discovery.
    late_used: BTreeSet<u8>,
    /// Wireless product id per receiver slot, from the slot's last 0x41.
    wpids: BTreeMap<u8, u16>,
    budgets: HashMap<GrantId, Budget>,
}

fn is_short(c: &Collection) -> bool {
    c.vid == LOGITECH && (c.usage_page, c.usage) == SHORT_USAGE && c.output_len == SHORT_LEN
}

fn is_long(c: &Collection) -> bool {
    c.vid == LOGITECH && LONG_USAGES.contains(&(c.usage_page, c.usage)) && c.output_len == LONG_LEN
}

fn update(key: DeviceKey, presence: Presence, source: Source) -> DeviceUpdate {
    DeviceUpdate {
        key,
        name: None,
        kind: None,
        via: None,
        level: None,
        millivolts: None,
        charge: None,
        presence: Some(presence),
        source,
    }
}

fn reading_update(key: DeviceKey, r: &Reading, source: Source) -> DeviceUpdate {
    DeviceUpdate {
        level: r.level,
        millivolts: r.millivolts,
        charge: Some(r.charge),
        ..update(key, Presence::Online, source)
    }
}

impl HidppEngine {
    /// A Logitech group with a short or long HID++ collection.
    pub fn accepts(group: &HidGroup) -> bool {
        group.collections.iter().any(Self::reads)
    }

    /// A short or long HID++ collection: the only collections the engine reads or writes.
    pub fn reads(c: &Collection) -> bool {
        is_short(c) || is_long(c)
    }

    /// # Panics
    /// If [`HidppEngine::accepts`] rejects `group`.
    pub fn new(group: &HidGroup, catalog: &Catalog) -> Self {
        // Requests go out short when the device has a short collection;
        // long-only devices get long frames for everything.
        let (report_id, out) = match group.collections.iter().find(|c| is_short(c)) {
            Some(c) => (SHORT, c),
            None => (
                LONG,
                group
                    .collections
                    .iter()
                    .find(|c| is_long(c))
                    .expect("HidppEngine::new needs a group it accepts"),
            ),
        };
        let quirks = catalog.device(out.vid, out.pid).map(|d| &d.quirks);
        let curve = quirks
            .and_then(|q| q.voltage_curve.as_deref())
            .and_then(curves::curve_by_name)
            .unwrap_or(VoltageCurve::HeadsetControlG633);
        let fallback_kind = if group
            .collections
            .iter()
            .any(|c| c.usage_page == HEADSET_PAGE)
        {
            DeviceKind::Headset
        } else {
            DeviceKind::Other
        };
        Self {
            group_key: DeviceKey(format!("logi-group:{:04X}:{}", out.pid, group.id.0)),
            pid: out.pid,
            product: out.product.clone(),
            report_id,
            out: out.id,
            receiver: out.product.contains("Receiver"),
            fallback_kind,
            curve,
            query_wakes: quirks.is_some_and(|q| q.query_wakes),
            devices: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            pending: BTreeSet::new(),
            late_used: BTreeSet::new(),
            wpids: BTreeMap::new(),
            budgets: HashMap::new(),
        }
    }

    fn device_key(&self, index: u8, unit: u32) -> DeviceKey {
        // 0xFFFFFFFF is a placeholder (the PRO X Wireless dongle reports it), not an identity.
        if unit != 0 && unit != u32::MAX {
            DeviceKey(format!("logi:unit:{unit:08X}"))
        } else {
            DeviceKey(format!("logi:{:04X}:{index}", self.pid))
        }
    }

    /// How long a request to `index` may stay unanswered.
    fn timeout(&self, index: u8) -> u32 {
        if self.receiver && index == DIRECT {
            RECEIVER_TIMEOUT_MS
        } else {
            DEVICE_TIMEOUT_MS
        }
    }

    /// Takes one send from `grant`'s budget.
    fn spend(&mut self, grant: GrantId, now: Ms) -> bool {
        match self.budgets.get_mut(&grant) {
            Some(b) if b.remaining > 0 && now <= b.expires_ms => {
                b.remaining -= 1;
                true
            }
            _ => false,
        }
    }

    fn request(&self, index: u8, c: &Conversation) -> Option<Request> {
        if c.step == Step::Read {
            let d = self.devices.get(&index)?;
            return Some(d.decoder.read(d.battery_fi));
        }
        discovery::request(c.step, &c.found)
    }

    /// Sends the request for `c`'s step to `index`. Without budget left the
    /// conversation ends quietly: the device did nothing wrong.
    fn issue(&mut self, index: u8, c: Conversation, now: Ms, actions: &mut Vec<Action>) {
        let Some(request) = self.request(index, &c) else {
            return;
        };
        if !self.spend(c.grant, now) {
            return;
        }
        actions.push(Action::Send {
            grant: c.grant,
            collection: self.out,
            bytes: request.bytes(self.report_id, index),
        });
        let deadline = now.saturating_add(Ms::from(self.timeout(index)));
        self.in_flight.insert(
            index,
            InFlight {
                conversation: c,
                request,
                deadline,
            },
        );
    }

    /// Pings each idle index in `indices`.
    fn discover(
        &mut self,
        indices: impl IntoIterator<Item = u8>,
        grant: GrantId,
        now: Ms,
        actions: &mut Vec<Action>,
    ) {
        for index in indices {
            if !self.in_flight.contains_key(&index) {
                self.issue(index, Conversation::new(grant, Step::Ping), now, actions);
            }
        }
    }

    fn interpret(&self, decoder: Decoder, p: &[u8]) -> Outcome {
        let r = match decoder {
            Decoder::Unified => battery::decode_1004(p),
            Decoder::LevelStatus => battery::decode_1000(p),
            Decoder::Voltage => battery::decode_1001(p),
            Decoder::Adc => battery::decode_1f20(p, self.curve),
            Decoder::Reg0D => battery::decode_reg_0d(p),
            Decoder::Reg07 => battery::decode_reg_07(p),
        };
        if r.online {
            Outcome::Reading(r)
        } else if decoder == Decoder::Adc && p.len() >= 3 {
            // Only a full 0x1F20 payload with unknown flags means the headset is
            // off; a truncated one is no reading at all.
            Outcome::Offline
        } else {
            Outcome::Nothing
        }
    }

    /// Moves `c` on after `reply` to its request on `index`.
    fn advance(
        &mut self,
        index: u8,
        mut c: Conversation,
        reply: Reply<'_>,
        now: Ms,
        actions: &mut Vec<Action>,
    ) {
        if c.step == Step::Read {
            self.read_reply(index, reply, actions);
            return;
        }
        match discovery::advance(c.step, &mut c.found, reply) {
            // A known device re-enabled its notifications: read with what we know.
            Next::Ask(Step::Battery) if c.known => {
                c.step = Step::Read;
                self.issue(index, c, now, actions);
            }
            Next::Ask(step) => {
                c.step = step;
                self.issue(index, c, now, actions);
            }
            Next::Discovered(p) => self.discovered(index, c.found, Some(p), actions),
            // Receiver notifications set up (or refused): ask it to report its slots.
            Next::Done if matches!(c.step, Step::GetNotifications | Step::SetNotifications(_)) => {
                c.step = Step::NotifyDevices;
                self.issue(index, c, now, actions);
            }
            // The receiver now sends a 0x41 per paired slot.
            Next::Done if c.step == Step::NotifyDevices && matches!(reply, Reply::Ok(_)) => {}
            // It refused: find the devices by pinging every slot.
            Next::Done if c.step == Step::NotifyDevices => {
                self.discover(RECEIVER_SLOTS, c.grant, now, actions);
            }
            // A known device answered with an error: same meaning as on a read.
            Next::Done if c.known => self.read_reply(index, reply, actions),
            // A direct device found its battery feature but the battery did not answer
            // (owner trace 2026-11-23: headset off, dongle on → Err20 0x05). No link-up
            // notification will bring it back, so register it asleep now.
            Next::Done
                if c.step == Step::Battery
                    && self.is_direct(index)
                    && matches!(
                        reply,
                        Reply::Err20(_)
                            | Reply::Err10(
                                ERR_CONNECT_FAIL | ERR_UNKNOWN_DEVICE | ERR_RESOURCE_ERROR
                            )
                    ) =>
            {
                self.discovered(index, c.found, None, actions);
            }
            Next::Done => {}
        }
    }

    /// The device behind a direct (non-receiver) group.
    fn is_direct(&self, index: u8) -> bool {
        !self.receiver && index == DIRECT
    }

    /// Registers the device on `index` from its first battery payload `p`;
    /// `None` (the first read failed) registers it asleep without a level.
    fn discovered(&mut self, index: u8, found: Found, p: Option<&[u8]>, actions: &mut Vec<Action>) {
        let Some((decoder, battery_fi)) = found.battery else {
            return;
        };
        let key = self.device_key(index, found.unit);
        let mut initial = match p.map(|p| self.interpret(decoder, p)) {
            Some(Outcome::Reading(r)) => reading_update(key.clone(), &r, Source::Query),
            Some(Outcome::Offline) | None => update(key.clone(), Presence::Asleep, Source::Query),
            Some(Outcome::Nothing) => return,
        };
        let name = String::from_utf8_lossy(&found.name);
        let name = name.trim_end_matches('\0').trim();
        initial.name = Some(if name.is_empty() {
            self.product.clone()
        } else {
            name.to_owned()
        });
        initial.kind = Some(found.kind.unwrap_or(self.fallback_kind));
        initial.via = Some(if self.receiver {
            Via::Receiver
        } else {
            Via::Usb
        });

        // The same device may have moved to another index (re-pairing).
        self.devices.retain(|_, d| d.key != key);
        self.late_used.remove(&index);
        self.devices.insert(
            index,
            Device {
                key: key.clone(),
                decoder,
                battery_fi,
            },
        );
        actions.push(Action::Register {
            key,
            caps: Capabilities {
                pushes_battery: true,
                query_wakes: self.query_wakes,
                min_query_gap_ms: 0,
            },
            initial,
            wpid: self.wpids.get(&index).copied(),
        });
    }

    /// Handles the answer to a known device's battery read.
    fn read_reply(&mut self, index: u8, reply: Reply<'_>, actions: &mut Vec<Action>) {
        let Some(d) = self.devices.get(&index) else {
            return;
        };
        match reply {
            Reply::Ok(p) => match self.interpret(d.decoder, p) {
                Outcome::Reading(r) => {
                    actions.push(Action::Update(reading_update(
                        d.key.clone(),
                        &r,
                        Source::Query,
                    )));
                }
                Outcome::Offline => actions.push(Action::Asleep(d.key.clone())),
                Outcome::Nothing => {}
            },
            // Source: Solaar `base.py` (0x04 connect fail, 0x08 "no device with that number
            // currently accessible", 0x09 resource error); kernel hid-logitech-hidpp.c treats 0x08
            // and 0x09 alike as "device might not be connected". Hardware: an idle Superlight behind
            // a LIGHTSPEED receiver answers 0x08. Unpairing is the receiver's 0x40/0x02 notification.
            Reply::Err20(_)
            | Reply::Err10(ERR_CONNECT_FAIL | ERR_UNKNOWN_DEVICE | ERR_RESOURCE_ERROR) => {
                actions.push(Action::Asleep(d.key.clone()));
            }
            Reply::Err10(_) | Reply::NotMine => {}
        }
    }

    /// The device on `index` is gone (unpaired): drop it so the next link-up rediscovers the index.
    fn forget(&mut self, index: u8, actions: &mut Vec<Action>) {
        self.in_flight.remove(&index);
        self.late_used.remove(&index);
        self.wpids.remove(&index);
        if let Some(d) = self.devices.remove(&index) {
            actions.push(Action::Unregister(d.key));
        }
    }

    /// A report that answers no request: a broadcast or notification. Never sends.
    fn on_event(&mut self, frame: &[u8], actions: &mut Vec<Action>) {
        let &[report, index, sub, address, ..] = frame else {
            return;
        };
        if report != SHORT && report != LONG {
            return;
        }
        if self.receiver && report == SHORT && sub == LINK_NOTIFICATION {
            self.on_link(index, frame, actions);
            return;
        }
        if self.receiver
            && report == SHORT
            && sub == CONNECT_DISCONNECT
            && address == UNPAIRED
            && RECEIVER_SLOTS.contains(&index)
        {
            self.forget(index, actions);
            return;
        }
        // A feature reply carrying our software id from a slot with nothing in flight.
        if self.receiver
            && RECEIVER_SLOTS.contains(&index)
            && sub < FIRST_REGISTER_SUB_ID
            && address & 0x0F == SW_ID
        {
            self.late_reply(index, actions);
        }
        // A direct device may report as index 0x00 instead of 0xFF.
        let index = if !self.receiver && index == 0 {
            DIRECT
        } else {
            index
        };
        let Some(d) = self.devices.get(&index) else {
            return;
        };
        let outcome = match d.decoder {
            // Source: kernel hid-logitech-hidpp.c / Solaar `notifications.py`:
            // short register notifications carry the register payload from byte 3.
            Decoder::Reg0D | Decoder::Reg07 if report == SHORT => match sub {
                REG_BATTERY_STATUS => self.interpret(Decoder::Reg07, &frame[3..]),
                REG_BATTERY_CHARGE => self.interpret(Decoder::Reg0D, &frame[3..]),
                _ => return,
            },
            Decoder::Reg0D | Decoder::Reg07 => return,
            // HID++ 2.0 broadcasts: battery feature index, function 0, software id 0.
            _ if sub == d.battery_fi && address == 0x00 => self.interpret(d.decoder, &frame[4..]),
            _ => return,
        };
        match outcome {
            Outcome::Reading(r) => {
                actions.push(Action::Update(reading_update(
                    d.key.clone(),
                    &r,
                    Source::Push,
                )));
            }
            Outcome::Offline => actions.push(Action::Asleep(d.key.clone())),
            Outcome::Nothing => {}
        }
    }

    /// Receiver 0x41 link notification for `index`.
    // Source: Solaar `notifications.py` (DJ pairing / 0x41 link status handling).
    fn on_link(&mut self, index: u8, frame: &[u8], actions: &mut Vec<Action>) {
        let Some(&flags) = frame.get(4) else {
            return;
        };
        let lost = flags & LINK_LOST != 0;
        // Bytes 5..7: the slot's wireless product id, little-endian; zero means not reported.
        // Source: Linux hid-logitech-dj.c (HIDPP_PARAM_EQUAD_LSB/MSB of the 0x41 notification);
        // owner capture 2026-11-23: `93 40` from a mouse whose 0x0003 model id is 4093.
        if let Some(&[lo, hi]) = frame.get(5..7)
            && RECEIVER_SLOTS.contains(&index)
            && u16::from_le_bytes([lo, hi]) != 0
        {
            self.wpids.insert(index, u16::from_le_bytes([lo, hi]));
        }
        // The receiver vouches for the slot again: a late reply may rediscover it once more.
        self.late_used.remove(&index);
        if let Some(d) = self.devices.get(&index) {
            actions.push(if lost {
                Action::Asleep(d.key.clone())
            } else {
                Action::Connected(d.key.clone())
            });
        } else if !lost && RECEIVER_SLOTS.contains(&index) && !self.in_flight.contains_key(&index) {
            // New pairing, or a device that slept through our arrival pings.
            self.pending.insert(index);
            actions.push(Action::Connected(self.group_key.clone()));
        }
    }

    /// An answer to one of our requests from slot `index` that came after its
    /// timeout: the device is there and awake. If the slot is still unknown,
    /// discover it as on a link-up (an already linked device sends no 0x41) —
    /// once: a device that keeps answering too late is not pinged forever.
    fn late_reply(&mut self, index: u8, actions: &mut Vec<Action>) {
        if self.devices.contains_key(&index)
            || self.pending.contains(&index)
            || self.in_flight.contains_key(&index)
            || !self.late_used.insert(index)
        {
            return;
        }
        self.pending.insert(index);
        actions.push(Action::Connected(self.group_key.clone()));
    }
}

impl Engine for HidppEngine {
    fn group_key(&self) -> DeviceKey {
        self.group_key.clone()
    }

    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action> {
        self.budgets.retain(|_, b| now <= b.expires_ms);
        self.budgets.insert(
            grant.id,
            Budget {
                remaining: grant.max_requests,
                expires_ms: grant.expires_ms,
            },
        );
        let mut actions = Vec::new();
        if grant.subject == self.group_key {
            if grant.trigger == Trigger::Arrival {
                if self.receiver {
                    let c = Conversation::new(grant.id, Step::GetNotifications);
                    self.issue(DIRECT, c, now, &mut actions);
                } else {
                    self.discover([DIRECT], grant.id, now, &mut actions);
                }
            } else {
                let pending = std::mem::take(&mut self.pending);
                self.discover(pending, grant.id, now, &mut actions);
            }
        } else if let Some((&index, d)) = self.devices.iter().find(|(_, d)| d.key == grant.subject)
            && !self.in_flight.contains_key(&index)
        {
            // HID++ 1.0 devices lose their battery notification flag across a
            // power cycle; set it again on every link-up before reading.
            // Source: Solaar `device.py` (re-enables notifications when a device becomes active).
            let hidpp10 = matches!(d.decoder, Decoder::Reg0D | Decoder::Reg07);
            let c = if grant.trigger == Trigger::Connected && hidpp10 {
                Conversation {
                    known: true,
                    ..Conversation::new(grant.id, Step::GetReg00)
                }
            } else {
                Conversation::new(grant.id, Step::Read)
            };
            self.issue(index, c, now, &mut actions);
        }
        actions
    }

    fn on_report(&mut self, _collection: CollectionId, bytes: &[u8], now: Ms) -> Vec<Action> {
        let mut actions = Vec::new();
        // Replies may come on either collection of the group.
        let answer =
            self.in_flight
                .iter()
                .find_map(|(&index, f)| match f.request.reply(bytes, index) {
                    Reply::NotMine => None,
                    reply => Some((index, reply)),
                });
        match answer {
            Some((index, reply)) => {
                if let Some(f) = self.in_flight.remove(&index) {
                    self.advance(index, f.conversation, reply, now, &mut actions);
                }
            }
            None => self.on_event(bytes, &mut actions),
        }
        actions
    }

    fn on_tick(&mut self, now: Ms) -> Vec<Action> {
        let mut actions = Vec::new();
        let due: Vec<u8> = self
            .in_flight
            .iter()
            .filter(|(_, f)| f.deadline <= now)
            .map(|(&index, _)| index)
            .collect();
        for index in due {
            let Some(f) = self.in_flight.remove(&index) else {
                continue;
            };
            match f.conversation.step {
                // The receiver did not answer: still ask it to report its slots.
                Step::GetNotifications | Step::SetNotifications(_) => {
                    let c = Conversation {
                        step: Step::NotifyDevices,
                        ..f.conversation
                    };
                    self.issue(index, c, now, &mut actions);
                }
                // No slot report either: find the devices by pinging every slot.
                Step::NotifyDevices => {
                    self.discover(RECEIVER_SLOTS, f.conversation.grant, now, &mut actions);
                }
                Step::Read => {
                    let Some(d) = self.devices.get(&index) else {
                        continue;
                    };
                    if !(self.receiver && RECEIVER_SLOTS.contains(&index)) {
                        // A direct device that stays silent is gone.
                        actions.push(Action::Asleep(d.key.clone()));
                    } else if !f.conversation.retried {
                        // A receiver reports a sleeping device with an error, so
                        // silence is a lost reply: ask once more within the grant.
                        // Only a grant that outlives the timeout allows it: a Connected
                        // grant (10 s) does, a Resume/RefreshNow grant (3 s) has expired,
                        // so `issue` sends nothing and presence stays as it is.
                        let again = Conversation {
                            retried: true,
                            ..f.conversation
                        };
                        self.issue(index, again, now, &mut actions);
                    }
                    // A second silence leaves presence as it is.
                }
                // A direct device's first battery read stayed silent: register it asleep.
                Step::Battery if self.is_direct(index) => {
                    self.discovered(index, f.conversation.found, None, &mut actions);
                }
                // Discovery: absent or asleep; its next link notification brings it back.
                _ => {}
            }
        }
        actions
    }

    fn next_deadline(&self) -> Option<Ms> {
        self.in_flight.values().map(|f| f.deadline).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::{Collection, GroupId};
    use crate::model::{
        Charge, CoarseLevel, DeviceKind, DeviceUpdate, Level, Presence, Source, Via,
    };
    use crate::scheduler::{Capabilities, GrantId, Trigger};

    const SHORT_C: CollectionId = CollectionId(1);
    const LONG_C: CollectionId = CollectionId(2);

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

    const FIXTURES: [&str; 10] = [
        include_str!("../../../tests/fixtures/hidpp/receiver_mouse_discovery.txt"),
        include_str!("../../../tests/fixtures/hidpp/headset_long_only_discovery.txt"),
        include_str!("../../../tests/fixtures/hidpp/receiver_hidpp10_discovery.txt"),
        include_str!("../../../tests/fixtures/hidpp/0aba-headset-off-at-start.txt"),
        include_str!("../../../tests/fixtures/hidpp/c547-slow-ping.txt"),
        include_str!("../../../tests/fixtures/hidpp/c547-late-ping-reply.txt"),
        include_str!("../../../tests/fixtures/hidpp/c547-superlight-arrival.txt"),
        include_str!("../../../tests/fixtures/hidpp/0aba-headset-arrival.txt"),
        include_str!("../../../tests/fixtures/hidpp/c547-superlight-resume.txt"),
        include_str!("../../../tests/fixtures/hidpp/0aba-headset-resume.txt"),
    ];

    fn catalog() -> Catalog {
        Catalog::parse(CATALOG).unwrap()
    }

    fn coll(
        id: CollectionId,
        page: u16,
        usage: u16,
        len: u16,
        pid: u16,
        product: &str,
    ) -> Collection {
        Collection {
            id,
            vid: 0x046D,
            pid,
            usage_page: page,
            usage,
            input_len: len,
            output_len: len,
            feature_len: 0,
            product: product.into(),
        }
    }

    fn receiver_group() -> HidGroup {
        HidGroup {
            id: GroupId(7),
            collections: vec![
                coll(SHORT_C, 0xFF00, 0x0001, 7, 0xC547, "USB Receiver"),
                coll(LONG_C, 0xFF00, 0x0002, 20, 0xC547, "USB Receiver"),
            ],
        }
    }

    fn headset_group() -> HidGroup {
        HidGroup {
            id: GroupId(8),
            collections: vec![coll(
                LONG_C,
                0xFF43,
                0x0202,
                20,
                0x0ABA,
                "PRO X Wireless Gaming Headset",
            )],
        }
    }

    /// Short frame, zero-padded to 7 bytes.
    fn s(head: &[u8]) -> Vec<u8> {
        let mut f = head.to_vec();
        f.resize(7, 0);
        f
    }

    /// Long frame, zero-padded to 20 bytes.
    fn l(head: &[u8]) -> Vec<u8> {
        let mut f = head.to_vec();
        f.resize(20, 0);
        f
    }

    fn grant(id: u32, subject: &DeviceKey, trigger: Trigger) -> Grant {
        Grant {
            id: GrantId(id),
            subject: subject.clone(),
            trigger,
            expires_ms: 10_000,
            max_requests: 96,
        }
    }

    fn sent(actions: &[Action]) -> Vec<Vec<u8>> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .collect()
    }

    fn mouse_key() -> DeviceKey {
        DeviceKey("logi:unit:1A2B3C4D".into())
    }

    fn headset_key() -> DeviceKey {
        DeviceKey("logi:0ABA:255".into())
    }

    fn pushing() -> Capabilities {
        Capabilities {
            pushes_battery: true,
            query_wakes: false,
            min_query_gap_ms: 0,
        }
    }

    fn update(key: DeviceKey, source: Source) -> DeviceUpdate {
        DeviceUpdate {
            key,
            name: None,
            kind: None,
            via: None,
            level: None,
            millivolts: None,
            charge: None,
            presence: None,
            source,
        }
    }

    /// Feeds each reply and checks it triggers exactly the paired request.
    fn run(e: &mut HidppEngine, script: &[(Vec<u8>, Vec<u8>)], now: Ms) {
        for (i, (reply, want)) in script.iter().enumerate() {
            let actions = e.on_report(LONG_C, reply, now);
            assert_eq!(sent(&actions), vec![want.clone()], "step {i}: {actions:?}");
        }
    }

    /// The receiver's "report every paired slot" write (register 0x02 = 0x02).
    fn notify_devices() -> Vec<u8> {
        s(&[0x10, 0xFF, 0x80, 0x02, 0x02, 0x00, 0x00])
    }

    /// Receiver after arrival with wireless notifications already on and the
    /// register 0x02 write refused, so the slot ping sweep runs: pings 1..=6
    /// outstanding, sent at t = 1.
    fn receiver_pinging() -> HidppEngine {
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        let a = e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);
        assert_eq!(sent(&a), vec![s(&[0x10, 0xFF, 0x81, 0x00])]);
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), 1);
        assert_eq!(sent(&a), vec![notify_devices()]);
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x8F, 0x80, 0x02, 0x02]), 1);
        let pings: Vec<Vec<u8>> = (1..=6)
            .map(|i| s(&[0x10, i, 0x00, 0x1B, 0x00, 0x00, 0x5A]))
            .collect();
        assert_eq!(sent(&a), pings);
        e
    }

    /// Receiver after arrival whose register 0x02 write was acknowledged: waiting for 0x41s.
    fn receiver_notified() -> HidppEngine {
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), 1);
        assert_eq!(sent(&a), vec![notify_devices()]);
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x80, 0x02]), 2);
        assert!(a.is_empty(), "{a:?}");
        assert_eq!(e.next_deadline(), None);
        e
    }

    /// Replies of the mouse on index 1 from its ping up to unit id, each with the request it triggers.
    fn mouse_script() -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![
            (
                l(&[0x11, 1, 0x00, 0x1B, 0x04, 0x02, 0x5A]),
                s(&[0x10, 1, 0x00, 0x0B, 0x00, 0x05]),
            ),
            (
                l(&[0x11, 1, 0x00, 0x0B, 3]),
                s(&[0x10, 1, 0x00, 0x0B, 0x00, 0x03]),
            ),
            (
                l(&[0x11, 1, 0x00, 0x0B, 2]),
                s(&[0x10, 1, 0x00, 0x0B, 0x10, 0x04]),
            ),
            (l(&[0x11, 1, 0x00, 0x0B, 6]), s(&[0x10, 1, 3, 0x0B])),
            (l(&[0x11, 1, 3, 0x0B, 5]), s(&[0x10, 1, 3, 0x1B, 0])),
            (
                l(&[0x11, 1, 3, 0x1B, b'P', b'R', b'O', b' ', b'X']),
                s(&[0x10, 1, 3, 0x2B]),
            ),
            (l(&[0x11, 1, 3, 0x2B, 3]), s(&[0x10, 1, 2, 0x0B])),
            (
                l(&[0x11, 1, 2, 0x0B, 0x00, 0x1A, 0x2B, 0x3C, 0x4D]),
                s(&[0x10, 1, 6, 0x1B]),
            ),
        ]
    }

    /// Receiver with the mouse registered on index 1 and no conversation left.
    fn mouse_engine() -> HidppEngine {
        let mut e = receiver_pinging();
        for i in 2..=6 {
            let a = e.on_report(SHORT_C, &s(&[0x10, i, 0x8F, 0x00, 0x1B, 0x08]), 2);
            assert!(a.is_empty(), "{a:?}");
        }
        run(&mut e, &mouse_script(), 3);
        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), 4);
        assert!(matches!(a.as_slice(), [Action::Register { .. }]), "{a:?}");
        assert_eq!(e.next_deadline(), None);
        e
    }

    /// Replies of the headset from its ping up to the 0x1F20 lookup, each with the request it triggers.
    fn headset_script() -> Vec<(Vec<u8>, Vec<u8>)> {
        let get = |hi, lo| l(&[0x11, 0xFF, 0x00, 0x0B, hi, lo]);
        let found = |fi| l(&[0x11, 0xFF, 0x00, 0x0B, fi]);
        vec![
            (
                l(&[0x11, 0xFF, 0x00, 0x1B, 0x04, 0x02, 0x5A]),
                get(0x00, 0x05),
            ),
            (found(0), get(0x00, 0x03)),
            (found(0), get(0x10, 0x04)),
            (found(0), get(0x10, 0x00)),
            (found(0), get(0x10, 0x01)),
            (found(0), get(0x1F, 0x20)),
            (found(8), l(&[0x11, 0xFF, 0x08, 0x0B])),
        ]
    }

    /// Runs the headset's arrival with `catalog` and returns the engine and its `Register`.
    fn headset_with(catalog: &Catalog) -> (HidppEngine, Vec<Action>) {
        let mut e = HidppEngine::new(&headset_group(), catalog);
        let gk = e.group_key();
        let a = e.on_grant(&grant(5, &gk, Trigger::Arrival), 0);
        assert_eq!(
            sent(&a),
            vec![l(&[0x11, 0xFF, 0x00, 0x1B, 0x00, 0x00, 0x5A])]
        );
        run(&mut e, &headset_script(), 1);
        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0x08, 0x0B, 0x0E, 0xF4, 0x01]), 2);
        (e, a)
    }

    fn headset_engine() -> HidppEngine {
        let (e, a) = headset_with(&catalog());
        assert!(matches!(a.as_slice(), [Action::Register { .. }]), "{a:?}");
        assert_eq!(e.next_deadline(), None);
        e
    }

    /// Every `in` frame of every fixture.
    fn fixture_inputs() -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for text in FIXTURES {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or_default();
                let mut words = line.split_whitespace();
                if words.next() != Some("in") {
                    continue;
                }
                let len = match words.next() {
                    Some("short") => 7,
                    _ => 20,
                };
                let mut f: Vec<u8> = words.map(|w| u8::from_str_radix(w, 16).unwrap()).collect();
                f.resize(len, 0);
                frames.push(f);
            }
        }
        frames
    }

    #[test]
    fn accepts_logitech_hidpp_groups_only() {
        assert!(HidppEngine::accepts(&receiver_group()));
        assert!(HidppEngine::accepts(&headset_group()));

        let mut other_vendor = receiver_group();
        for c in &mut other_vendor.collections {
            c.vid = 0x1532;
        }
        assert!(!HidppEngine::accepts(&other_vendor));

        let mut wrong_len = receiver_group();
        wrong_len.collections[0].output_len = 20;
        wrong_len.collections[1].output_len = 7;
        assert!(!HidppEngine::accepts(&wrong_len));

        let mut mouse_only = receiver_group();
        for c in &mut mouse_only.collections {
            (c.usage_page, c.usage) = (0x0001, 0x0002);
        }
        assert!(!HidppEngine::accepts(&mouse_only));
    }

    #[test]
    fn group_key_names_pid_and_group() {
        let e = HidppEngine::new(&receiver_group(), &catalog());
        assert_eq!(e.group_key(), DeviceKey("logi-group:C547:7".into()));
        let e = HidppEngine::new(&headset_group(), &catalog());
        assert_eq!(e.group_key(), DeviceKey("logi-group:0ABA:8".into()));
    }

    #[test]
    fn receiver_arrival_enables_wireless_notifications_or_only() {
        for (flags, write) in [
            ([0x00, 0x00, 0x00], Some([0x00, 0x01, 0x00])),
            ([0x09, 0x0A, 0x30], Some([0x09, 0x0B, 0x30])),
            ([0x00, 0x01, 0x00], None),
        ] {
            let mut e = HidppEngine::new(&receiver_group(), &catalog());
            let gk = e.group_key();
            let a = e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);
            assert_eq!(
                a,
                vec![Action::Send {
                    grant: GrantId(1),
                    collection: SHORT_C,
                    bytes: s(&[0x10, 0xFF, 0x81, 0x00]),
                }]
            );
            assert_eq!(e.next_deadline(), Some(1_000));

            let a = e.on_report(
                SHORT_C,
                &s(&[&[0x10, 0xFF, 0x81, 0x00][..], &flags].concat()),
                1,
            );
            match write {
                Some(params) => {
                    assert_eq!(
                        sent(&a),
                        vec![s(&[&[0x10, 0xFF, 0x80, 0x00][..], &params].concat())]
                    );
                    let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x80, 0x00]), 2);
                    assert_eq!(sent(&a), vec![notify_devices()], "{flags:?}");
                }
                None => assert_eq!(sent(&a), vec![notify_devices()], "{flags:?}"),
            }
        }
    }

    #[test]
    fn hidpp20_discovery_registers_mouse_with_unified_battery() {
        let mut e = receiver_pinging();
        run(&mut e, &mouse_script(), 2);

        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), 3);

        assert_eq!(
            a,
            vec![Action::Register {
                key: mouse_key(),
                caps: pushing(),
                initial: DeviceUpdate {
                    name: Some("PRO X".into()),
                    kind: Some(DeviceKind::Mouse),
                    via: Some(Via::Receiver),
                    level: Some(Level::Percent(34)),
                    charge: Some(Charge::Discharging),
                    presence: Some(Presence::Online),
                    ..update(mouse_key(), Source::Query)
                },
                wpid: None,
            }]
        );
        // Pings 2..=6 went out at t = 1 and are still unanswered (device timeout 4 s).
        assert_eq!(e.next_deadline(), Some(4_001));
    }

    #[test]
    fn receiver_slot_registers_with_wireless_pid_from_link_notification() {
        let mut e = receiver_notified();
        let gk = e.group_key();
        // Owner capture 2026-11-23: link up on slot 1, wireless PID 0x4093 little-endian in
        // bytes 5..7 (the device's own 0x0003 model id says 4093 too).
        let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x41, 0x11, 0xA2, 0x93, 0x40]), 3);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);
        let a = e.on_grant(&grant(2, &gk, Trigger::Connected), 4);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x00, 0x1B, 0x00, 0x00, 0x5A])]);
        run(&mut e, &mouse_script(), 5);

        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), 6);

        let [Action::Register { key, wpid, .. }] = a.as_slice() else {
            panic!("{a:?}");
        };
        assert_eq!(*key, mouse_key());
        assert_eq!(*wpid, Some(0x4093));
    }

    #[test]
    fn long_names_are_read_in_chunks() {
        let name = b"G PRO X SUPERLIGHT 2";
        let mut e = receiver_pinging();
        let mut script = mouse_script();
        script.truncate(4);
        script.extend([
            (l(&[0x11, 1, 3, 0x0B, 20]), s(&[0x10, 1, 3, 0x1B, 0])),
            (
                l(&[&[0x11, 1, 3, 0x1B][..], &name[..16]].concat()),
                s(&[0x10, 1, 3, 0x1B, 16]),
            ),
            (
                l(&[&[0x11, 1, 3, 0x1B][..], &name[16..]].concat()),
                s(&[0x10, 1, 3, 0x2B]),
            ),
            (l(&[0x11, 1, 3, 0x2B, 3]), s(&[0x10, 1, 2, 0x0B])),
            (
                l(&[0x11, 1, 2, 0x0B, 0x00, 0x1A, 0x2B, 0x3C, 0x4D]),
                s(&[0x10, 1, 6, 0x1B]),
            ),
        ]);
        run(&mut e, &script, 2);

        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), 3);

        let [Action::Register { initial, .. }] = a.as_slice() else {
            panic!("{a:?}");
        };
        assert_eq!(initial.name.as_deref(), Some("G PRO X SUPERLIGHT 2"));
    }

    #[test]
    fn headset_long_only_uses_long_frames_and_prox_curve() {
        let mut e = HidppEngine::new(&headset_group(), &catalog());
        let gk = e.group_key();
        let send = |bytes| Action::Send {
            grant: GrantId(5),
            collection: LONG_C,
            bytes,
        };
        let a = e.on_grant(&grant(5, &gk, Trigger::Arrival), 0);
        assert_eq!(
            a,
            vec![send(l(&[0x11, 0xFF, 0x00, 0x1B, 0x00, 0x00, 0x5A]))]
        );
        // A direct device gets the device timeout.
        assert_eq!(e.next_deadline(), Some(4_000));
        for (i, (reply, want)) in headset_script().into_iter().enumerate() {
            let a = e.on_report(LONG_C, &reply, 1);
            assert_eq!(a, vec![send(want)], "step {i}");
        }

        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0x08, 0x0B, 0x0E, 0xF4, 0x01]), 2);

        assert_eq!(
            a,
            vec![Action::Register {
                key: headset_key(),
                caps: pushing(),
                initial: DeviceUpdate {
                    name: Some("PRO X Wireless Gaming Headset".into()),
                    kind: Some(DeviceKind::Headset),
                    via: Some(Via::Usb),
                    level: Some(Level::Percent(68)),
                    millivolts: Some(3828),
                    charge: Some(Charge::Discharging),
                    presence: Some(Presence::Online),
                    ..update(headset_key(), Source::Query)
                },
                wpid: None,
            }]
        );
    }

    #[test]
    fn headset_without_curve_quirk_uses_default_curve() {
        let plain = Catalog::parse("").unwrap();
        let (_, a) = headset_with(&plain);
        let [Action::Register { initial, .. }] = a.as_slice() else {
            panic!("{a:?}");
        };
        // HeadsetControl G633 curve: 3828 mV lies between 3750 (40 %) and 3850 (60 %).
        assert_eq!(initial.level, Some(Level::Percent(55)));
    }

    #[test]
    fn query_wakes_quirk_reaches_capabilities() {
        let wakes = Catalog::parse(&CATALOG.replace(
            "quirks = { voltage_curve = \"pro-x-wireless-ghub\" }",
            "quirks = { query_wakes = true }",
        ))
        .unwrap();
        let (_, a) = headset_with(&wakes);
        let [Action::Register { caps, .. }] = a.as_slice() else {
            panic!("{a:?}");
        };
        assert_eq!(
            *caps,
            Capabilities {
                query_wakes: true,
                ..pushing()
            }
        );
    }

    #[test]
    fn ping_resource_error_registers_nothing_and_does_not_retry() {
        let mut e = receiver_pinging();

        let a = e.on_report(SHORT_C, &s(&[0x10, 2, 0x8F, 0x00, 0x1B, 0x09]), 2);
        assert!(a.is_empty(), "{a:?}");

        // The other pings time out: skipped during discovery, nothing retried.
        let a = e.on_tick(1 + 4_000);
        assert!(a.is_empty(), "{a:?}");
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn hidpp10_enables_battery_reporting_bit_only() {
        for (flags, next) in [
            (
                [0x00, 0x00, 0x00],
                s(&[0x10, 1, 0x80, 0x00, 0x10, 0x00, 0x00]),
            ),
            (
                [0x01, 0x05, 0x09],
                s(&[0x10, 1, 0x80, 0x00, 0x11, 0x05, 0x09]),
            ),
            ([0x10, 0x00, 0x00], s(&[0x10, 1, 0x81, 0x0D])),
        ] {
            let mut e = receiver_pinging();
            let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x8F, 0x00, 0x1B, 0x01]), 2);
            assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x81, 0x00])]);

            let a = e.on_report(
                SHORT_C,
                &s(&[&[0x10, 1, 0x81, 0x00][..], &flags].concat()),
                3,
            );

            assert_eq!(sent(&a), vec![next], "{flags:?}");
        }
    }

    #[test]
    fn hidpp10_falls_back_to_register_07() {
        let mut e = receiver_pinging();
        let script = [
            (
                s(&[0x10, 1, 0x8F, 0x00, 0x1B, 0x01]),
                s(&[0x10, 1, 0x81, 0x00]),
            ),
            (s(&[0x10, 1, 0x81, 0x00, 0x10]), s(&[0x10, 1, 0x81, 0x0D])),
            (
                s(&[0x10, 1, 0x8F, 0x81, 0x0D, 0x02]),
                s(&[0x10, 1, 0x81, 0x07]),
            ),
        ];
        for (reply, want) in script {
            let a = e.on_report(SHORT_C, &reply, 2);
            assert_eq!(sent(&a), vec![want]);
        }

        let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x81, 0x07, 5, 0x00]), 3);

        let key = DeviceKey("logi:C547:1".into());
        assert_eq!(
            a,
            vec![Action::Register {
                key: key.clone(),
                caps: pushing(),
                initial: DeviceUpdate {
                    name: Some("USB Receiver".into()),
                    kind: Some(DeviceKind::Other),
                    via: Some(Via::Receiver),
                    level: Some(Level::Coarse(CoarseLevel::Good)),
                    charge: Some(Charge::Discharging),
                    presence: Some(Presence::Online),
                    ..update(key, Source::Query)
                },
                wpid: None,
            }]
        );

        // Register notifications of a known HID++ 1.0 device.
        let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x0D, 60, 0x00, 0x40]), 4);
        let [Action::Update(u)] = a.as_slice() else {
            panic!("{a:?}");
        };
        assert_eq!(
            (u.level, u.charge, u.source),
            (
                Some(Level::Percent(60)),
                Some(Charge::Charging),
                Source::Push
            )
        );
    }

    #[test]
    fn battery_broadcast_updates_without_send() {
        let mut e = mouse_engine();

        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x00, 40, 4, 0, 0]), 10);

        assert_eq!(
            a,
            vec![Action::Update(DeviceUpdate {
                level: Some(Level::Percent(40)),
                charge: Some(Charge::Discharging),
                presence: Some(Presence::Online),
                ..update(mouse_key(), Source::Push)
            })]
        );
    }

    #[test]
    fn adc_broadcast_offline_marks_asleep_keeps_level() {
        let mut e = headset_engine();

        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0x08, 0x00, 0x0E, 0xF4, 0x0F]), 10);

        // No Update: the level stays what it was, only presence changes.
        assert_eq!(a, vec![Action::Asleep(headset_key())]);
    }

    #[test]
    fn truncated_adc_broadcast_is_ignored() {
        let mut e = headset_engine();
        let a = e.on_report(LONG_C, &[0x11, 0xFF, 0x08, 0x00, 0x0E, 0xF4], 10);
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn direct_device_broadcast_on_index_zero_updates() {
        let mut e = headset_engine();
        let a = e.on_report(LONG_C, &l(&[0x11, 0x00, 0x08, 0x00, 0x0F, 0x83, 0x03]), 10);
        assert_eq!(
            a,
            vec![Action::Update(DeviceUpdate {
                level: Some(Level::Percent(87)),
                millivolts: Some(3971),
                charge: Some(Charge::Charging),
                presence: Some(Presence::Online),
                ..update(headset_key(), Source::Push)
            })]
        );
    }

    /// Headset dongle with every feature (owner trace 2026-11-23 shape), discovered up to
    /// the 0x1F20 read at index 6, which is in flight (sent at t = 1).
    fn headset_off_until_read() -> HidppEngine {
        let name = b"PRO X Wireless Gaming Headset";
        let mut e = HidppEngine::new(&headset_group(), &catalog());
        let gk = e.group_key();
        e.on_grant(&grant(5, &gk, Trigger::Arrival), 0);
        let get = |hi, lo| l(&[0x11, 0xFF, 0x00, 0x0B, hi, lo]);
        let found = |fi| l(&[0x11, 0xFF, 0x00, 0x0B, fi]);
        let script = vec![
            (
                l(&[0x11, 0xFF, 0x00, 0x1B, 0x04, 0x02, 0x5A]),
                get(0x00, 0x05),
            ),
            (found(2), get(0x00, 0x03)),
            (found(3), get(0x10, 0x04)),
            (found(0), get(0x10, 0x00)),
            (found(0), get(0x10, 0x01)),
            (found(0), get(0x1F, 0x20)),
            (found(6), l(&[0x11, 0xFF, 0x02, 0x0B])),
            (
                l(&[0x11, 0xFF, 0x02, 0x0B, 29]),
                l(&[0x11, 0xFF, 0x02, 0x1B, 0]),
            ),
            (
                l(&[&[0x11, 0xFF, 0x02, 0x1B][..], &name[..16]].concat()),
                l(&[0x11, 0xFF, 0x02, 0x1B, 16]),
            ),
            (
                l(&[&[0x11, 0xFF, 0x02, 0x1B][..], &name[16..]].concat()),
                l(&[0x11, 0xFF, 0x02, 0x2B]),
            ),
            (
                l(&[0x11, 0xFF, 0x02, 0x2B, 8]),
                l(&[0x11, 0xFF, 0x03, 0x0B]),
            ),
            (
                l(&[0x11, 0xFF, 0x03, 0x0B, 0x01, 0xFF, 0xFF, 0xFF, 0xFF]),
                l(&[0x11, 0xFF, 0x06, 0x0B]),
            ),
        ];
        run(&mut e, &script, 1);
        e
    }

    /// The headset registered asleep, then switched on.
    fn check_off_headset_registered_then_updated(e: &mut HidppEngine, registered: &[Action]) {
        // Unit id FFFFFFFF is a placeholder: the key falls back to pid and index.
        let key = headset_key();
        assert_eq!(
            registered,
            [Action::Register {
                key: key.clone(),
                caps: pushing(),
                initial: DeviceUpdate {
                    name: Some("PRO X Wireless Gaming Headset".into()),
                    kind: Some(DeviceKind::Headset),
                    via: Some(Via::Usb),
                    presence: Some(Presence::Asleep),
                    ..update(key.clone(), Source::Query)
                },
                wpid: None,
            }]
        );
        assert_eq!(e.next_deadline(), None);

        let a = e.on_report(
            LONG_C,
            &l(&[0x11, 0xFF, 0x06, 0x00, 0x0E, 0xF4, 0x01]),
            5_000,
        );

        assert_eq!(
            a,
            vec![Action::Update(DeviceUpdate {
                level: Some(Level::Percent(68)),
                millivolts: Some(3828),
                charge: Some(Charge::Discharging),
                presence: Some(Presence::Online),
                ..update(key, Source::Push)
            })]
        );
    }

    #[test]
    fn direct_device_failed_first_read_registers_asleep() {
        let mut e = headset_off_until_read();
        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0xFF, 0x06, 0x0B, 0x05]), 2);
        check_off_headset_registered_then_updated(&mut e, &a);
    }

    #[test]
    fn direct_device_first_read_timeout_registers_asleep() {
        let mut e = headset_off_until_read();
        assert!(e.on_tick(4_000).is_empty());
        let a = e.on_tick(4_001);
        check_off_headset_registered_then_updated(&mut e, &a);
    }

    #[test]
    fn direct_device_asleep_errors_on_first_read_register_asleep() {
        for code in [0x04, 0x08, 0x09] {
            let mut e = headset_off_until_read();
            let a = e.on_report(LONG_C, &s(&[0x10, 0xFF, 0x8F, 0x06, 0x0B, code]), 2);
            assert!(
                matches!(
                    a.as_slice(),
                    [Action::Register { initial, .. }] if initial.presence == Some(Presence::Asleep)
                ),
                "{code:#04x}: {a:?}"
            );
        }
    }

    #[test]
    fn receiver_slot_failed_first_read_registers_nothing() {
        for (reply, tick) in [
            (Some(l(&[0x11, 1, 0xFF, 6, 0x1B, 0x05])), false),
            (Some(s(&[0x10, 1, 0x8F, 6, 0x1B, 0x09])), false),
            (None, true),
        ] {
            let mut e = receiver_pinging();
            for i in 2..=6 {
                e.on_report(SHORT_C, &s(&[0x10, i, 0x8F, 0x00, 0x1B, 0x08]), 2);
            }
            run(&mut e, &mouse_script(), 3);
            let a = match reply {
                Some(r) => e.on_report(LONG_C, &r, 4),
                None => e.on_tick(3 + 4_000),
            };
            assert!(a.is_empty(), "tick {tick}: {a:?}");
            assert_eq!(e.next_deadline(), None);
        }
    }

    #[test]
    fn link_lost_notification_marks_asleep() {
        let mut e = mouse_engine();

        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x40, 0x00, 0x00], 10);
        assert_eq!(a, vec![Action::Asleep(mouse_key())]);

        let a = e.on_report(SHORT_C, &[0x10, 0x03, 0x41, 0x04, 0x40, 0x00, 0x00], 11);
        assert!(a.is_empty(), "unknown index: {a:?}");
    }

    #[test]
    fn link_established_notification_requests_connected_grant() {
        let mut e = mouse_engine();
        let gk = e.group_key();

        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x00, 0x00], 10);
        assert_eq!(a, vec![Action::Connected(mouse_key())]);

        // Unknown index: a new pairing (or a device asleep at startup).
        let a = e.on_report(SHORT_C, &[0x10, 0x02, 0x41, 0x04, 0x00, 0x00, 0x00], 11);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);

        let a = e.on_grant(&grant(9, &gk, Trigger::Connected), 12);
        assert_eq!(
            a,
            vec![Action::Send {
                grant: GrantId(9),
                collection: SHORT_C,
                bytes: s(&[0x10, 2, 0x00, 0x1B, 0x00, 0x00, 0x5A]),
            }]
        );
        assert_eq!(e.next_deadline(), Some(4_012));
    }

    #[test]
    fn device_grant_reads_battery_once() {
        let mut e = mouse_engine();
        let a = e.on_grant(&grant(2, &mouse_key(), Trigger::RefreshNow), 100);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 6, 0x1B])]);

        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 55, 4, 1, 0]), 110);

        assert_eq!(
            a,
            vec![Action::Update(DeviceUpdate {
                level: Some(Level::Percent(55)),
                charge: Some(Charge::Charging),
                presence: Some(Presence::Online),
                ..update(mouse_key(), Source::Query)
            })]
        );
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn battery_read_errors_mark_asleep() {
        for reply in [
            l(&[0x11, 1, 0xFF, 6, 0x1B, 0x05]),
            s(&[0x10, 1, 0x8F, 6, 0x1B, 0x09]),
            s(&[0x10, 1, 0x8F, 6, 0x1B, 0x04]),
        ] {
            let mut e = mouse_engine();
            e.on_grant(&grant(2, &mouse_key(), Trigger::RefreshNow), 100);

            let a = e.on_report(LONG_C, &reply, 110);

            assert_eq!(a, vec![Action::Asleep(mouse_key())], "{reply:02X?}");
        }
    }

    #[test]
    fn ignores_replies_with_foreign_software_id() {
        let mut e = HidppEngine::new(&headset_group(), &catalog());
        let gk = e.group_key();
        e.on_grant(&grant(5, &gk, Trigger::Arrival), 0);

        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0x00, 0x1D, 0x04, 0x02, 0x5A]), 10);
        assert!(a.is_empty(), "{a:?}");
        assert_eq!(e.next_deadline(), Some(4_000));

        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0x00, 0x1B, 0x04, 0x02, 0x5A]), 20);
        assert_eq!(sent(&a), vec![l(&[0x11, 0xFF, 0x00, 0x0B, 0x00, 0x05])]);
    }

    #[test]
    fn read_timeout_marks_known_device_asleep() {
        // A direct device: silence means it is gone.
        let mut e = headset_engine();
        let a = e.on_grant(&grant(2, &headset_key(), Trigger::RefreshNow), 100);
        assert_eq!(sent(&a), vec![l(&[0x11, 0xFF, 0x08, 0x0B])]);
        assert_eq!(e.next_deadline(), Some(4_100));

        assert!(e.on_tick(4_099).is_empty());
        assert_eq!(e.on_tick(4_100), vec![Action::Asleep(headset_key())]);
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn receiver_read_timeout_retries_once_and_keeps_presence() {
        // A Connected grant (discovery budget, 10 s) outlives a 4 s timeout: the retry is real.
        let mut e = mouse_engine();
        let read = s(&[0x10, 1, 6, 0x1B]);
        let a = e.on_grant(&grant(2, &mouse_key(), Trigger::Connected), 100);
        assert_eq!(sent(&a), vec![read.clone()]);

        // A receiver reports a sleeping device with an error: silence is a lost reply.
        let a = e.on_tick(4_100);
        assert_eq!(
            a,
            vec![Action::Send {
                grant: GrantId(2),
                collection: SHORT_C,
                bytes: read,
            }]
        );
        assert_eq!(e.next_deadline(), Some(8_100));

        let a = e.on_tick(8_100);
        assert!(a.is_empty(), "no second retry, no Asleep: {a:?}");
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn receiver_read_timeout_under_short_read_grant_is_not_retried() {
        // Resume/RefreshNow grants last Policy::read_ms (3 s): expired when a 4 s read times out.
        let mut e = mouse_engine();
        let refresh = Grant {
            expires_ms: 100 + 3_000,
            max_requests: 4,
            ..grant(2, &mouse_key(), Trigger::RefreshNow)
        };
        let a = e.on_grant(&refresh, 100);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 6, 0x1B])]);

        let a = e.on_tick(4_100);

        assert!(a.is_empty(), "no retry, no Asleep: {a:?}");
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn receiver_read_retry_needs_budget() {
        let mut e = mouse_engine();
        let one = Grant {
            max_requests: 1,
            ..grant(2, &mouse_key(), Trigger::RefreshNow)
        };
        e.on_grant(&one, 100);

        assert!(e.on_tick(4_100).is_empty());
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn slow_receiver_slot_ping_continues_discovery() {
        // Owner trace 2026-11-23: an idle, linked Superlight answered the slot-1 ping
        // after 443, 937 and 1 288 ms.
        let mut e = receiver_pinging();
        for i in 2..=6 {
            e.on_report(SHORT_C, &s(&[0x10, i, 0x8F, 0x00, 0x1B, 0x08]), 2);
        }
        let late = 1 + 1_288;
        assert!(e.on_tick(late).is_empty());
        assert_eq!(e.next_deadline(), Some(4_001));

        run(&mut e, &mouse_script(), late);
        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), late + 1);

        assert!(
            matches!(a.as_slice(), [Action::Register { key, .. }] if *key == mouse_key()),
            "{a:?}"
        );
    }

    #[test]
    fn silent_receiver_slot_ping_is_dropped_after_4_s() {
        let mut e = receiver_pinging();
        for i in 2..=6 {
            e.on_report(SHORT_C, &s(&[0x10, i, 0x8F, 0x00, 0x1B, 0x08]), 2);
        }
        assert!(e.on_tick(4_000).is_empty());
        assert_eq!(e.next_deadline(), Some(4_001));

        assert!(e.on_tick(4_001).is_empty());
        assert_eq!(e.next_deadline(), None);
        // A reply after the timeout is a sign of life: rediscover through a Connected grant.
        let a = e.on_report(LONG_C, &l(&[0x11, 1, 0x00, 0x1B, 0x04, 0x02, 0x5A]), 4_002);
        assert_eq!(a, vec![Action::Connected(e.group_key())]);
    }

    #[test]
    fn receiver_own_request_times_out_after_1_s() {
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        let a = e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);
        assert_eq!(sent(&a), vec![s(&[0x10, 0xFF, 0x81, 0x00])]);
        assert_eq!(e.next_deadline(), Some(1_000));

        // No answer from the receiver: still ask it to report its slots.
        let a = e.on_tick(1_000);
        assert_eq!(sent(&a), vec![notify_devices()]);
        assert_eq!(e.next_deadline(), Some(2_000));

        // No answer again: fall back to pinging every slot, at the device timeout.
        let a = e.on_tick(2_000);
        assert_eq!(sent(&a).len(), 6, "{a:?}");
        assert_eq!(e.next_deadline(), Some(6_000));
    }

    #[test]
    fn receiver_start_asks_for_link_notifications_instead_of_pinging() {
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);

        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), 1);

        assert_eq!(
            a,
            vec![Action::Send {
                grant: GrantId(1),
                collection: SHORT_C,
                bytes: notify_devices(),
            }]
        );
        // The acknowledgement asks nothing more: the receiver now reports its slots.
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x80, 0x02]), 2);
        assert!(a.is_empty(), "{a:?}");
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn slot_link_notifications_drive_discovery_of_linked_slots_only() {
        let mut e = receiver_notified();
        let gk = e.group_key();

        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x93, 0x40], 3);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);
        // Paired, link not established: nothing to do until it links.
        let a = e.on_report(SHORT_C, &[0x10, 0x02, 0x41, 0x04, 0x40, 0x8A, 0x40], 4);
        assert!(a.is_empty(), "{a:?}");

        let a = e.on_grant(&grant(2, &gk, Trigger::Connected), 5);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x00, 0x1B, 0x00, 0x00, 0x5A])]);
        run(&mut e, &mouse_script(), 6);
        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), 7);
        assert!(
            matches!(a.as_slice(), [Action::Register { key, .. }] if *key == mouse_key()),
            "{a:?}"
        );
    }

    #[test]
    fn notify_devices_failure_falls_back_to_ping_sweep() {
        let pings: Vec<Vec<u8>> = (1..=6)
            .map(|i| s(&[0x10, i, 0x00, 0x1B, 0x00, 0x00, 0x5A]))
            .collect();
        for code in [0x02, 0x09] {
            let mut e = HidppEngine::new(&receiver_group(), &catalog());
            let gk = e.group_key();
            e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);
            e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), 1);
            let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x8F, 0x80, 0x02, code]), 2);
            assert_eq!(sent(&a), pings, "{code:#04x}");
        }
        // Timeout: same fallback.
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);
        e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), 1);
        assert!(e.on_tick(1_000).is_empty());
        assert_eq!(sent(&e.on_tick(1_001)), pings);
    }

    #[test]
    fn late_slot_reply_counts_as_link_up() {
        let mut e = receiver_notified();
        let gk = e.group_key();
        e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x93, 0x40], 3);
        let a = e.on_grant(&grant(2, &gk, Trigger::Connected), 10);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x00, 0x1B, 0x00, 0x00, 0x5A])]);
        assert!(e.on_tick(10 + 4_000).is_empty(), "ping dropped quietly");

        // The mouse answers 4.5 s after the ping.
        let late = l(&[0x11, 1, 0x00, 0x1B, 0x04, 0x02, 0x5A]);
        let a = e.on_report(LONG_C, &late, 10 + 4_500);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);
        // Already pending: a second late frame asks nothing more.
        assert!(e.on_report(LONG_C, &late, 10 + 4_501).is_empty());

        let a = e.on_grant(&grant(3, &gk, Trigger::Connected), 10 + 4_502);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x00, 0x1B, 0x00, 0x00, 0x5A])]);
        run(&mut e, &mouse_script(), 10 + 4_503);
        let a = e.on_report(LONG_C, &l(&[0x11, 1, 6, 0x1B, 34, 4, 0, 0]), 10 + 4_504);
        assert!(
            matches!(a.as_slice(), [Action::Register { key, .. }] if *key == mouse_key()),
            "{a:?}"
        );

        // Registered: further late frames from the slot are ignored.
        assert!(e.on_report(LONG_C, &late, 10 + 4_505).is_empty());
    }

    #[test]
    fn late_reply_rediscovers_a_slot_once_until_rearmed() {
        let ping = s(&[0x10, 1, 0x00, 0x1B, 0x00, 0x00, 0x5A]);
        let late = l(&[0x11, 1, 0x00, 0x1B, 0x04, 0x02, 0x5A]);
        let mut e = receiver_notified();
        let gk = e.group_key();
        e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x93, 0x40], 3);
        assert_eq!(
            sent(&e.on_grant(&grant(2, &gk, Trigger::Connected), 10)),
            vec![ping.clone()]
        );
        e.on_tick(10 + 4_000);

        // First late reply: one rediscovery.
        let a = e.on_report(LONG_C, &late, 4_100);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);
        assert_eq!(
            sent(&e.on_grant(&grant(3, &gk, Trigger::Connected), 4_101)),
            vec![ping.clone()]
        );
        e.on_tick(4_101 + 4_000);

        // Its ping timed out too: a second late reply asks nothing more.
        let a = e.on_report(LONG_C, &late, 8_200);
        assert!(a.is_empty(), "{a:?}");

        // A real 0x41 for the slot re-arms it.
        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x93, 0x40], 8_201);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);
        assert_eq!(
            sent(&e.on_grant(&grant(4, &gk, Trigger::Connected), 8_202)),
            vec![ping]
        );
        e.on_tick(8_202 + 4_000);
        let a = e.on_report(LONG_C, &late, 12_300);
        assert_eq!(a, vec![Action::Connected(gk)]);
    }

    #[test]
    fn late_frames_without_our_software_id_are_not_link_ups() {
        let mut e = receiver_notified();
        // Battery broadcast (software id 0) and a reply to another program (0x0D).
        for frame in [
            l(&[0x11, 1, 6, 0x00, 40, 4, 0, 0]),
            l(&[0x11, 1, 0x00, 0x1D, 0x04, 0x02, 0x5A]),
        ] {
            let a = e.on_report(LONG_C, &frame, 3);
            assert!(a.is_empty(), "{frame:02X?}: {a:?}");
        }
    }

    #[test]
    fn placeholder_unit_id_falls_back_to_pid_and_index() {
        let mut e = headset_off_until_read();

        let a = e.on_report(LONG_C, &l(&[0x11, 0xFF, 0x06, 0x0B, 0x0E, 0xF4, 0x01]), 2);

        let [Action::Register { key, initial, .. }] = a.as_slice() else {
            panic!("{a:?}");
        };
        assert_eq!(*key, DeviceKey("logi:0ABA:255".into()));
        assert_eq!(initial.level, Some(Level::Percent(68)));
    }

    #[test]
    fn unpair_notification_unregisters_and_next_link_up_rediscovers() {
        let mut e = mouse_engine();
        let gk = e.group_key();

        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x40, 0x02, 0x00, 0x00, 0x00], 10);
        assert_eq!(a, vec![Action::Unregister(mouse_key())]);

        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x00, 0x00], 11);
        assert_eq!(a, vec![Action::Connected(gk.clone())]);
        let a = e.on_grant(&grant(9, &gk, Trigger::Connected), 12);
        assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x00, 0x1B, 0x00, 0x00, 0x5A])]);
    }

    #[test]
    fn unpair_notification_drops_conversation() {
        let mut e = mouse_engine();
        e.on_grant(&grant(2, &mouse_key(), Trigger::RefreshNow), 100);

        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x40, 0x02, 0x00, 0x00, 0x00], 110);

        assert_eq!(a, vec![Action::Unregister(mouse_key())]);
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn unpair_frame_on_receiver_index_keeps_arrival_conversation() {
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        e.on_grant(&grant(1, &gk, Trigger::Arrival), 0);

        let a = e.on_report(SHORT_C, &[0x10, 0xFF, 0x40, 0x02, 0x00, 0x00, 0x00], 1);

        assert!(a.is_empty(), "{a:?}");
        assert_eq!(e.next_deadline(), Some(1_000));
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), 2);
        assert_eq!(sent(&a), vec![notify_devices()]);
    }

    #[test]
    fn unknown_device_error_on_read_marks_asleep() {
        // Hardware: an idle Superlight behind a LIGHTSPEED receiver answers 0x08.
        let mut e = mouse_engine();
        e.on_grant(&grant(2, &mouse_key(), Trigger::RefreshNow), 100);

        let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x8F, 6, 0x1B, 0x08]), 110);

        assert_eq!(a, vec![Action::Asleep(mouse_key())]);
        // Still known: link-up is a Connected on the device, not a rediscovery.
        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x00, 0x00], 120);
        assert_eq!(a, vec![Action::Connected(mouse_key())]);
    }

    #[test]
    fn unknown_device_error_on_hidpp10_link_up_marks_asleep() {
        let key = DeviceKey("logi:C547:1".into());
        let mut e = hidpp10_engine();
        e.on_grant(&grant(3, &key, Trigger::Connected), 100);

        let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x8F, 0x81, 0x00, 0x08]), 110);

        assert_eq!(a, vec![Action::Asleep(key.clone())]);
        let a = e.on_report(SHORT_C, &[0x10, 0x01, 0x41, 0x04, 0x00, 0x00, 0x00], 120);
        assert_eq!(a, vec![Action::Connected(key)]);
    }

    /// Receiver with a HID++ 1.0 device (register 0x0D) on index 1 and no conversation left.
    fn hidpp10_engine() -> HidppEngine {
        let mut e = receiver_pinging();
        for i in 2..=6 {
            e.on_report(SHORT_C, &s(&[0x10, i, 0x8F, 0x00, 0x1B, 0x08]), 2);
        }
        for reply in [
            s(&[0x10, 1, 0x8F, 0x00, 0x1B, 0x01]),
            s(&[0x10, 1, 0x81, 0x00, 0x10]),
        ] {
            e.on_report(SHORT_C, &reply, 3);
        }
        let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x81, 0x0D, 50, 0x00, 0x00]), 4);
        assert!(matches!(a.as_slice(), [Action::Register { .. }]), "{a:?}");
        assert_eq!(e.next_deadline(), None);
        e
    }

    #[test]
    fn connected_grant_reenables_hidpp10_battery_flag() {
        let key = DeviceKey("logi:C547:1".into());
        let get = s(&[0x10, 1, 0x81, 0x00]);
        let read = s(&[0x10, 1, 0x81, 0x0D]);
        for (flags, write) in [
            (
                [0x00, 0x00, 0x00],
                Some(s(&[0x10, 1, 0x80, 0x00, 0x10, 0x00, 0x00])),
            ),
            (
                [0x01, 0x02, 0x03],
                Some(s(&[0x10, 1, 0x80, 0x00, 0x11, 0x02, 0x03])),
            ),
            ([0x10, 0x00, 0x00], None),
        ] {
            let mut e = hidpp10_engine();
            let a = e.on_grant(&grant(3, &key, Trigger::Connected), 100);
            assert_eq!(sent(&a), vec![get.clone()]);

            let a = e.on_report(
                SHORT_C,
                &s(&[&[0x10, 1, 0x81, 0x00][..], &flags].concat()),
                110,
            );
            match write {
                Some(set) => {
                    assert_eq!(sent(&a), vec![set], "{flags:?}");
                    let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x80, 0x00]), 120);
                    assert_eq!(sent(&a), vec![read.clone()], "{flags:?}");
                }
                None => assert_eq!(sent(&a), vec![read.clone()], "{flags:?}"),
            }

            let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x81, 0x0D, 55, 0x00, 0x40]), 130);
            assert_eq!(
                a,
                vec![Action::Update(DeviceUpdate {
                    level: Some(Level::Percent(55)),
                    charge: Some(Charge::Charging),
                    presence: Some(Presence::Online),
                    ..update(key.clone(), Source::Query)
                })]
            );
        }
    }

    #[test]
    fn refresh_grant_on_hidpp10_device_reads_only() {
        let key = DeviceKey("logi:C547:1".into());
        for trigger in [Trigger::RefreshNow, Trigger::Resume] {
            let mut e = hidpp10_engine();
            let a = e.on_grant(&grant(3, &key, trigger), 100);
            assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x81, 0x0D])], "{trigger:?}");
        }
    }

    #[test]
    fn asleep_errors_mid_discovery_stop_asking() {
        for code in [0x04, 0x08, 0x09] {
            let mut e = receiver_pinging();
            for i in 2..=6 {
                e.on_report(SHORT_C, &s(&[0x10, i, 0x8F, 0x00, 0x1B, 0x08]), 2);
            }
            let a = e.on_report(LONG_C, &l(&[0x11, 1, 0x00, 0x1B, 0x04, 0x02, 0x5A]), 3);
            assert_eq!(sent(&a), vec![s(&[0x10, 1, 0x00, 0x0B, 0x00, 0x05])]);

            let a = e.on_report(SHORT_C, &s(&[0x10, 1, 0x8F, 0x00, 0x0B, code]), 4);

            assert!(a.is_empty(), "{code:#04x}: {a:?}");
            assert_eq!(e.next_deadline(), None, "{code:#04x}");
        }
    }

    #[test]
    fn exhausted_grant_ends_conversation_quietly() {
        let mut e = HidppEngine::new(&receiver_group(), &catalog());
        let gk = e.group_key();
        let small = Grant {
            max_requests: 2,
            ..grant(1, &gk, Trigger::Arrival)
        };
        e.on_grant(&small, 0);
        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x81, 0x00]), 1);
        assert_eq!(sent(&a).len(), 1);

        let a = e.on_report(SHORT_C, &s(&[0x10, 0xFF, 0x80, 0x00]), 2);

        assert!(
            a.is_empty(),
            "no budget left for the register 0x02 write: {a:?}"
        );
        assert_eq!(e.next_deadline(), None);
    }

    #[test]
    fn pushed_events_never_send() {
        let frames = fixture_inputs();
        assert!(frames.len() > 20);
        let engines = [
            HidppEngine::new(&receiver_group(), &catalog()),
            HidppEngine::new(&headset_group(), &catalog()),
            mouse_engine(),
            headset_engine(),
        ];
        for mut e in engines {
            for (t, f) in (100..).zip(&frames) {
                let c = if f.len() == 7 { SHORT_C } else { LONG_C };
                let a = e.on_report(c, f, t);
                assert!(sent(&a).is_empty(), "{f:02X?} -> {a:?}");
            }
            assert!(e.on_tick(1_000_000).is_empty());
        }
    }
}
