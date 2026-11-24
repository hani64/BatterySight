//! Decides when anything may be sent to a device.
//!
//! Engines never query on their own: they ask for a [`Grant`] with the event
//! that justifies it, then call [`Scheduler::admit_send`] before every report
//! they write. Time only moves through the `now` arguments; nothing here runs
//! on a timer or produces grants by itself.

use std::collections::HashMap;

use crate::model::DeviceKey;
use crate::time::Ms;

/// Event that justifies asking a device. Opening the menu is deliberately not
/// a trigger: it shows last known values and never queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// Device appeared (app start counts as arrival).
    Arrival,
    /// Connect notification from the receiver/link.
    Connected,
    /// System resume.
    Resume,
    /// Explicit "Refresh now" command.
    RefreshNow,
    /// User or audio activity observed on the device. Refused while the device is asleep
    /// and never wakes it: a headset's dongle plays audio with the headset off, so activity
    /// proves nothing about the device itself.
    Activity,
}

/// What an engine declares about a registered device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The device reports battery on its own; asking after activity is pointless.
    pub pushes_battery: bool,
    /// A query wakes the device: only observed activity may justify one.
    pub query_wakes: bool,
    /// Minimum time since the last granted request (any trigger) or noted read
    /// ([`Scheduler::note_read`]) before an `Activity` grant; the other triggers ignore it.
    /// Zero means no `Activity` grant at all.
    pub min_query_gap_ms: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GrantId(pub u32);

/// Permission to send at most `max_requests` reports to `subject` until `expires_ms`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub id: GrantId,
    pub subject: DeviceKey,
    pub trigger: Trigger,
    pub expires_ms: Ms,
    pub max_requests: u16,
}

/// Size and lifetime of grants. Discovery (`Arrival`, `Connected`) may need to
/// enumerate features, so its budget is a cap, not a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub discovery_budget: u16,
    pub discovery_ms: Ms,
    pub read_budget: u16,
    pub read_ms: Ms,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            discovery_budget: 96,
            discovery_ms: 10_000,
            read_budget: 4,
            read_ms: 3_000,
        }
    }
}

/// Per-device history the rules depend on.
#[derive(Debug)]
struct DeviceSlot {
    caps: Capabilities,
    last_grant_ms: Option<Ms>,
    arrival_used: bool,
    asleep: bool,
}

/// What is left of an issued grant.
#[derive(Debug)]
struct OpenGrant {
    subject: DeviceKey,
    expires_ms: Ms,
    remaining: u16,
}

#[derive(Debug)]
pub struct Scheduler {
    policy: Policy,
    devices: HashMap<DeviceKey, DeviceSlot>,
    grants: HashMap<GrantId, OpenGrant>,
    /// Id of the next grant; ids are never reused.
    next_id: u32,
}

