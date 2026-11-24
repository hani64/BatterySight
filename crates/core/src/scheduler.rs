//! Decides when anything may be sent to a device.
//!
//! The hub hands an engine a [`Grant`] for the event that justifies a query
//! and spends it only after checking the report. Time only moves through the
//! `now` arguments; nothing here runs on a timer or produces grants by itself.

use std::collections::HashMap;
use std::collections::hash_map::{Entry, OccupiedEntry};

use crate::hid::GroupId;
use crate::model::DeviceKey;
use crate::time::Ms;

/// Event that justifies asking a device. Opening the menu is deliberately not
/// a trigger: it shows last known values and never queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// Device appeared (app start counts as arrival).
    Arrival,
    /// A group's discovery registered nothing and is tried again: at most
    /// [`REDISCOVERIES`] times after each arrival grant.
    Rediscovery,
    /// Connect notification from the receiver/link.
    Connected,
    /// System resume.
    Resume,
    /// Explicit "Refresh now" command.
    RefreshNow,
    /// Audio on the physical device or an opted-in charging check. Cannot wake an
    /// asleep device: audio can still play to a dongle whose headset is off.
    Activity,
    /// Fresh activity reported by the device itself.
    Awake,
}

/// How many [`Trigger::Rediscovery`] grants a key gets after each arrival grant.
pub const REDISCOVERIES: u8 = 3;

/// Gap between interval grants without a user setting: 10 minutes, or the
/// device's `min_query_gap_ms` when that is longer.
pub const DEFAULT_INTERVAL_MS: Ms = 600_000;

/// What an engine declares about a registered device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The device reports battery changes on its own and doesn't need interval reads.
    pub pushes_battery: bool,
    /// A query wakes the device: only observed activity may justify one.
    pub query_wakes: bool,
    /// Audible activity on this device's physical container permits a read.
    pub audio: bool,
    /// A fresh input report from this device permits a read.
    pub awake_signal: bool,
    /// Being online and charging permits a timed read, even without activity.
    pub charging: bool,
    /// Minimum time since the last grant or noted read before any interval grant.
    /// A user gap or [`DEFAULT_INTERVAL_MS`] may lengthen it; the first grant
    /// after a wake uses this minimum alone. Zero disables all interval grants.
    pub min_query_gap_ms: Ms,
}

#[derive(Clone, Copy)]
enum ActivitySource {
    Audio,
    Awake,
    Charging,
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
    /// Rediscovery grants since the last arrival grant.
    rediscoveries: u8,
    asleep: bool,
    /// The device just registered or woke, and no grant other than a `Connected` one has
    /// been issued since: the next interval grant waits only the engine minimum,
    /// not the user gap.
    woke: bool,
}

impl DeviceSlot {
    fn interval_ready_at(
        &self,
        source: ActivitySource,
        user_gap: impl FnOnce() -> Option<Ms>,
    ) -> Option<Ms> {
        let caps = self.caps;
        let allowed = match source {
            ActivitySource::Audio => caps.audio,
            ActivitySource::Awake => caps.awake_signal,
            ActivitySource::Charging => caps.charging,
        };
        let min = caps.min_query_gap_ms;
        if !allowed || caps.pushes_battery || self.asleep || min == 0 {
            return None;
        }
        let gap = if self.woke {
            min
        } else {
            min.max(user_gap().unwrap_or(DEFAULT_INTERVAL_MS))
        };
        Some(self.last_grant_ms.map_or(0, |last| last.saturating_add(gap)))
    }
}

/// What is left of an issued grant.
#[derive(Debug)]
struct OpenGrant {
    subject: DeviceKey,
    owner: GroupId,
    expires_ms: Ms,
    remaining: u16,
}

/// A live grant held while the hub checks a request. Dropping it costs nothing.
pub(crate) struct PendingSend<'a>(OccupiedEntry<'a, GrantId, OpenGrant>);

impl PendingSend<'_> {
    pub(crate) fn subject(&self) -> &DeviceKey {
        &self.0.get().subject
    }

    pub(crate) fn expires_ms(&self) -> Ms {
        self.0.get().expires_ms
    }

    pub(crate) fn spend(mut self) {
        let remaining = &mut self.0.get_mut().remaining;
        *remaining -= 1;
        if *remaining == 0 {
            self.0.remove();
        }
    }
}

