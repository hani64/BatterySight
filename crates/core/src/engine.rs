//! Protocol engines: one per HID group, sans-IO.
//!
//! An engine turns reports into [`Action`]s and may only write to a device
//! under a [`Grant`] handed to it by the hub. It never reads a clock or
//! touches a handle; the hub decides what reaches the device.

use crate::engines::hidpp::frame::{FIRST_REGISTER_SUB_ID, LONG, SHORT, SW_ID};
use crate::hid::{CollectionId, CollectionRole, ReportKind, ReportOrigin};
use crate::model::{DeviceKey, DeviceUpdate, Presence};
use crate::scheduler::{Capabilities, Grant, GrantId};
use crate::time::Ms;

/// Identifies a feature SET within its grant. Successive SETs in the same
/// transaction are issued on different ticks, and each has its own completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureSetId {
    pub grant: GrantId,
    pub issued_ms: Ms,
}

/// What an engine asks the hub to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Write `bytes` to `collection`, charged to `grant`; dropped if the grant does not admit it.
    /// For feature operations, `deadline` is the absolute end of this transaction. Output
    /// reports use `None`; their engine tracks any answer in its own state.
    Send {
        grant: GrantId,
        collection: CollectionId,
        kind: ReportKind,
        bytes: Vec<u8>,
        deadline: Option<Ms>,
    },
    /// Start tracking a device discovered behind this engine's group. `wpid` is the
    /// wireless product id of a device behind a receiver, when the receiver reported one.
    /// The hub looks a device registered `via` a receiver up by `wpid` only (none: the
    /// vendor family), any other device by the group's USB product id.
    Register {
        key: DeviceKey,
        caps: Capabilities,
        initial: DeviceUpdate,
        wpid: Option<u16>,
    },
    /// This group no longer serves the device (e.g. unpaired); with no other
    /// group serving it, its entry goes offline.
    Unregister(DeviceKey),
    /// New reading or presence for a registered device.
    Update(DeviceUpdate),
    /// Presence without a new battery sample or a grant. Offline clears charge.
    Presence {
        key: DeviceKey,
        presence: Presence,
    },
    /// The device sleeps or lost its link: stop asking it.
    Asleep(DeviceKey),
    /// A link to `key` (a device or this engine's group key) was established: the hub
    /// requests [`Trigger::Connected`](crate::scheduler::Trigger::Connected) for it.
    Connected(DeviceKey),
    /// A fresh device-originated sign of activity, not a battery reading. Only
    /// accepted from the current owner's routed input report.
    Awake(DeviceKey),
    /// The group's discovery registered nothing and is due again: the hub requests
    /// [`Trigger::Rediscovery`](crate::scheduler::Trigger::Rediscovery) on the group key
    /// while the group has no registered device.
    Rediscover,
    /// The group's discovery ended without registering a device. The engine reports
    /// [`Action::Rediscover`] at `retry_at`; `None` means it gave up until the next
    /// arrival, resume or refresh.
    DiscoveryFailed { retry_at: Option<Ms> },
}

/// A documented payload, including its report ID when numbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BytePattern {
    Exact(&'static [u8]),
    ZeroPadded(&'static [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidppTarget {
    Receiver,
    Device,
}

/// The HID++ destination, with the engine's local key if the device is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidppDestination<'a> {
    Receiver,
    Device(Option<&'a DeviceKey>),
}

impl HidppDestination<'_> {
    fn target(self) -> HidppTarget {
        match self {
            Self::Receiver => HidppTarget::Receiver,
            Self::Device(_) => HidppTarget::Device,
        }
    }
}

