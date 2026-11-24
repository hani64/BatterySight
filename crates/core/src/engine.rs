//! Protocol engines: one per HID group, sans-IO.
//!
//! An engine turns reports into [`Action`]s and may only write to a device
//! under a [`Grant`] handed to it by the hub. It never reads a clock or
//! touches a handle; the hub decides what reaches the device.

use crate::hid::CollectionId;
use crate::model::{DeviceKey, DeviceUpdate};
use crate::scheduler::{Capabilities, Grant, GrantId};
use crate::time::Ms;

/// What an engine asks the hub to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Write `bytes` to `collection`, charged to `grant`; dropped if the grant does not admit it.
    /// The engine keeps its own deadline for the answer (see [`Engine::next_deadline`]).
    Send {
        grant: GrantId,
        collection: CollectionId,
        bytes: Vec<u8>,
    },
    /// Start tracking a device discovered behind this engine's group.
    Register {
        key: DeviceKey,
        caps: Capabilities,
        initial: DeviceUpdate,
    },
    /// This group no longer serves the device (e.g. unpaired); with no other
    /// group serving it, its entry goes offline.
    Unregister(DeviceKey),
    /// New reading or presence for a registered device.
    Update(DeviceUpdate),
    /// The device sleeps or lost its link: stop asking it.
    Asleep(DeviceKey),
    /// A link to `key` (a device or this engine's group key) was established.
    /// The only grant an engine can ask for: the hub requests
    /// [`Trigger::Connected`](crate::scheduler::Trigger::Connected) for it.
    Connected(DeviceKey),
}

pub trait Engine {
    /// Scheduler key for conversations with the group itself (e.g. a receiver).
    fn group_key(&self) -> DeviceKey;
    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action>;
    fn on_report(&mut self, collection: CollectionId, bytes: &[u8], now: Ms) -> Vec<Action>;
    fn on_tick(&mut self, now: Ms) -> Vec<Action>;
    /// When `on_tick` next needs to run, if ever.
    fn next_deadline(&self) -> Option<Ms>;
}