impl Scheduler {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            devices: HashMap::new(),
            grants: HashMap::new(),
            next_id: 1,
        }
    }

    /// Starts tracking `key`. Registering a known key only updates its
    /// capabilities; its history (arrival used, last grant, asleep) is kept.
    pub fn register(&mut self, key: DeviceKey, caps: Capabilities) {
        self.devices
            .entry(key)
            .and_modify(|slot| slot.caps = caps)
            .or_insert(DeviceSlot {
                caps,
                last_grant_ms: None,
                arrival_used: false,
                asleep: false,
            });
    }

    /// Forgets `key` and revokes its open grants.
    pub fn unregister(&mut self, key: &DeviceKey) {
        self.devices.remove(key);
        self.grants.retain(|_, grant| grant.subject != *key);
    }

    /// Grants permission to query `key` for `trigger`, or `None` if any rule refuses it.
    pub fn request(&mut self, key: &DeviceKey, trigger: Trigger, now: Ms) -> Option<Grant> {
        // A clock that went backwards counts as no time passed.
        if trigger == Trigger::Activity && self.activity_ready_at(key).is_none_or(|at| now < at) {
            return None;
        }
        let slot = self.devices.get_mut(key)?;
        let caps = slot.caps;
        let allowed = match trigger {
            Trigger::Arrival => !caps.query_wakes && !slot.arrival_used,
            Trigger::Connected => !caps.query_wakes,
            Trigger::Resume | Trigger::RefreshNow => !caps.query_wakes && !slot.asleep,
            // Checked above.
            Trigger::Activity => true,
        };
        if !allowed {
            return None;
        }

        slot.last_grant_ms = Some(now);
        // Only a push, a link-up or a successful read wakes an asleep device.
        match trigger {
            Trigger::Arrival => slot.arrival_used = true,
            Trigger::Connected => slot.asleep = false,
            Trigger::Resume | Trigger::RefreshNow | Trigger::Activity => {}
        }

        let (max_requests, duration) = match trigger {
            Trigger::Arrival | Trigger::Connected => {
                (self.policy.discovery_budget, self.policy.discovery_ms)
            }
            Trigger::Resume | Trigger::RefreshNow | Trigger::Activity => {
                (self.policy.read_budget, self.policy.read_ms)
            }
        };
        let expires_ms = now.saturating_add(duration);
        let id = GrantId(self.next_id);
        self.next_id += 1;

        // Lazily drop grants that can no longer admit anything.
        self.grants.retain(|_, grant| now <= grant.expires_ms);
        self.grants.insert(
            id,
            OpenGrant {
                subject: key.clone(),
                expires_ms,
                remaining: max_requests,
            },
        );
        Some(Grant {
            id,
            subject: key.clone(),
            trigger,
            expires_ms,
            max_requests,
        })
    }

    /// Consumes one send from `grant`; `false` if it is unknown, expired or exhausted.
    pub fn admit_send(&mut self, grant: GrantId, now: Ms) -> bool {
        let Some(open) = self.grants.get_mut(&grant) else {
            return false;
        };
        if now > open.expires_ms || open.remaining == 0 {
            self.grants.remove(&grant);
            return false;
        }
        open.remaining -= 1;
        if open.remaining == 0 {
            self.grants.remove(&grant);
        }
        true
    }

    /// Device answered "asleep/unlinked" or stayed silent: stop asking it.
    pub fn mark_asleep(&mut self, key: &DeviceKey) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.asleep = true;
        }
    }

    /// A push or other sign of life arrived.
    pub fn mark_awake(&mut self, key: &DeviceKey) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.asleep = false;
        }
    }

    pub fn is_asleep(&self, key: &DeviceKey) -> bool {
        self.devices.get(key).is_some_and(|slot| slot.asleep)
    }

    /// When `key` may next get an [`Trigger::Activity`] grant: `min_query_gap_ms` after its
    /// last grant or read (at once without one). `None` while no activity can justify a
    /// query: the device is unknown, pushes its battery, is asleep, or declares no gap (the
    /// gap is the only limit on activity reads, so a device without one gets none).
    pub fn activity_ready_at(&self, key: &DeviceKey) -> Option<Ms> {
        let slot = self.devices.get(key)?;
        let gap = slot.caps.min_query_gap_ms;
        if slot.caps.pushes_battery || slot.asleep || gap == 0 {
            return None;
        }
        Some(
            slot.last_grant_ms
                .map_or(0, |last| last.saturating_add(gap)),
        )
    }

    /// `key` was read without a grant of its own (its discovery's first battery read, or a
    /// reading it pushed): the activity gap counts from `now`, unless a later grant already
    /// restarted it.
    pub fn note_read(&mut self, key: &DeviceKey, now: Ms) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.last_grant_ms = Some(slot.last_grant_ms.map_or(now, |last| last.max(now)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TRIGGERS: [Trigger; 5] = [
        Trigger::Arrival,
        Trigger::Connected,
        Trigger::Resume,
        Trigger::RefreshNow,
        Trigger::Activity,
    ];

    fn key() -> DeviceKey {
        DeviceKey("hid:046d:c547:mouse".to_string())
    }

    fn caps(pushes_battery: bool, query_wakes: bool, min_query_gap_ms: Ms) -> Capabilities {
        Capabilities {
            pushes_battery,
            query_wakes,
            min_query_gap_ms,
        }
    }

    fn scheduler_with(caps: Capabilities) -> Scheduler {
        let mut s = Scheduler::new(Policy::default());
        s.register(key(), caps);
        s
    }

    #[test]
    fn arrival_grants_once() {
        let mut s = scheduler_with(caps(false, false, 0));
        assert!(s.request(&key(), Trigger::Arrival, 0).is_some());
        assert_eq!(s.request(&key(), Trigger::Arrival, 10), None);

        // Registering again without unregistering keeps the history.
        s.register(key(), caps(false, false, 0));
        assert_eq!(s.request(&key(), Trigger::Arrival, 20), None);

        s.unregister(&key());
        s.register(key(), caps(false, false, 0));
        assert!(s.request(&key(), Trigger::Arrival, 30).is_some());
    }

    #[test]
    fn pushing_device_ignores_activity() {
        // A gap of its own, so only the pushing rule refuses.
        let mut s = scheduler_with(caps(true, false, 300_000));
        for t in [0, 1, 300_000, 86_400_000, Ms::MAX] {
            assert_eq!(s.request(&key(), Trigger::Activity, t), None, "t = {t}");
        }
        // Only activity is pointless for a pushing device; arrival still reads once.
        assert!(s.request(&key(), Trigger::Arrival, 0).is_some());
    }

    #[test]
    fn poll_only_activity_respects_min_gap() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        assert!(s.request(&key(), Trigger::Arrival, 0).is_some());
        assert_eq!(s.request(&key(), Trigger::Activity, 299_999), None);
        let grant = s.request(&key(), Trigger::Activity, 300_000).unwrap();
        assert_eq!(grant.trigger, Trigger::Activity);
        // The gap restarts from the latest grant.
        assert_eq!(s.request(&key(), Trigger::Activity, 599_999), None);
        assert!(s.request(&key(), Trigger::Activity, 600_000).is_some());
    }

    #[test]
    fn query_wakes_excluded_from_one_off_triggers() {
        let gap = 300_000;
        let mut s = scheduler_with(caps(false, true, gap));
        for trigger in [
            Trigger::Arrival,
            Trigger::Resume,
            Trigger::RefreshNow,
            Trigger::Connected,
        ] {
            assert_eq!(s.request(&key(), trigger, 0), None, "{trigger:?}");
        }
        assert!(s.request(&key(), Trigger::Activity, gap).is_some());
    }

    #[test]
    fn asleep_blocks_resume_and_refresh() {
        let mut s = scheduler_with(caps(false, false, 0));
        s.mark_asleep(&key());
        assert!(s.is_asleep(&key()));
        assert_eq!(s.request(&key(), Trigger::Resume, 0), None);
        assert_eq!(s.request(&key(), Trigger::RefreshNow, 0), None);
        assert!(s.is_asleep(&key()));

        assert!(s.request(&key(), Trigger::Connected, 0).is_some());
        assert!(!s.is_asleep(&key()));
    }

    #[test]
    fn activity_gap_does_not_block_refresh() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        assert!(s.request(&key(), Trigger::Activity, 0).is_some());
        assert!(s.request(&key(), Trigger::RefreshNow, 1_000).is_some());
    }

    #[test]
    fn activity_respects_gap() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        assert!(s.request(&key(), Trigger::Activity, 0).is_some());
        assert_eq!(s.request(&key(), Trigger::Activity, 299_999), None);
        assert!(s.request(&key(), Trigger::Activity, 300_000).is_some());
    }

    #[test]
    fn asleep_device_gets_no_activity_grant() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        s.mark_asleep(&key());
        assert_eq!(s.request(&key(), Trigger::Activity, 0), None);
        assert!(s.is_asleep(&key()));
    }

    #[test]
    fn activity_grant_does_not_clear_asleep_state() {
        for asleep in [false, true] {
            let mut s = scheduler_with(caps(false, false, 1));
            if asleep {
                s.mark_asleep(&key());
            }
            let granted = s.request(&key(), Trigger::Activity, 0).is_some();
            assert_eq!(granted, !asleep, "asleep = {asleep}");
            assert_eq!(s.is_asleep(&key()), asleep);
        }
    }

    #[test]
    fn activity_ready_at_mirrors_the_activity_rules() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        assert_eq!(s.activity_ready_at(&key()), Some(0));
        assert!(s.request(&key(), Trigger::RefreshNow, 1_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(301_000));
        assert_eq!(s.request(&key(), Trigger::Activity, 300_999), None);
        assert!(s.request(&key(), Trigger::Activity, 301_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(601_000));

        s.mark_asleep(&key());
        assert_eq!(s.activity_ready_at(&key()), None, "asleep");
        let pushing = scheduler_with(caps(true, false, 300_000));
        assert_eq!(pushing.activity_ready_at(&key()), None, "pushes");
        let empty = Scheduler::new(Policy::default());
        assert_eq!(empty.activity_ready_at(&key()), None, "unknown");
    }

    #[test]
    fn a_read_outside_a_grant_restarts_the_activity_gap() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        s.note_read(&key(), 1_000);
        assert_eq!(s.request(&key(), Trigger::Activity, 300_999), None);
        assert!(s.request(&key(), Trigger::Activity, 301_000).is_some());
        // An older read does not move the clock back.
        s.note_read(&key(), 5);
        assert_eq!(s.activity_ready_at(&key()), Some(601_000));
    }

    #[test]
    fn device_without_a_gap_gets_no_activity_grant() {
        // The gap is the only limit on audio-driven reads: an engine that declares a
        // non-pushing device without one gets none.
        let mut s = scheduler_with(caps(false, false, 0));
        assert_eq!(s.activity_ready_at(&key()), None);
        for t in [0, 1, 300_000] {
            assert_eq!(s.request(&key(), Trigger::Activity, t), None, "t = {t}");
        }
        assert!(s.request(&key(), Trigger::RefreshNow, 0).is_some());
    }

    #[test]
    fn mark_awake_lifts_asleep() {
        let mut s = scheduler_with(caps(false, false, 0));
        s.mark_asleep(&key());
        s.mark_awake(&key());
        assert!(!s.is_asleep(&key()));
        assert!(s.request(&key(), Trigger::RefreshNow, 0).is_some());
    }

    #[test]
    fn grants_use_policy_budget_for_their_trigger() {
        let policy = Policy::default();
        assert_eq!(
            policy,
            Policy {
                discovery_budget: 96,
                discovery_ms: 10_000,
                read_budget: 4,
                read_ms: 3_000,
            }
        );
        // A 1 ms gap lets the activity request one millisecond after the others.
        let mut s = scheduler_with(caps(false, false, 1));
        let mut ids = Vec::new();
        for (now, trigger) in (1_000..).zip(ALL_TRIGGERS) {
            let grant = s.request(&key(), trigger, now).unwrap();
            let (budget, duration) = match trigger {
                Trigger::Arrival | Trigger::Connected => (96, 10_000),
                Trigger::Resume | Trigger::RefreshNow | Trigger::Activity => (4, 3_000),
            };
            assert_eq!(grant.subject, key());
            assert_eq!(grant.trigger, trigger);
            assert_eq!(grant.max_requests, budget, "{trigger:?}");
            assert_eq!(grant.expires_ms, now + duration, "{trigger:?}");
            ids.push(grant.id);
        }
        assert_eq!(
            ids,
            [GrantId(1), GrantId(2), GrantId(3), GrantId(4), GrantId(5)]
        );
    }

    #[test]
    fn grant_budget_and_expiry() {
        let mut s = scheduler_with(caps(false, false, 0));
        let grant = s.request(&key(), Trigger::RefreshNow, 0).unwrap();
        for _ in 0..4 {
            assert!(s.admit_send(grant.id, 100));
        }
        assert!(!s.admit_send(grant.id, 100));

        let fresh = s.request(&key(), Trigger::RefreshNow, 0).unwrap();
        assert!(!s.admit_send(fresh.id, 3_001));

        let last_moment = s.request(&key(), Trigger::RefreshNow, 0).unwrap();
        assert!(s.admit_send(last_moment.id, 3_000));
    }

    #[test]
    fn unknown_grant_rejected() {
        let mut s = Scheduler::new(Policy::default());
        assert!(!s.admit_send(GrantId(999), 0));
    }

    #[test]
    fn unregister_forgets_state() {
        let mut s = scheduler_with(caps(false, false, 0));
        let open = s.request(&key(), Trigger::Arrival, 0).unwrap();
        s.mark_asleep(&key());
        s.unregister(&key());

        for trigger in ALL_TRIGGERS {
            assert_eq!(s.request(&key(), trigger, 0), None, "{trigger:?}");
        }
        assert!(!s.is_asleep(&key()));
        // Grants issued before the device went away no longer admit sends.
        assert!(!s.admit_send(open.id, 0));
    }
}