pub(crate) enum WriteDestination<'a> {
    Collection,
    Group,
    Device(&'a DeviceKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidppFeatureParams {
    Exact([u8; 3]),
    RootLookup(&'static [u16]),
    NameOffset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidppRegisterParams {
    Exact([u8; 3]),
    EnableNotification { byte: usize, mask: u8 },
}

/// The complete forms of commands an engine may issue. A grant is still required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRule {
    Bytes {
        role: CollectionRole,
        kind: ReportKind,
        wire_len: usize,
        pattern: BytePattern,
    },
    /// HID++ 2.0 output, addressed to a device (not the receiver itself).
    HidppFeature {
        role: CollectionRole,
        feature: u16,
        function: u8,
        params: HidppFeatureParams,
    },
    /// HID++ 1.0 output.
    HidppRegister {
        role: CollectionRole,
        target: HidppTarget,
        sub_id: u8,
        address: u8,
        params: HidppRegisterParams,
    },
}

/// The HID++ payload following the report ID: destination, command, address, and
/// three parameters. Our long requests use the same parameters as short ones.
fn hidpp_fields(kind: ReportKind, bytes: &[u8]) -> Option<(u8, u8, u8, [u8; 3])> {
    if kind != ReportKind::Output {
        return None;
    }
    let &[report, index, command, address, a, b, c, ref padding @ ..] = bytes else {
        return None;
    };
    match (report, bytes.len()) {
        (SHORT, 7) => {}
        (LONG, 20) if padding.iter().all(|&byte| byte == 0) => {}
        _ => return None,
    }
    Some((index, command, address, [a, b, c]))
}

pub(crate) fn write_allowed<'a>(
    engine: &'a dyn Engine,
    role: CollectionRole,
    kind: ReportKind,
    bytes: &[u8],
    grant: GrantId,
) -> Option<WriteDestination<'a>> {
    let mut hidpp = None;
    let mut feature_id = None;
    for rule in engine.allowed_writes() {
        let candidate_role = match *rule {
            WriteRule::Bytes {
                role: candidate,
                kind: wanted,
                wire_len,
                pattern,
            } => {
                if candidate == role
                    && wanted == kind
                    && bytes.len() == wire_len
                    && match pattern {
                        BytePattern::Exact(wanted) => bytes == wanted,
                        BytePattern::ZeroPadded(prefix) => bytes
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.iter().all(|&byte| byte == 0)),
                    }
                {
                    return Some(WriteDestination::Collection);
                }
                continue;
            }
            WriteRule::HidppFeature { role, .. } | WriteRule::HidppRegister { role, .. } => role,
        };
        if candidate_role != role {
            continue;
        }
        let Some((index, command, address, actual, destination)) = *hidpp.get_or_insert_with(|| {
            let (index, command, address, actual) = hidpp_fields(kind, bytes)?;
            let destination = engine.hidpp_destination(index)?;
            Some((index, command, address, actual, destination))
        }) else {
            continue;
        };
        let matched = match *rule {
            WriteRule::HidppFeature {
                feature,
                function,
                params,
                ..
            } if destination.target() == HidppTarget::Device
                && command < FIRST_REGISTER_SUB_ID
                && address & 0x0F == SW_ID
                && address >> 4 == function
                && *feature_id.get_or_insert_with(|| engine.feature_id_for_index(index, command))
                    == Some(feature) =>
            {
                match params {
                    HidppFeatureParams::Exact(expected) => actual == expected,
                    HidppFeatureParams::RootLookup(ids) => {
                        actual[2] == 0 && ids.contains(&u16::from_be_bytes([actual[0], actual[1]]))
                    }
                    HidppFeatureParams::NameOffset => {
                        actual[1..] == [0, 0]
                            && engine.hidpp_name_offset(grant, index) == Some(actual[0])
                    }
                }
            }
            WriteRule::HidppRegister {
                target: wanted,
                sub_id,
                address: register,
                params,
                ..
            } if destination.target() == wanted && command == sub_id && address == register => {
                match params {
                    HidppRegisterParams::Exact(expected) => actual == expected,
                    HidppRegisterParams::EnableNotification { byte, mask } => engine
                        .hidpp_notification_read(grant, index)
                        .is_some_and(|mut read| {
                            let Some(flag) = read.get_mut(byte) else {
                                return false;
                            };
                            if *flag & mask != 0 {
                                return false;
                            }
                            *flag |= mask;
                            actual == read
                        }),
                }
            }
            _ => false,
        };
        if matched {
            return Some(match destination {
                HidppDestination::Device(Some(key)) => WriteDestination::Device(key),
                HidppDestination::Device(None) | HidppDestination::Receiver => {
                    WriteDestination::Group
                }
            });
        }
    }
    None
}

pub trait Engine {
    /// Scheduler key for conversations with the group itself (e.g. a receiver).
    fn group_key(&self) -> DeviceKey;

    /// Arrival, resume and refresh can query the group itself even if its radio
    /// device is not registered or linked. Used by dongles answering for themselves.
    fn group_one_shots(&self) -> bool {
        false
    }

    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action>;
    fn on_report(
        &mut self,
        collection: CollectionId,
        origin: ReportOrigin,
        bytes: &[u8],
        now: Ms,
    ) -> Vec<Action>;
    /// Transport confirmed this SET; it is not an input report or a GET reply.
    fn on_feature_set_complete(
        &mut self,
        _collection: CollectionId,
        _id: FeatureSetId,
        _now: Ms,
    ) -> Vec<Action> {
        Vec::new()
    }
    fn on_tick(&mut self, now: Ms) -> Vec<Action>;
    /// When `on_tick` next needs to run, if ever.
    fn next_deadline(&self) -> Option<Ms>;

    /// No commands are allowed unless the engine declares them explicitly.
    fn allowed_writes(&self) -> &'static [WriteRule] {
        &[]
    }

    fn collection_role(&self, _collection: CollectionId) -> Option<CollectionRole> {
        None
    }

    /// Resolves a valid HID++ wire destination within this group.
    fn hidpp_destination(&self, _device_index: u8) -> Option<HidppDestination<'_>> {
        None
    }

    /// Resolves a feature only from the mapping for this destination.
    fn feature_id_for_index(&self, _device_index: u8, _feature_index: u8) -> Option<u16> {
        None
    }

    /// The current offset for an in-flight HID++ name request.
    fn hidpp_name_offset(&self, _grant: GrantId, _device_index: u8) -> Option<u8> {
        None
    }

    /// Unmodified flags from the matched read for an in-flight notification SET.
    fn hidpp_notification_read(&self, _grant: GrantId, _device_index: u8) -> Option<[u8; 3]> {
        None
    }
}