#[derive(Debug)]
pub struct Scheduler {
    policy: Policy,
    devices: HashMap<DeviceKey, DeviceSlot>,
    grants: HashMap<GrantId, OpenGrant>,
    /// Id of the next grant; ids are never reused.
    next_id: u32,
    /// User gaps between interval grants, by device key; kept across registrations.
    user_gaps: HashMap<DeviceKey, Ms>,
}

impl Scheduler {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            devices: HashMap::new(),
            grants: HashMap::new(),
            next_id: 1,
            user_gaps: HashMap::new(),
        }
    }

    /// Starts tracking `key`; a first registration counts as a wake. Registering a known
    /// key only updates its capabilities; its history (arrival used, last grant, asleep,
    /// wake) is kept.
    pub fn register(&mut self, key: DeviceKey, caps: Capabilities) {
        self.devices
            .entry(key)
            .and_modify(|slot| slot.caps = caps)
            .or_insert(DeviceSlot {
                caps,
                last_grant_ms: None,
                arrival_used: false,
                rediscoveries: 0,
                asleep: false,
                woke: true,
            });
    }

    /// Restores the current registrant's capabilities without changing the read history.
    pub(crate) fn set_capabilities(&mut self, key: &DeviceKey, caps: Capabilities) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.caps = caps;
        }
    }

    /// Lets `key` have another arrival grant, and with it [`REDISCOVERIES`] more
    /// rediscovery grants: for a group that registered no device, a resume or refresh
    /// starts over as an arrival.
    pub fn rearm_arrival(&mut self, key: &DeviceKey) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.arrival_used = false;
        }
    }

    /// Forgets `key` and revokes its open grants.
    pub fn unregister(&mut self, key: &DeviceKey) {
        self.devices.remove(key);
        self.revoke_grants(key);
    }

    /// Discards an owner's open grants without resetting the device's read history.
    pub(crate) fn revoke_grants(&mut self, key: &DeviceKey) {
        self.grants.retain(|_, grant| grant.subject != *key);
    }

    /// Replaces every user gap between interval grants (key → ms). A gap below the
    /// device's `min_query_gap_ms` counts as the minimum; a key without one uses
    /// [`DEFAULT_INTERVAL_MS`], or the minimum if that is longer.
    pub fn set_user_gaps(&mut self, gaps: HashMap<DeviceKey, Ms>) {
        self.user_gaps = gaps;
    }

    /// Grants `owner` permission to query `key` for `trigger`, if eligible.
    pub fn request(
        &mut self,
        key: &DeviceKey,
        owner: GroupId,
        trigger: Trigger,
        now: Ms,
    ) -> Option<Grant> {
        self.request_with_source(key, owner, trigger, ActivitySource::Audio, now)
    }

    /// Charging retains the existing Activity grant for the engine, but does not
    /// borrow the audio eligibility that normally backs that trigger.
    pub(crate) fn request_charging(
        &mut self,
        key: &DeviceKey,
        owner: GroupId,
        now: Ms,
    ) -> Option<Grant> {
        self.request_with_source(
            key,
            owner,
            Trigger::Activity,
            ActivitySource::Charging,
            now,
        )
    }

    fn request_with_source(
        &mut self,
        key: &DeviceKey,
        owner: GroupId,
        trigger: Trigger,
        activity_source: ActivitySource,
        now: Ms,
    ) -> Option<Grant> {
        let slot = self.devices.get_mut(key)?;
        let caps = slot.caps;
        let allowed = match trigger {
            Trigger::Arrival => !caps.query_wakes && !slot.arrival_used,
            Trigger::Rediscovery => !caps.query_wakes && slot.rediscoveries < REDISCOVERIES,
            Trigger::Connected => !caps.query_wakes,
            Trigger::Resume | Trigger::RefreshNow => !caps.query_wakes && !slot.asleep,
            Trigger::Activity | Trigger::Awake => {
                let source = if trigger == Trigger::Awake {
                    ActivitySource::Awake
                } else {
                    activity_source
                };
                slot.interval_ready_at(source, || self.user_gaps.get(key).copied())
                    .is_some_and(|at| now >= at)
            }
        };
        if !allowed {
            return None;
        }

        slot.last_grant_ms = Some(now);
        // Any grant reads the device, so its latest reading is no longer the first reading
        // after the wake, unless the grant is the link-up itself: its read is that first
        // reading. Keeping `woke` there costs one earlier read, for consistency with a wake push.
        if trigger != Trigger::Connected {
            slot.woke = false;
        }
        // A link-up can clear asleep itself; an Awake grant requires the hub to
        // have already validated the signal and marked the device awake.
        match trigger {
            Trigger::Arrival => {
                slot.arrival_used = true;
                slot.rediscoveries = 0;
            }
            Trigger::Rediscovery => slot.rediscoveries += 1,
            Trigger::Connected => {
                slot.woke |= slot.asleep;
                slot.asleep = false;
            }
            Trigger::Resume | Trigger::RefreshNow | Trigger::Activity | Trigger::Awake => {}
        }

        let (max_requests, duration) = match trigger {
            Trigger::Arrival | Trigger::Rediscovery | Trigger::Connected => {
                (self.policy.discovery_budget, self.policy.discovery_ms)
            }
            Trigger::Resume | Trigger::RefreshNow | Trigger::Activity | Trigger::Awake => {
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
                owner,
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

    /// Borrows a live, unspent grant issued to this group, without spending it.
    pub(crate) fn pending_send(
        &mut self,
        grant: GrantId,
        owner: GroupId,
        now: Ms,
    ) -> Option<PendingSend<'_>> {
        let Entry::Occupied(entry) = self.grants.entry(grant) else {
            return None;
        };
        let open = entry.get();
        if open.owner != owner {
            return None;
        }
        if now > open.expires_ms || open.remaining == 0 {
            entry.remove();
            return None;
        }
        Some(PendingSend(entry))
    }

    /// Device answered "asleep/unlinked" or stayed silent: stop asking it.
    pub fn mark_asleep(&mut self, key: &DeviceKey) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.asleep = true;
        }
    }

    /// A push or other sign of life arrived; an asleep device counts as woken.
    pub fn mark_awake(&mut self, key: &DeviceKey) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.woke |= slot.asleep;
            slot.asleep = false;
        }
    }

    pub fn is_asleep(&self, key: &DeviceKey) -> bool {
        self.devices.get(key).is_some_and(|slot| slot.asleep)
    }

    /// When audio can next justify a [`Trigger::Activity`] grant. There is no
    /// deadline for a device that disallows audio, pushes, is asleep or has no gap.
    pub fn activity_ready_at(&self, key: &DeviceKey) -> Option<Ms> {
        self.interval_ready_at(key, ActivitySource::Audio)
    }

    pub(crate) fn charging_ready_at(&self, key: &DeviceKey) -> Option<Ms> {
        self.interval_ready_at(key, ActivitySource::Charging)
    }

    fn interval_ready_at(&self, key: &DeviceKey, source: ActivitySource) -> Option<Ms> {
        self.devices
            .get(key)?
            .interval_ready_at(source, || self.user_gaps.get(key).copied())
    }

    /// `key` was read without a grant of its own (its discovery's first battery read, or a
    /// reading it pushed): the interval counts from `now`, unless a later grant
    /// already restarted it.
    pub fn note_read(&mut self, key: &DeviceKey, now: Ms) {
        if let Some(slot) = self.devices.get_mut(key) {
            slot.last_grant_ms = Some(slot.last_grant_ms.map_or(now, |last| last.max(now)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: GroupId = GroupId(1);

    const ALL_TRIGGERS: [Trigger; 7] = [
        Trigger::Arrival,
        Trigger::Rediscovery,
        Trigger::Connected,
        Trigger::Resume,
        Trigger::RefreshNow,
        Trigger::Activity,
        Trigger::Awake,
    ];

    fn key() -> DeviceKey {
        DeviceKey("hid:046d:c547:mouse".to_string())
    }

    fn caps(pushes_battery: bool, query_wakes: bool, min_query_gap_ms: Ms) -> Capabilities {
        Capabilities {
            pushes_battery,
            query_wakes,
            audio: !pushes_battery && min_query_gap_ms > 0,
            awake_signal: false,
            charging: false,
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
        assert!(s.request(&key(), OWNER, Trigger::Arrival, 0).is_some());
        assert_eq!(s.request(&key(), OWNER, Trigger::Arrival, 10), None);

        // Registering again without unregistering keeps the history.
        s.register(key(), caps(false, false, 0));
        assert_eq!(s.request(&key(), OWNER, Trigger::Arrival, 20), None);

        s.unregister(&key());
        s.register(key(), caps(false, false, 0));
        assert!(s.request(&key(), OWNER, Trigger::Arrival, 30).is_some());
    }

    #[test]
    fn rediscovery_is_granted_three_times_per_arrival() {
        let mut s = scheduler_with(caps(false, false, 0));
        assert!(s.request(&key(), OWNER, Trigger::Arrival, 0).is_some());
        for now in 1..=3 {
            assert!(
                s.request(&key(), OWNER, Trigger::Rediscovery, now).is_some(),
                "{now}"
            );
        }
        assert_eq!(s.request(&key(), OWNER, Trigger::Rediscovery, 4), None);

        // A resume or refresh rearms the arrival, whose grant restarts the count.
        s.rearm_arrival(&key());
        assert_eq!(s.request(&key(), OWNER, Trigger::Rediscovery, 5), None);
        assert!(s.request(&key(), OWNER, Trigger::Arrival, 6).is_some());
        assert_eq!(s.request(&key(), OWNER, Trigger::Arrival, 7), None);
        assert!(s.request(&key(), OWNER, Trigger::Rediscovery, 8).is_some());
    }

    #[test]
    fn pushing_device_ignores_activity() {
        // A gap of its own, so only the pushing rule refuses.
        let mut s = scheduler_with(caps(true, false, 300_000));
        for t in [0, 1, 300_000, 86_400_000, Ms::MAX] {
            assert_eq!(s.request(&key(), OWNER, Trigger::Activity, t), None, "t = {t}");
        }
        // Only activity is pointless for a pushing device; arrival still reads once.
        assert!(s.request(&key(), OWNER, Trigger::Arrival, 0).is_some());
    }

    #[test]
    fn pushing_device_never_gets_awake_grants() {
        let mut s = scheduler_with(Capabilities {
            awake_signal: true,
            ..caps(true, false, 300_000)
        });
        assert_eq!(s.request(&key(), OWNER, Trigger::Awake, 0), None);
        assert_eq!(s.request(&key(), OWNER, Trigger::Awake, 600_000), None);
    }

    #[test]
    fn awake_audio_and_charging_share_one_interval() {
        let mut s = scheduler_with(Capabilities {
            audio: true,
            awake_signal: true,
            charging: true,
            ..caps(false, false, 100)
        });
        s.note_read(&key(), 0);
        s.set_user_gaps([(key(), 300)].into_iter().collect());

        assert!(s.request(&key(), OWNER, Trigger::Activity, 100).is_some());
        assert_eq!(s.request(&key(), OWNER, Trigger::Awake, 399), None);
        assert_eq!(s.request_charging(&key(), OWNER, 399), None);
        assert!(s.request_charging(&key(), OWNER, 400).is_some());
        assert_eq!(s.request(&key(), OWNER, Trigger::Awake, 699), None);
        assert_eq!(s.activity_ready_at(&key()), Some(700));
        assert!(s.request(&key(), OWNER, Trigger::Awake, 700).is_some());
        assert_eq!(s.charging_ready_at(&key()), Some(1_000));
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 999), None);
    }

    #[test]
    fn poll_only_activity_respects_the_default_gap() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        // The arrival grant ends the registration's wake: the default gap applies.
        assert!(s.request(&key(), OWNER, Trigger::Arrival, 0).is_some());
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 599_999), None);
        let grant = s.request(&key(), OWNER, Trigger::Activity, 600_000).unwrap();
        assert_eq!(grant.trigger, Trigger::Activity);
        // The gap restarts from the latest grant.
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 1_199_999), None);
        assert!(s.request(&key(), OWNER, Trigger::Activity, 1_200_000).is_some());
    }

    #[test]
    fn query_wakes_excluded_from_one_off_triggers() {
        let gap = 300_000;
        let mut s = scheduler_with(caps(false, true, gap));
        for trigger in [
            Trigger::Arrival,
            Trigger::Rediscovery,
            Trigger::Resume,
            Trigger::RefreshNow,
            Trigger::Connected,
        ] {
            assert_eq!(s.request(&key(), OWNER, trigger, 0), None, "{trigger:?}");
        }
        assert!(s.request(&key(), OWNER, Trigger::Activity, gap).is_some());
    }

    #[test]
    fn asleep_blocks_resume_and_refresh() {
        let mut s = scheduler_with(caps(false, false, 0));
        s.mark_asleep(&key());
        assert!(s.is_asleep(&key()));
        assert_eq!(s.request(&key(), OWNER, Trigger::Resume, 0), None);
        assert_eq!(s.request(&key(), OWNER, Trigger::RefreshNow, 0), None);
        assert!(s.is_asleep(&key()));

        assert!(s.request(&key(), OWNER, Trigger::Connected, 0).is_some());
        assert!(!s.is_asleep(&key()));
    }

    #[test]
    fn activity_gap_does_not_block_refresh() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        assert!(s.request(&key(), OWNER, Trigger::Activity, 0).is_some());
        assert!(s.request(&key(), OWNER, Trigger::RefreshNow, 1_000).is_some());
    }

    #[test]
    fn activity_respects_gap() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        // The first grant after registration; the default gap follows it.
        assert!(s.request(&key(), OWNER, Trigger::Activity, 0).is_some());
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 599_999), None);
        assert!(s.request(&key(), OWNER, Trigger::Activity, 600_000).is_some());
    }

    #[test]
    fn asleep_device_gets_no_activity_grant() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        s.mark_asleep(&key());
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 0), None);
        assert!(s.is_asleep(&key()));
    }

    #[test]
    fn activity_grant_does_not_clear_asleep_state() {
        for asleep in [false, true] {
            let mut s = scheduler_with(caps(false, false, 1));
            if asleep {
                s.mark_asleep(&key());
            }
            let granted = s.request(&key(), OWNER, Trigger::Activity, 0).is_some();
            assert_eq!(granted, !asleep, "asleep = {asleep}");
            assert_eq!(s.is_asleep(&key()), asleep);
        }
    }

    #[test]
    fn activity_ready_at_mirrors_the_activity_rules() {
        let mut s = scheduler_with(caps(false, false, 300_000));
        assert_eq!(s.activity_ready_at(&key()), Some(0));
        assert!(s.request(&key(), OWNER, Trigger::RefreshNow, 1_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(601_000));
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 600_999), None);
        assert!(s.request(&key(), OWNER, Trigger::Activity, 601_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(1_201_000));

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
        // Registered (a wake) and read: the minimum.
        s.note_read(&key(), 1_000);
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 300_999), None);
        assert!(s.request(&key(), OWNER, Trigger::Activity, 301_000).is_some());
        // An older read does not move the clock back.
        s.note_read(&key(), 5);
        assert_eq!(s.activity_ready_at(&key()), Some(901_000));
    }

    #[test]
    fn device_without_a_gap_gets_no_activity_grant() {
        // The gap is the only limit on audio-driven reads: an engine that declares a
        // non-pushing device without one gets none.
        let mut s = scheduler_with(caps(false, false, 0));
        assert_eq!(s.activity_ready_at(&key()), None);
        for t in [0, 1, 300_000] {
            assert_eq!(s.request(&key(), OWNER, Trigger::Activity, t), None, "t = {t}");
        }
        assert!(s.request(&key(), OWNER, Trigger::RefreshNow, 0).is_some());
    }

    #[test]
    fn mark_awake_lifts_asleep() {
        let mut s = scheduler_with(caps(false, false, 0));
        s.mark_asleep(&key());
        s.mark_awake(&key());
        assert!(!s.is_asleep(&key()));
        assert!(s.request(&key(), OWNER, Trigger::RefreshNow, 0).is_some());
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
        // A 1 ms gap (minimum and user gap) lets the activity request one millisecond after
        // the others.
        let mut s = scheduler_with(Capabilities {
            awake_signal: true,
            ..caps(false, false, 1)
        });
        s.set_user_gaps([(key(), 1)].into_iter().collect());
        let mut ids = Vec::new();
        for (now, trigger) in (1_000..).zip(ALL_TRIGGERS) {
            let grant = s.request(&key(), OWNER, trigger, now).unwrap();
            let (budget, duration) = match trigger {
                Trigger::Arrival | Trigger::Rediscovery | Trigger::Connected => (96, 10_000),
                Trigger::Resume | Trigger::RefreshNow | Trigger::Activity | Trigger::Awake => {
                    (4, 3_000)
                }
            };
            assert_eq!(grant.subject, key());
            assert_eq!(grant.trigger, trigger);
            assert_eq!(grant.max_requests, budget, "{trigger:?}");
            assert_eq!(grant.expires_ms, now + duration, "{trigger:?}");
            ids.push(grant.id);
        }
        assert_eq!(
            ids,
            [
                GrantId(1),
                GrantId(2),
                GrantId(3),
                GrantId(4),
                GrantId(5),
                GrantId(6),
                GrantId(7),
            ]
        );
    }

    #[test]
    fn grant_budget_and_expiry() {
        let mut s = scheduler_with(caps(false, false, 0));
        let grant = s.request(&key(), OWNER, Trigger::RefreshNow, 0).unwrap();
        for _ in 0..4 {
            s.pending_send(grant.id, OWNER, 100).unwrap().spend();
        }
        assert!(s.pending_send(grant.id, OWNER, 100).is_none());

        let fresh = s.request(&key(), OWNER, Trigger::RefreshNow, 0).unwrap();
        assert!(s.pending_send(fresh.id, OWNER, 3_001).is_none());

        let last_moment = s.request(&key(), OWNER, Trigger::RefreshNow, 0).unwrap();
        s.pending_send(last_moment.id, OWNER, 3_000).unwrap().spend();
    }

    #[test]
    fn unknown_grant_rejected() {
        let mut s = Scheduler::new(Policy::default());
        assert!(s.pending_send(GrantId(999), OWNER, 0).is_none());
    }

    #[test]
    fn unregister_forgets_state() {
        let mut s = scheduler_with(caps(false, false, 0));
        let open = s.request(&key(), OWNER, Trigger::Arrival, 0).unwrap();
        s.mark_asleep(&key());
        s.unregister(&key());

        for trigger in ALL_TRIGGERS {
            assert_eq!(s.request(&key(), OWNER, trigger, 0), None, "{trigger:?}");
        }
        assert!(!s.is_asleep(&key()));
        // Grants issued before the device went away no longer admit sends.
        assert!(s.pending_send(open.id, OWNER, 0).is_none());
    }

    /// Engine minimum of the polled device in the user-gap tests.
    const MIN: Ms = 300_000;

    fn gaps(gap: Option<Ms>) -> HashMap<DeviceKey, Ms> {
        gap.into_iter().map(|g| (key(), g)).collect()
    }

    /// A polled device just registered (a wake), read at 0 and granted its post-wake
    /// activity read at the minimum.
    fn polled_after_wake_grant() -> Scheduler {
        let mut s = scheduler_with(caps(false, false, MIN));
        s.note_read(&key(), 0);
        assert_eq!(s.activity_ready_at(&key()), Some(MIN));
        assert!(s.request(&key(), OWNER, Trigger::Activity, MIN).is_some());
        s
    }

    #[test]
    fn user_gap_lengthens_activity_gap() {
        let mut s = polled_after_wake_grant();
        s.set_user_gaps(gaps(Some(1_800_000)));
        assert_eq!(s.activity_ready_at(&key()), Some(2_100_000));
        assert_eq!(s.request(&key(), OWNER, Trigger::Activity, 2_099_999), None);
        assert!(s.request(&key(), OWNER, Trigger::Activity, 2_100_000).is_some());
    }

    #[test]
    fn user_gap_never_below_minimum() {
        let mut s = polled_after_wake_grant();
        s.set_user_gaps(gaps(Some(60_000)));
        assert_eq!(s.activity_ready_at(&key()), Some(600_000));
    }

    #[test]
    fn first_activity_after_wake_uses_minimum() {
        let mut s = polled_after_wake_grant();
        s.set_user_gaps(gaps(Some(1_800_000)));
        s.mark_asleep(&key());
        s.mark_awake(&key());
        // The wake push.
        s.note_read(&key(), 1_000_000);
        assert_eq!(s.activity_ready_at(&key()), Some(1_300_000));
        assert!(s.request(&key(), OWNER, Trigger::Activity, 1_300_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(3_100_000));
    }

    #[test]
    fn mark_awake_on_an_awake_device_is_no_wake() {
        let mut s = polled_after_wake_grant();
        s.set_user_gaps(gaps(Some(1_800_000)));
        s.mark_awake(&key());
        s.note_read(&key(), 1_000_000);
        assert_eq!(s.activity_ready_at(&key()), Some(2_800_000));
    }

    #[test]
    fn user_gap_survives_re_registration() {
        let mut s = scheduler_with(caps(false, false, MIN));
        s.set_user_gaps(gaps(Some(1_800_000)));
        s.unregister(&key());
        // A fresh registration counts as a wake.
        s.register(key(), caps(false, false, MIN));
        s.note_read(&key(), 0);
        assert_eq!(s.activity_ready_at(&key()), Some(MIN));
        assert!(s.request(&key(), OWNER, Trigger::Activity, MIN).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(2_100_000));
    }

    #[test]
    fn re_registering_a_known_key_keeps_its_wake() {
        // Registered again before its post-wake grant: still the minimum.
        let mut s = scheduler_with(caps(false, false, MIN));
        s.set_user_gaps(gaps(Some(1_800_000)));
        s.note_read(&key(), 0);
        s.register(key(), caps(false, false, MIN));
        assert_eq!(s.activity_ready_at(&key()), Some(MIN));

        // Registered again after it: the user gap, a known key is no wake.
        let mut s = polled_after_wake_grant();
        s.set_user_gaps(gaps(Some(1_800_000)));
        s.register(key(), caps(false, false, MIN));
        s.note_read(&key(), 1_000_000);
        assert_eq!(s.activity_ready_at(&key()), Some(2_800_000));
    }

    #[test]
    fn changing_user_gap_applies_to_next_grant() {
        let mut s = polled_after_wake_grant();
        s.set_user_gaps(gaps(Some(1_800_000)));
        assert_eq!(s.activity_ready_at(&key()), Some(2_100_000));
        s.set_user_gaps(gaps(Some(1_200_000)));
        assert_eq!(s.activity_ready_at(&key()), Some(1_500_000));
        // None stored: the default.
        s.set_user_gaps(gaps(None));
        assert_eq!(s.activity_ready_at(&key()), Some(900_000));
    }

    #[test]
    fn user_gap_ignored_for_pushing_devices() {
        let mut s = scheduler_with(caps(true, false, MIN));
        s.set_user_gaps(gaps(Some(1_800_000)));
        assert_eq!(s.activity_ready_at(&key()), None);
    }

    #[test]
    fn default_interval_applies_without_user_gap() {
        assert_eq!(DEFAULT_INTERVAL_MS, 600_000);
        let mut s = polled_after_wake_grant();
        assert_eq!(s.activity_ready_at(&key()), Some(900_000));
        // A stored gap, even the minimum, replaces the default.
        s.set_user_gaps(gaps(Some(MIN)));
        assert_eq!(s.activity_ready_at(&key()), Some(600_000));

        // A minimum above the default stays the gap.
        let mut s = scheduler_with(caps(false, false, 900_000));
        s.note_read(&key(), 0);
        assert!(s.request(&key(), OWNER, Trigger::Activity, 900_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(1_800_000));
    }

    #[test]
    fn other_grants_end_the_wake() {
        let mut s = scheduler_with(caps(false, false, MIN));
        s.set_user_gaps(gaps(Some(1_800_000)));
        assert!(s.request(&key(), OWNER, Trigger::RefreshNow, 1_000).is_some());
        assert_eq!(s.activity_ready_at(&key()), Some(1_801_000));
    }

    #[test]
    fn connected_wake_keeps_the_minimum_for_the_next_activity() {
        // A link-up flips the device awake through the grant itself, or through
        // `mark_awake` just before it (as the hub does); either way the grant's read is
        // the wake reading.
        for awake_first in [false, true] {
            let mut s = polled_after_wake_grant();
            s.set_user_gaps(gaps(Some(1_800_000)));
            s.mark_asleep(&key());
            if awake_first {
                s.mark_awake(&key());
            }
            assert!(s.request(&key(), OWNER, Trigger::Connected, 1_000_000).is_some());
            s.note_read(&key(), 1_000_000);
            assert_eq!(
                s.activity_ready_at(&key()),
                Some(1_300_000),
                "awake_first = {awake_first}"
            );
            assert!(s.request(&key(), OWNER, Trigger::Activity, 1_300_000).is_some());
            assert_eq!(s.activity_ready_at(&key()), Some(3_100_000));
        }
    }
}
