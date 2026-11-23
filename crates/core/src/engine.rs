//! Protocol engines: one per HID group, sans-IO.
//!
//! An engine turns reports into [`Action`]s and may only write to a device
//! under a [`Grant`] handed to it by the hub. It never reads a clock or
//! touches a handle; the hub decides what reaches the device.

use crate::hid::CollectionId;
use crate::model::{DeviceKey, DeviceUpdate};
use crate::scheduler::{Capabilities, Grant, GrantId, Trigger};
use crate::time::Ms;

/// What an engine asks the hub to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Write `bytes` to `collection`, charged to `grant`; dropped if the grant does not admit it.
    Send {
        grant: GrantId,
        collection: CollectionId,
        bytes: Vec<u8>,
        timeout_ms: u32,
    },
    /// Start tracking a device discovered behind this engine's group.
    Register {
        key: DeviceKey,
        caps: Capabilities,
        initial: DeviceUpdate,
    },
    /// The device is gone from this group (e.g. unpaired); its entry goes offline.
    Unregister(DeviceKey),
    /// New reading or presence for a registered device.
    Update(DeviceUpdate),
    /// The device sleeps or lost its link: stop asking it.
    Asleep(DeviceKey),
    /// Ask the scheduler for a grant on `key` (a device or this engine's group key).
    WantGrant { key: DeviceKey, trigger: Trigger },
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
