//! Sans-IO orchestrator: owns the engines, the device state and the scheduler.
//!
//! The platform runtime feeds [`HubInput`]s with the current time and executes
//! the returned [`HubOutput`]s; nothing here performs I/O or reads a clock.

use std::collections::{BTreeMap, HashMap};

use crate::catalog::{Catalog, CollectionSelector, EngineKind};
use crate::engine::{Action, Engine, WriteDestination, write_allowed};
use crate::engines::{self, Selection};
use crate::hid::{CollectionId, CollectionRole, GroupId, HidGroup, ReportKind, ReportOrigin};
use crate::model::{
    Charge, DeviceKey, DeviceKind, DeviceState, DeviceUpdate, Presence, Verification, Via,
};
use crate::scheduler::{Capabilities, Grant, GrantId, Policy, Scheduler, Trigger};
use crate::time::Ms;

/// How long a removed device keeps its last values before it disappears.
pub const REMOVAL_GRACE_MS: Ms = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubInput {
    GroupArrived(HidGroup),
    /// Runtime arrival after opening the collections selected before any device I/O.
    MatchedGroupArrived(HidGroup, Selection),
    GroupRemoved(GroupId),
    Report {
        collection: CollectionId,
        origin: ReportOrigin,
        bytes: Vec<u8>,
    },
    Resume,
    RefreshNow,
    /// Audible activity on the physical device, not evidence that it is awake.
    Activity(DeviceKey),
    /// The user's gaps between activity reads, by device key; replaces all earlier ones.
    SetIntervals(HashMap<DeviceKey, Ms>),
    Tick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubOutput {
    Send {
        collection: CollectionId,
        kind: ReportKind,
        bytes: Vec<u8>,
        /// For feature operations, the transaction deadline capped by its grant expiry.
        deadline: Option<Ms>,
    },
    /// A request was denied without exposing its payload or the device identity.
    WriteRefused {
        kind: ReportKind,
        collection: CollectionId,
        reason: WriteRefusal,
    },
    /// The snapshot changed during this call.
    StateChanged,
    /// The discovery of the group with key `group` registered no device; it is retried at
    /// `retry_at`, or not at all until the next arrival, resume or refresh (`None`).
    DiscoveryFailed {
        group: DeviceKey,
        retry_at: Option<Ms>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
    Collection,
    Width,
    Rule,
    Grant,
    Deadline,
}

struct SendRequest<'a> {
    collection: CollectionId,
    kind: ReportKind,
    bytes: &'a [u8],
    deadline: Option<Ms>,
}

type EngineFactory = Box<dyn Fn(&HidGroup, &Catalog) -> Option<Box<dyn Engine>>>;

/// A group key gets its arrival conversation, `Rediscovery` grants while its engine
/// registered no device, an arrival again on resume or refresh in that case (never for a
/// catalog `query_wakes` device), and the `Connected` grants its engine asks for (e.g. to
/// discover a newly paired device).
const GROUP_CAPS: Capabilities = Capabilities {
    pushes_battery: false,
    query_wakes: false,
    audio: false,
    awake_signal: false,
    charging: false,
    min_query_gap_ms: 0,
};

/// How deep grant deliveries may nest within one `handle` call (an engine
/// reporting `Connected` from inside `on_grant`); deeper requests are dropped.
const MAX_NESTED_GRANTS: usize = 4;

struct Route {
    group: GroupId,
    reads: bool,
    role: Option<CollectionRole>,
    output_len: u16,
    feature_len: u16,
}

/// One arrived group and its engine.
struct Slot {
    engine: Box<dyn Engine>,
    key: DeviceKey,
    /// Vendor ID for wireless PID and family lookups, never a receiver's USB PID.
    vid: Option<u16>,
    kind: EngineKind,
    /// Direct entry chosen during admission, not a second VID/PID search.
    direct: Option<usize>,
    /// HID product string, the name of a device whose engine gives none.
    product: String,
    collections: Vec<CollectionId>,
    /// Hub keys of devices this engine registered under a shared key another live group
    /// already held (see [`Hub::claim`]), by the engine's own key, which it keeps using.
    aliases: BTreeMap<DeviceKey, DeviceKey>,
}

impl Slot {
    fn engine_key<'a>(&'a self, hub_key: &'a DeviceKey) -> &'a DeviceKey {
        self.aliases
            .iter()
            .find_map(|(own, hub)| (hub == hub_key).then_some(own))
            .unwrap_or(hub_key)
    }
}

/// The fields of an [`Action::Register`].
struct Registration {
    key: DeviceKey,
    caps: Capabilities,
    initial: DeviceUpdate,
    /// Wireless product id of a device behind a receiver, if the receiver reported one.
    wpid: Option<u16>,
}

struct Registrant {
    group: GroupId,
    caps: Capabilities,
}

fn apply_polling_caps(state: &mut DeviceState, caps: Capabilities) -> bool {
    let min = ((caps.audio || caps.awake_signal) && !caps.pushes_battery && caps.min_query_gap_ms > 0)
        .then_some(caps.min_query_gap_ms);
    let changed = state.poll_min_ms != min || state.reports_changes != caps.pushes_battery;
    state.poll_min_ms = min;
    state.reports_changes = caps.pushes_battery;
    changed
}

/// What one `handle` call produces.
#[derive(Default)]
struct Pass {
    out: Vec<HubOutput>,
    changed: bool,
    /// Grant deliveries currently on the stack.
    depth: usize,
}

pub struct Hub {
    catalog: Catalog,
    factory: Option<EngineFactory>,
    scheduler: Scheduler,
    engines: BTreeMap<GroupId, Slot>,
    routes: HashMap<CollectionId, Route>,
    /// Registered devices and every live group whose engine registered them;
    /// the last one is the current owner. The same device can be served by
    /// several groups at once (e.g. receiver and cable).
    owners: BTreeMap<DeviceKey, Vec<Registrant>>,
    states: BTreeMap<DeviceKey, DeviceState>,
    /// Devices no engine owns any more, and when their entries expire.
    grace: BTreeMap<DeviceKey, Ms>,
}

impl Hub {
    pub fn new(catalog: Catalog, policy: Policy) -> Self {
        Self {
            catalog,
            factory: None,
            scheduler: Scheduler::new(policy),
            engines: BTreeMap::new(),
            routes: HashMap::new(),
            owners: BTreeMap::new(),
            states: BTreeMap::new(),
            grace: BTreeMap::new(),
        }
    }

    pub fn with_engine_factory(self, f: EngineFactory) -> Self {
        Self {
            factory: Some(f),
            ..self
        }
    }

    /// Resolve the roles against this hub's catalog, using only cached OS names for product gates.
    pub fn select_group(
        &self,
        group: &HidGroup,
        product: impl FnMut(usize, &CollectionSelector) -> Option<bool>,
    ) -> Option<Selection> {
        engines::select_with(group, &self.catalog, product)
    }

    pub fn handle(&mut self, input: HubInput, now: Ms) -> Vec<HubOutput> {
        let mut pass = Pass::default();
        match input {
            HubInput::GroupArrived(group) => self.arrive(&group, None, now, &mut pass),
            HubInput::MatchedGroupArrived(group, selection) => {
                self.arrive(&group, Some(&selection), now, &mut pass)
            }
            HubInput::GroupRemoved(id) => self.remove_group(id, now, &mut pass),
            HubInput::Report {
                collection,
                origin,
                bytes,
            } => {
                if let Some(route) = self.routes.get(&collection).filter(|route| match origin {
                    ReportOrigin::Input => route.reads,
                    ReportOrigin::FeatureReply => route.role.is_some(),
                })
                    && let Some(slot) = self.engines.get_mut(&route.group)
                {
                    let gid = route.group;
                    let actions = slot.engine.on_report(collection, origin, &bytes, now);
                    self.apply(gid, actions, Some(origin), now, &mut pass);
                }
            }
            HubInput::Resume => self.request_all(Trigger::Resume, now, &mut pass),
            HubInput::RefreshNow => self.request_all(Trigger::RefreshNow, now, &mut pass),
            HubInput::Activity(key) => {
                if let Some(gid) = self.device_owner(&key) {
                    self.grant(gid, &key, Trigger::Activity, now, &mut pass);
                }
            }
            HubInput::SetIntervals(gaps) => self.scheduler.set_user_gaps(gaps),
            HubInput::Tick => self.tick(now, &mut pass),
        }
        if pass.changed {
            pass.out.push(HubOutput::StateChanged);
        }
        pass.out
    }

    /// Every known device, ordered by key.
    pub fn snapshot(&self) -> Vec<DeviceState> {
        self.states.values().cloned().collect()
    }

    /// When the next `Tick` is due: the earliest engine deadline, grace expiry or charging
    /// device's read ([`Hub::charging_reads`]).
    pub fn next_deadline(&self) -> Option<Ms> {
        self.engines
            .values()
            .filter_map(|slot| slot.engine.next_deadline())
            .chain(self.grace.values().copied())
            .chain(self.charging_reads().map(|(_, at)| at))
            .min()
    }

    /// Devices asked during use that are owned, online and charging, each with when the
    /// scheduler could next grant an opted-in charging read.
    fn charging_reads(&self) -> impl Iterator<Item = (&DeviceKey, Ms)> {
        self.states.values().filter_map(|state| {
            self.charging_owner(state)?;
            Some((&state.key, self.scheduler.charging_ready_at(&state.key)?))
        })
    }

    fn charging_owner(&self, state: &DeviceState) -> Option<GroupId> {
        if state.presence != Presence::Online || state.charge != Charge::Charging {
            return None;
        }
        self.device_owner(&state.key)
    }

    /// The registered devices `group`'s engine currently owns, ordered by key: those an
    /// [`HubInput::Activity`] on the group's physical device would reach.
    pub fn keys_of(&self, group: GroupId) -> Vec<DeviceKey> {
        self.owners
            .iter()
            .filter(|(_, groups)| groups.last().is_some_and(|owner| owner.group == group))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// When an [`HubInput::Activity`] for `key` may next be granted. Awake-only
    /// devices never set an audio deadline, even while online.
    pub fn audio_ready_at(&self, key: &DeviceKey) -> Option<Ms> {
        self.scheduler.activity_ready_at(key)
    }

    fn arrive(
        &mut self,
        group: &HidGroup,
        prepared: Option<&Selection>,
        now: Ms,
        pass: &mut Pass,
    ) {
        // The same id again means the old handles are gone.
        self.remove_group(group.id, now, pass);
        let resolved;
        let (engine, selection): (Box<dyn Engine>, Option<&Selection>) =
            if let Some(factory) = self.factory.as_ref().filter(|_| prepared.is_none()) {
                let Some(engine) = factory(group, &self.catalog) else {
                    return;
                };
                (engine, None)
            } else {
                let selection = if let Some(selection) = prepared {
                    selection
                } else {
                    resolved = engines::select(group, &self.catalog);
                    let Some(selection) = resolved.as_ref() else {
                        return;
                    };
                    selection
                };
                let Some(engine) = selection.engine(group, &self.catalog) else {
                    return;
                };
                (engine, Some(selection))
            };
        let key = engine.group_key();
        let mut collections = Vec::new();
        let mut routed = group
            .collections
            .iter()
            .filter(|c| selection.is_none_or(|selection| selection.opens(c.id)))
            .peekable();
        let first = routed.peek().copied();
        let direct = if let Some(selection) = selection {
            selection.direct()
        } else {
            // Custom engines supply their own routes; a gated entry still requires selection.
            first.and_then(|c| {
                let mut entries = self.catalog.direct_entries(c.vid, c.pid);
                let (index, entry) = entries.next()?;
                (entries.next().is_none() && entry.receive.is_none() && entry.send.is_none())
                    .then_some(index)
            })
        };
        for c in routed {
            collections.push(c.id);
            let (reads, role) = selection.map_or_else(
                || (true, engine.collection_role(c.id)),
                |selection| (selection.reads(c.id), selection.send_role(c.id)),
            );
            self.routes.insert(
                c.id,
                Route {
                    group: group.id,
                    reads,
                    role,
                    output_len: c.output_len,
                    feature_len: c.feature_len,
                },
            );
        }
        self.engines.insert(
            group.id,
            Slot {
                engine,
                key: key.clone(),
                vid: first.map(|c| c.vid),
                kind: selection.map_or(EngineKind::Hidpp, Selection::kind),
                direct,
                product: first.map(|c| c.product.clone()).unwrap_or_default(),
                collections,
                aliases: BTreeMap::new(),
            },
        );
        self.scheduler.register(key.clone(), GROUP_CAPS);
        self.grant(group.id, &key, Trigger::Arrival, now, pass);
    }

    /// Drops the engine; devices no other group serves go offline and enter the removal grace.
    fn remove_group(&mut self, id: GroupId, now: Ms, pass: &mut Pass) {
        let Some(slot) = self.engines.remove(&id) else {
            return;
        };
        for c in &slot.collections {
            if self.routes.get(c).is_some_and(|route| route.group == id) {
                self.routes.remove(c);
            }
        }
        // Group keys have no grace: a new arrival is a new conversation.
        self.scheduler.unregister(&slot.key);
        let served: Vec<DeviceKey> = self
            .owners
            .iter()
            .filter(|(_, groups)| groups.iter().any(|r| r.group == id))
            .map(|(key, _)| key.clone())
            .collect();
        for key in served {
            self.drop_registrant(&key, id, now, pass);
        }
    }

    /// `gid` no longer serves `key`. Ownership falls back to the latest
    /// remaining registrant and its capabilities, without changing battery values
    /// or read history. With none left, the device goes offline and is kept until
    /// the grace expires; a reconnect within grace cannot bypass the min gap or get a
    /// second arrival.
    fn drop_registrant(&mut self, key: &DeviceKey, gid: GroupId, now: Ms, pass: &mut Pass) {
        let Some(groups) = self.owners.get_mut(key) else {
            return;
        };
        let was_owner = groups.last().is_some_and(|owner| owner.group == gid);
        groups.retain(|r| r.group != gid);
        if was_owner {
            self.scheduler.revoke_grants(key);
        }
        if let Some(owner) = groups.last() {
            if was_owner {
                self.scheduler.set_capabilities(key, owner.caps);
                if let Some(state) = self.states.get_mut(key) {
                    pass.changed |= apply_polling_caps(state, owner.caps);
                }
            }
            return;
        }
        self.owners.remove(key);
        if let Some(state) = self.states.get_mut(key) {
            state.presence = Presence::Offline;
            pass.changed = true;
        }
        self.grace
            .insert(key.clone(), now.saturating_add(REMOVAL_GRACE_MS));
    }

    fn tick(&mut self, now: Ms, pass: &mut Pass) {
        let ids: Vec<GroupId> = self.engines.keys().copied().collect();
        for gid in ids {
            if let Some(slot) = self.engines.get_mut(&gid) {
                let actions = slot.engine.on_tick(now);
                self.apply(gid, actions, None, now, pass);
            }
        }
        let charging: Vec<DeviceKey> = self
            .charging_reads()
            .filter(|&(_, at)| now >= at)
            .map(|(key, _)| key.clone())
            .collect();
        for key in charging {
            // An earlier grant may have changed the charge, presence or owner.
            if let Some(gid) = self.states.get(&key).and_then(|s| self.charging_owner(s))
                && let Some(grant) = self.scheduler.request_charging(&key, gid, now)
            {
                self.deliver_grant(gid, grant, now, pass);
            }
        }
        let expired: Vec<DeviceKey> = self
            .grace
            .iter()
            .filter(|&(_, &until)| now >= until)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.grace.remove(&key);
            self.states.remove(&key);
            self.scheduler.unregister(&key);
            pass.changed = true;
        }
    }

    /// Requests one shot from each group that answers for itself, or from each
    /// registered device. Other groups without a device repeat discovery.
    fn request_all(&mut self, trigger: Trigger, now: Ms, pass: &mut Pass) {
        let devices: Vec<(DeviceKey, GroupId)> = self
            .owners
            .keys()
            .filter_map(|key| {
                let gid = self.device_owner(key)?;
                (!self.engines.get(&gid)?.engine.group_one_shots()).then(|| (key.clone(), gid))
            })
            .collect();
        for (key, gid) in devices {
            // An earlier grant's actions may have moved or released the device.
            if self.device_owner(&key) == Some(gid) {
                self.grant(gid, &key, trigger, now, pass);
            }
        }
        let groups: Vec<_> = self
            .engines
            .iter()
            .filter_map(|(&gid, slot)| {
                let one_shot = if slot.engine.group_one_shots() {
                    trigger
                } else if self.rediscoverable(gid) {
                    Trigger::Arrival
                } else {
                    return None;
                };
                Some((gid, slot.key.clone(), one_shot))
            })
            .collect();
        for (gid, key, one_shot) in groups {
            if one_shot != Trigger::Arrival {
                self.grant(gid, &key, one_shot, now, pass);
            } else if self.rediscoverable(gid) {
                self.scheduler.rearm_arrival(&key);
                self.grant(gid, &key, one_shot, now, pass);
            }
        }
    }

    /// Whether `gid`'s group may be discovered again: it is live, its engine registers no
    /// device, and its catalog entry does not say that a query wakes it.
    fn rediscoverable(&self, gid: GroupId) -> bool {
        let Some(slot) = self.engines.get(&gid) else {
            return false;
        };
        let query_wakes = slot
            .direct
            .and_then(|index| self.catalog.devices().get(index))
            .is_some_and(|device| device.quirks.query_wakes);
        !query_wakes
            && !self
                .owners
                .values()
                .any(|groups| groups.iter().any(|r| r.group == gid))
    }

    /// Requests a grant on `key` and, if the scheduler allows it, hands it to `gid`'s engine.
    fn grant(&mut self, gid: GroupId, key: &DeviceKey, trigger: Trigger, now: Ms, pass: &mut Pass) {
        // An engine that asks again from inside `on_grant` must not recurse without end.
        if pass.depth > MAX_NESTED_GRANTS || self.owner(key) != Some(gid) {
            return;
        }
        let Some(grant) = self.scheduler.request(key, gid, trigger, now) else {
            return;
        };
        self.deliver_grant(gid, grant, now, pass);
    }

    fn deliver_grant(&mut self, gid: GroupId, mut grant: Grant, now: Ms, pass: &mut Pass) {
        if let Some(slot) = self.engines.get_mut(&gid) {
            let subject = slot.engine_key(&grant.subject);
            if subject != &grant.subject {
                grant.subject = subject.clone();
            }
            let actions = slot.engine.on_grant(&grant, now);
            pass.depth += 1;
            self.apply(gid, actions, None, now, pass);
            pass.depth -= 1;
        }
    }

    /// Group whose engine currently owns device `key`: its latest live registrant.
    fn device_owner(&self, key: &DeviceKey) -> Option<GroupId> {
        self.owners
            .get(key)
            .and_then(|groups| groups.last().map(|owner| owner.group))
    }

    /// Group whose engine receives grants on `key`: the registrant of a
    /// device, or the engine a group key belongs to.
    fn owner(&self, key: &DeviceKey) -> Option<GroupId> {
        self.device_owner(key).or_else(|| {
            self.engines
                .iter()
                .find(|(_, slot)| slot.key == *key)
                .map(|(&gid, _)| gid)
        })
    }

    /// The hub key of the device `gid`'s engine calls `key`.
    fn hub_key(&self, gid: GroupId, key: DeviceKey) -> DeviceKey {
        self.engines
            .get(&gid)
            .and_then(|slot| slot.aliases.get(&key))
            .cloned()
            .unwrap_or(key)
    }

    /// The hub key `gid`'s engine registers `key` under. A key that may be shared
    /// ([`engines::shared_key`]) held by another live group becomes the first `key#n`,
    /// n ≥ 2, no other live group holds; any other key is kept as is, so one device served
    /// by several groups stays one entry. Recomputed on every registration: a device that
    /// got `#2` keeps it until it registers again. A device `gid` alone served under an
    /// earlier hub key for `key` moves to the new key with its last values, replacing what
    /// another device left there in grace; one shared with other groups loses `gid` as a
    /// registrant instead.
    fn claim(&mut self, gid: GroupId, key: DeviceKey, now: Ms, pass: &mut Pass) -> DeviceKey {
        if !engines::shared_key(&key) {
            return key;
        }
        let held_elsewhere = |k: &DeviceKey| {
            self.owners
                .get(k)
                .is_some_and(|groups| groups.iter().any(|r| r.group != gid))
        };
        let mut claimed = key.clone();
        let mut n = 1;
        while held_elsewhere(&claimed) {
            n += 1;
            claimed = DeviceKey(format!("{}#{n}", key.0));
        }
        let previous = self.hub_key(gid, key.clone());
        if previous != claimed {
            if self
                .owners
                .get(&previous)
                .is_some_and(|groups| matches!(groups.as_slice(), [r] if r.group == gid))
            {
                self.owners.remove(&previous);
                self.grace.remove(&previous);
                self.scheduler.unregister(&previous);
                if self.grace.remove(&claimed).is_some() {
                    self.scheduler.unregister(&claimed);
                }
                if let Some(mut state) = self.states.remove(&previous) {
                    state.key = claimed.clone();
                    self.states.insert(claimed.clone(), state);
                }
                pass.changed = true;
            } else {
                self.drop_registrant(&previous, gid, now, pass);
            }
        }
        if let Some(slot) = self.engines.get_mut(&gid) {
            if claimed == key {
                slot.aliases.remove(&key);
            } else {
                slot.aliases.insert(key, claimed.clone());
            }
        }
        claimed
    }

    fn admit_write(
        &mut self,
        gid: GroupId,
        grant: GrantId,
        request: SendRequest<'_>,
        now: Ms,
    ) -> Result<Option<Ms>, WriteRefusal> {
        let SendRequest {
            collection,
            kind,
            bytes,
            deadline,
        } = request;
        let route = self
            .routes
            .get(&collection)
            .filter(|route| route.group == gid)
            .ok_or(WriteRefusal::Collection)?;
        let slot = self.engines.get(&gid).ok_or(WriteRefusal::Collection)?;
        let role = route
            .role
            .filter(|&role| slot.engine.collection_role(collection) == Some(role))
            .ok_or(WriteRefusal::Collection)?;
        let width = match kind {
            ReportKind::Output => route.output_len,
            ReportKind::SetFeature | ReportKind::GetFeature => route.feature_len,
        };
        if width == 0 || bytes.len() != usize::from(width) {
            return Err(WriteRefusal::Width);
        }
        let permit = self
            .scheduler
            .pending_send(grant, gid, now)
            .ok_or(WriteRefusal::Grant)?;
        let subject = permit.subject();
        if self
            .owners
            .get(subject)
            .and_then(|groups| groups.last())
            .map_or(&slot.key != subject, |owner| owner.group != gid)
        {
            return Err(WriteRefusal::Grant);
        }
        match write_allowed(&*slot.engine, role, kind, bytes, grant).ok_or(WriteRefusal::Rule)? {
            WriteDestination::Collection => {}
            WriteDestination::Device(key) => {
                let key = slot.aliases.get(key).unwrap_or(key);
                if (subject != &slot.key && subject != key)
                    || self
                        .owners
                        .get(key)
                        .and_then(|groups| groups.last())
                        .is_none_or(|owner| owner.group != gid)
                {
                    return Err(WriteRefusal::Grant);
                }
            }
            WriteDestination::Group if subject != &slot.key => return Err(WriteRefusal::Grant),
            WriteDestination::Group => {}
        }
        let deadline = match kind {
            ReportKind::Output => None,
            ReportKind::SetFeature | ReportKind::GetFeature => Some(
                deadline
                    .map(|deadline| deadline.min(permit.expires_ms()))
                    .filter(|&deadline| deadline > now)
                    .ok_or(WriteRefusal::Deadline)?,
            ),
        };
        permit.spend();
        Ok(deadline)
    }

    /// Carries out `actions` from `gid`'s engine. Actions on devices or
    /// collections the engine does not own cannot reach the device.
    fn apply(
        &mut self,
        gid: GroupId,
        actions: Vec<Action>,
        origin: Option<ReportOrigin>,
        now: Ms,
        pass: &mut Pass,
    ) {
        for action in actions {
            match action {
                Action::Send {
                    grant,
                    collection,
                    kind,
                    bytes,
                    deadline,
                } => {
                    let request = SendRequest {
                        collection,
                        kind,
                        bytes: &bytes,
                        deadline,
                    };
                    match self.admit_write(gid, grant, request, now) {
                        Ok(deadline) => pass.out.push(HubOutput::Send {
                            collection,
                            kind,
                            bytes,
                            deadline,
                        }),
                        Err(reason) => pass.out.push(HubOutput::WriteRefused {
                            kind,
                            collection,
                            reason,
                        }),
                    }
                }
                Action::Register {
                    key,
                    caps,
                    initial,
                    wpid,
                } => {
                    let registration = Registration {
                        key,
                        caps,
                        initial,
                        wpid,
                    };
                    self.register(gid, registration, now, pass);
                }
                Action::Unregister(key) => {
                    let key = self.hub_key(gid, key);
                    self.drop_registrant(&key, gid, now, pass);
                }
                Action::Update(mut update) => {
                    update.key = self.hub_key(gid, update.key);
                    if self.device_owner(&update.key) != Some(gid) {
                        continue;
                    }
                    self.note_presence(&update.key, update.presence);
                    // A fresh level (pushed or queried) is as good as an activity read.
                    if update.level.is_some() {
                        self.scheduler.note_read(&update.key, now);
                    }
                    if let Some(state) = self.states.get_mut(&update.key) {
                        state.apply(&update, now);
                        if update.presence.is_none() {
                            state.presence = Presence::Online;
                        }
                        pass.changed = true;
                    }
                }
                Action::Presence { key, presence } => {
                    let key = self.hub_key(gid, key);
                    if self.device_owner(&key) != Some(gid) {
                        continue;
                    }
                    self.note_presence(&key, Some(presence));
                    if let Some(state) = self.states.get_mut(&key) {
                        pass.changed |= state.presence != presence;
                        state.presence = presence;
                        if presence == Presence::Offline {
                            pass.changed |= state.charge != Charge::Unknown;
                            state.charge = Charge::Unknown;
                        }
                    }
                }
                Action::Asleep(key) => {
                    let key = self.hub_key(gid, key);
                    if self.device_owner(&key) != Some(gid) {
                        continue;
                    }
                    self.scheduler.mark_asleep(&key);
                    if let Some(state) = self.states.get_mut(&key)
                        && state.presence != Presence::Asleep
                    {
                        state.presence = Presence::Asleep;
                        pass.changed = true;
                    }
                }
                Action::Connected(key) => {
                    let key = self.hub_key(gid, key);
                    if self.owner(&key) != Some(gid) {
                        continue;
                    }
                    // A connect is a sign of life even when the scheduler refuses the grant.
                    self.mark_online(&key, pass);
                    self.grant(gid, &key, Trigger::Connected, now, pass);
                }
                Action::Awake(key) => {
                    if origin != Some(ReportOrigin::Input) {
                        continue;
                    }
                    let key = self.hub_key(gid, key);
                    if self.device_owner(&key) != Some(gid) {
                        continue;
                    }
                    // A valid signal proves presence even before a read is due.
                    self.mark_online(&key, pass);
                    self.grant(gid, &key, Trigger::Awake, now, pass);
                }
                Action::Rediscover => {
                    if self.rediscoverable(gid)
                        && let Some(key) = self.engines.get(&gid).map(|slot| slot.key.clone())
                    {
                        self.grant(gid, &key, Trigger::Rediscovery, now, pass);
                    }
                }
                Action::DiscoveryFailed { retry_at } => {
                    if let Some(slot) = self.engines.get(&gid) {
                        pass.out.push(HubOutput::DiscoveryFailed {
                            group: slot.key.clone(),
                            retry_at,
                        });
                    }
                }
            }
        }
    }

    /// Registers a device for `gid`'s engine. A key known from an earlier
    /// registration (e.g. a reconnect within grace) keeps its entry, its last
    /// values and its scheduler history. A catalog entry's name replaces the
    /// name the device reports.
    fn register(&mut self, gid: GroupId, registration: Registration, now: Ms, pass: &mut Pass) {
        let Registration {
            key,
            caps,
            initial,
            wpid,
        } = registration;
        if !self.engines.contains_key(&gid) {
            return;
        }
        let key = self.claim(gid, key, now, pass);
        let Some(slot) = self.engines.get(&gid) else {
            return;
        };
        let behind_receiver = initial.via == Some(Via::Receiver);
        let (verification, catalog_name) = self.identity(slot, behind_receiver, wpid);
        let product = slot.product.clone();

        let groups = self.owners.entry(key.clone()).or_default();
        if groups.last().is_none_or(|owner| owner.group != gid) {
            // A restored owner cannot resume a conversation issued before the handoff.
            self.scheduler.revoke_grants(&key);
        }
        groups.retain(|r| r.group != gid);
        groups.push(Registrant { group: gid, caps });
        self.grace.remove(&key);
        self.scheduler.register(key.clone(), caps);
        // The discovery that registered the device read its battery: audio already playing
        // waits the device's gap before asking again.
        self.scheduler.note_read(&key, now);
        self.note_presence(&key, initial.presence);

        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| DeviceState {
                key,
                name: product,
                // Engines are expected to fill these; the defaults only cover a sparse `initial`.
                kind: DeviceKind::Other,
                via: Via::Usb,
                level: None,
                millivolts: None,
                charge: Charge::Unknown,
                presence: Presence::Online,
                updated_ms: now,
                source: initial.source,
                verification,
                poll_min_ms: None,
                reports_changes: false,
            });
        state.verification = verification;
        apply_polling_caps(state, caps);
        state.apply(&initial, now);
        if let Some(name) = catalog_name {
            state.name = name;
        }
        if initial.presence.is_none() {
            state.presence = Presence::Online;
        }
        pass.changed = true;
    }

    /// Sign of life only: the last battery reading and its age do not change.
    fn mark_online(&mut self, key: &DeviceKey, pass: &mut Pass) {
        self.scheduler.mark_awake(key);
        if let Some(state) = self.states.get_mut(key)
            && state.presence != Presence::Online
        {
            state.presence = Presence::Online;
            pass.changed = true;
        }
    }

    /// A reading is a sign of life unless it says the device is away.
    fn note_presence(&mut self, key: &DeviceKey, presence: Option<Presence>) {
        match presence {
            Some(Presence::Asleep | Presence::Offline) => self.scheduler.mark_asleep(key),
            Some(Presence::Online) | None => self.scheduler.mark_awake(key),
        }
    }

    /// HID++ receiver slots use their wireless PID. Dedicated devices, including
    /// a dongle or base station, use the exact direct entry.
    /// Both fall back to the selected engine's family status, if present.
    fn identity(
        &self,
        slot: &Slot,
        behind_receiver: bool,
        wpid: Option<u16>,
    ) -> (Verification, Option<String>) {
        let Some(vid) = slot.vid else {
            return (Verification::Generic, None);
        };
        let entry = if behind_receiver && slot.kind == EngineKind::Hidpp {
            wpid.and_then(|wpid| self.catalog.device_by_wpid(vid, wpid))
        } else {
            slot.direct.and_then(|index| self.catalog.devices().get(index))
        };
        if let Some(device) = entry {
            return (device.status, Some(device.name.clone()));
        }
        let status = self
            .catalog
            .family(vid, slot.kind)
            .map_or(Verification::Generic, |family| family.status);
        (status, None)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::rc::Rc;

    use super::*;
    use crate::engine::{BytePattern, WriteRule};
    use crate::hid::{Collection, CollectionRole};
    use crate::model::{DeviceKind, DeviceUpdate, Level, Presence, Source, Verification, Via};
    use crate::scheduler::{Capabilities, Grant, Trigger};

    /// Stands for "the grant being delivered" in scripted `on_grant` sends;
    /// the scheduler never issues id 0.
    const THIS_GRANT: GrantId = GrantId(0);

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Grant(Grant),
        Report(CollectionId, ReportOrigin, Vec<u8>),
        Tick(Ms),
    }

    /// Script and call log of one [`FakeEngine`], shared with the test.
    #[derive(Default)]
    struct Script {
        on_grant: VecDeque<Vec<Action>>,
        on_report: VecDeque<Vec<Action>>,
        on_tick: VecDeque<Vec<Action>>,
        deadline: Option<Ms>,
        /// Report `Connected(group key)` from every `on_grant`.
        connected_on_grant: bool,
        calls: Vec<Call>,
    }

    type Fake = Rc<RefCell<Script>>;

    /// Records every call and replays the next scripted action list per call.
    struct FakeEngine {
        key: DeviceKey,
        out: CollectionId,
        script: Fake,
    }

    const COMMAND: CollectionRole = CollectionRole(0);

    const fn fake_byte_rule(bytes: &'static [u8]) -> WriteRule {
        WriteRule::Bytes {
            role: COMMAND,
            kind: ReportKind::Output,
            wire_len: 1,
            pattern: BytePattern::Exact(bytes),
        }
    }

    impl Engine for FakeEngine {
        fn group_key(&self) -> DeviceKey {
            self.key.clone()
        }

        fn collection_role(&self, collection: CollectionId) -> Option<CollectionRole> {
            (self.out == collection).then_some(COMMAND)
        }

        fn allowed_writes(&self) -> &'static [WriteRule] {
            const RULES: &[WriteRule] = &[
                fake_byte_rule(&[0]),
                fake_byte_rule(&[1]),
                fake_byte_rule(&[2]),
                fake_byte_rule(&[3]),
                fake_byte_rule(&[4]),
                fake_byte_rule(&[5]),
                fake_byte_rule(&[0xA1]),
                fake_byte_rule(&[0xA2]),
                fake_byte_rule(&[0xB0]),
                fake_byte_rule(&[0xC0]),
                fake_byte_rule(&[0xC2]),
                fake_byte_rule(&[0xD0]),
                fake_byte_rule(&[0xD2]),
                WriteRule::Bytes {
                    role: COMMAND,
                    kind: ReportKind::Output,
                    wire_len: 4,
                    pattern: BytePattern::ZeroPadded(&[0xA5, 0x42]),
                },
                WriteRule::Bytes {
                    role: COMMAND,
                    kind: ReportKind::GetFeature,
                    wire_len: 4,
                    pattern: BytePattern::Exact(&[0x31, 0, 0, 0]),
                },
            ];
            RULES
        }

        fn on_grant(&mut self, grant: &Grant, _now: Ms) -> Vec<Action> {
            let mut s = self.script.borrow_mut();
            s.calls.push(Call::Grant(grant.clone()));
            let mut actions = s.on_grant.pop_front().unwrap_or_default();
            for action in &mut actions {
                if let Action::Send { grant: id, .. } = action
                    && *id == THIS_GRANT
                {
                    *id = grant.id;
                }
            }
            if s.connected_on_grant {
                actions.push(Action::Connected(self.key.clone()));
            }
            actions
        }

        fn on_report(
            &mut self,
            collection: CollectionId,
            origin: ReportOrigin,
            bytes: &[u8],
            _now: Ms,
        ) -> Vec<Action> {
            let mut s = self.script.borrow_mut();
            s.calls.push(Call::Report(collection, origin, bytes.to_vec()));
            s.on_report.pop_front().unwrap_or_default()
        }

        fn on_tick(&mut self, now: Ms) -> Vec<Action> {
            let mut s = self.script.borrow_mut();
            s.calls.push(Call::Tick(now));
            s.on_tick.pop_front().unwrap_or_default()
        }

        fn next_deadline(&self) -> Option<Ms> {
            self.script.borrow().deadline
        }
    }

    const CATALOG: &str = r#"
[[family]]
engine = "hidpp"
vid = 0x046D
status = "source-verified"
source = "test"

[[device]]
name = "PRO X Wireless"
vid = 0x046D
pids = [0x0ABA]
kind = "headset"
engine = "hidpp"
status = "hardware-tested"
source = "test"

[[device]]
name = "PRO X Superlight"
vid = 0x046D
wpids = [0x4093]
kind = "mouse"
engine = "hidpp"
status = "hardware-tested"
source = "test"

[[device]]
name = "PRO X Superlight"
vid = 0x046D
pids = [0xC094]
kind = "mouse"
engine = "hidpp"
status = "source-verified"
source = "test"

[[device]]
name = "Wakes On Query"
vid = 0x046D
pids = [0x0AAB]
kind = "headset"
engine = "hidpp"
status = "source-verified"
source = "test"
quirks = { query_wakes = true }
"#;

    fn fake() -> Fake {
        Rc::default()
    }

    fn group_key(id: u32) -> DeviceKey {
        DeviceKey(format!("fake-group:{id}"))
    }

    fn dev(name: &str) -> DeviceKey {
        DeviceKey(name.into())
    }

    /// A hub whose factory builds a [`FakeEngine`] for each listed group id.
    fn hub_with(policy: Policy, fakes: &[(u32, &Fake)]) -> Hub {
        let map: HashMap<GroupId, Fake> = fakes
            .iter()
            .map(|(id, f)| (GroupId(*id), Rc::clone(f)))
            .collect();
        Hub::new(Catalog::parse(CATALOG).unwrap(), policy).with_engine_factory(Box::new(
            move |group, _| {
                let script = map.get(&group.id)?;
                Some(Box::new(FakeEngine {
                    key: group_key(group.id.0),
                    out: coll(group.id.0),
                    script: Rc::clone(script),
                }) as Box<dyn Engine>)
            },
        ))
    }

    fn hub(fakes: &[(u32, &Fake)]) -> Hub {
        hub_with(Policy::default(), fakes)
    }

    fn coll(group: u32) -> CollectionId {
        CollectionId(group * 10)
    }

    /// Group `id` with a single collection [`coll`]`(id)`.
    fn group(id: u32, vid: u16, pid: u16) -> HidGroup {
        HidGroup {
            id: GroupId(id),
            collections: vec![Collection {
                id: coll(id),
                vid,
                pid,
                usage_page: 0xFF00,
                usage: 0x0002,
                interface: None,
                input_len: 20,
                output_len: 1,
                feature_len: 0,
                product: "Fake Receiver".into(),
            }],
        }
    }

    fn logi(id: u32) -> HubInput {
        HubInput::GroupArrived(group(id, 0x046D, 0xC547))
    }

    fn report(group: u32) -> HubInput {
        HubInput::Report {
            collection: coll(group),
            origin: ReportOrigin::Input,
            bytes: vec![0x11],
        }
    }

    fn send(grant: GrantId, collection: CollectionId, byte: u8) -> Action {
        Action::Send {
            grant,
            collection,
            kind: ReportKind::Output,
            bytes: vec![byte],
            deadline: None,
        }
    }

    fn update(key: &DeviceKey) -> DeviceUpdate {
        DeviceUpdate {
            key: key.clone(),
            name: None,
            kind: None,
            via: None,
            level: None,
            millivolts: None,
            charge: None,
            presence: None,
            source: Source::Push,
        }
    }

    fn level_update(key: &DeviceKey, percent: u8) -> Action {
        Action::Update(DeviceUpdate {
            level: Some(Level::Percent(percent)),
            ..update(key)
        })
    }

    /// A device that pushes its battery. Its gap would allow activity reads, so only the
    /// pushing rule refuses them.
    fn pushing() -> Capabilities {
        Capabilities {
            pushes_battery: true,
            query_wakes: false,
            audio: false,
            awake_signal: false,
            charging: false,
            min_query_gap_ms: 300_000,
        }
    }

    fn register_with(key: &DeviceKey, caps: Capabilities, level: Option<Level>) -> Action {
        Action::Register {
            key: key.clone(),
            caps,
            initial: DeviceUpdate {
                name: Some("Mouse".into()),
                kind: Some(DeviceKind::Mouse),
                via: Some(Via::Receiver),
                level,
                source: Source::Query,
                ..update(key)
            },
            wpid: None,
        }
    }

    /// `register(key, None)` arriving `via`, with wireless PID `wpid`.
    fn register_as(key: &DeviceKey, via: Via, wpid: Option<u16>) -> Action {
        let Action::Register {
            key,
            caps,
            mut initial,
            ..
        } = register(key, None)
        else {
            unreachable!()
        };
        initial.via = Some(via);
        Action::Register {
            key,
            caps,
            initial,
            wpid,
        }
    }

    fn register(key: &DeviceKey, level: Option<Level>) -> Action {
        register_with(key, pushing(), level)
    }

    fn sends(out: &[HubOutput]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|o| match o {
                HubOutput::Send { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .collect()
    }

    fn state_changes(out: &[HubOutput]) -> usize {
        out.iter()
            .filter(|o| **o == HubOutput::StateChanged)
            .count()
    }

    fn grants(f: &Fake) -> Vec<(DeviceKey, Trigger)> {
        f.borrow()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Grant(g) => Some((g.subject.clone(), g.trigger)),
                _ => None,
            })
            .collect()
    }

    fn state(hub: &Hub, key: &DeviceKey) -> Option<DeviceState> {
        hub.snapshot().into_iter().find(|s| s.key == *key)
    }

    #[test]
    fn arrival_grant_reaches_engine_and_sends_are_emitted() {
        let f = fake();
        f.borrow_mut().on_grant.push_back(vec![
            send(THIS_GRANT, coll(1), 0xA1),
            send(THIS_GRANT, coll(1), 0xA2),
        ]);
        let mut hub = hub(&[(1, &f)]);

        let out = hub.handle(logi(1), 0);

        assert_eq!(
            out,
            vec![
                HubOutput::Send {
                    collection: coll(1),
                    kind: ReportKind::Output,
                    bytes: vec![0xA1],
                    deadline: None,
                },
                HubOutput::Send {
                    collection: coll(1),
                    kind: ReportKind::Output,
                    bytes: vec![0xA2],
                    deadline: None,
                },
            ]
        );
        let calls = f.borrow().calls.clone();
        assert_eq!(calls.len(), 1);
        let Call::Grant(grant) = &calls[0] else {
            panic!("expected a grant, got {calls:?}");
        };
        assert_eq!(grant.subject, group_key(1));
        assert_eq!(grant.trigger, Trigger::Arrival);
        assert_eq!(grant.max_requests, Policy::default().discovery_budget);
    }

    #[test]
    fn send_without_valid_grant_is_dropped() {
        let f = fake();
        f.borrow_mut()
            .on_report
            .push_back(vec![send(GrantId(999), coll(1), 0xB0)]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(report(1), 5);

        assert!(sends(&out).is_empty(), "{out:?}");
        assert_eq!(
            f.borrow().calls.last(),
            Some(&Call::Report(coll(1), ReportOrigin::Input, vec![0x11]))
        );
    }

    #[test]
    fn rejected_requests_do_not_spend_the_one_request_budget() {
        let f = fake();
        let mut g = group(1, 0x046D, 0xC547);
        g.collections[0].output_len = 4;
        g.collections[0].feature_len = 4;
        let mut other = g.collections[0].clone();
        other.id = CollectionId(99);
        g.collections.push(other);
        let valid = [0xA5, 0x42, 0, 0];
        let request = |collection, kind, bytes: &[u8]| Action::Send {
            grant: THIS_GRANT,
            collection,
            kind,
            bytes: bytes.to_vec(),
            deadline: None,
        };
        f.borrow_mut().on_grant.push_back(vec![
            request(CollectionId(99), ReportKind::Output, &valid),
            request(coll(1), ReportKind::Output, &valid[..3]),
            request(coll(1), ReportKind::SetFeature, &valid),
            request(coll(1), ReportKind::GetFeature, &valid),
            request(coll(1), ReportKind::Output, &[0xA5, 0x43, 0, 0]),
            request(coll(1), ReportKind::Output, &[0xA5, 0x42, 0, 1]),
            request(coll(1), ReportKind::Output, &valid),
            request(coll(1), ReportKind::Output, &valid),
        ]);
        let mut hub = hub_with(
            Policy {
                discovery_budget: 1,
                ..Policy::default()
            },
            &[(1, &f)],
        );

        let out = hub.handle(HubInput::GroupArrived(g), 0);
        let refused = |kind, collection, reason| HubOutput::WriteRefused {
            kind,
            collection,
            reason,
        };
        assert_eq!(
            out,
            [
                refused(ReportKind::Output, CollectionId(99), WriteRefusal::Collection),
                refused(ReportKind::Output, coll(1), WriteRefusal::Width),
                refused(ReportKind::SetFeature, coll(1), WriteRefusal::Rule),
                refused(ReportKind::GetFeature, coll(1), WriteRefusal::Rule),
                refused(ReportKind::Output, coll(1), WriteRefusal::Rule),
                refused(ReportKind::Output, coll(1), WriteRefusal::Rule),
                HubOutput::Send {
                    collection: coll(1),
                    kind: ReportKind::Output,
                    bytes: valid.to_vec(),
                    deadline: None,
                },
                refused(ReportKind::Output, coll(1), WriteRefusal::Grant),
            ]
        );
    }

    #[test]
    fn steelseries_allowlist_rejects_wrong_role_id_opcode_kind_width_and_padding() {
        for (pid, interface, id, width, settle) in [
            (0x2202, 3, 0, 65, 1_500),
            (0x12E5, 4, 6, 64, 5_000),
            (0x2244, 3, 1, 64, 5_000),
        ] {
            let mut hub = Hub::new(Catalog::builtin(), Policy::default());
            let mut group = group(1, 0x1038, pid);
            let command = &mut group.collections[0];
            command.interface = Some(interface);
            command.usage_page = 0xFFC0;
            command.usage = 1;
            command.input_len = width;
            command.output_len = width;
            command.feature_len = width; // Descriptor width alone must not admit features.
            let events = CollectionId(99);
            let event_collection = Collection {
                id: events,
                interface: Some(if id == 0 { 5 } else { interface }),
                usage_page: 0xFF00,
                ..command.clone()
            };
            group.collections.push(event_collection);
            let gid = group.id;
            assert!(hub.handle(HubInput::GroupArrived(group), 0).is_empty());
            let key = hub.engines[&gid].key.clone();
            let grant = hub
                .scheduler
                .request(&key, gid, Trigger::RefreshNow, 0)
                .unwrap();

            let mut valid = vec![0; usize::from(width)];
            valid[..2].copy_from_slice(&[id, 0xB0]);
            let mut wrong_id = valid.clone();
            wrong_id[0] = id ^ 1;
            let mut nonzero_pad = valid.clone();
            *nonzero_pad.last_mut().unwrap() = 1;
            let mut wrong_op = valid.clone();
            wrong_op[1] = 0xB1; // Elite settings reset is never an allowed command.
            let output = ReportKind::Output;
            for (kind, collection, bytes, reason) in [
                (output, events, &valid[..], WriteRefusal::Collection),
                (output, coll(1), &valid[..2], WriteRefusal::Width),
                (output, coll(1), &wrong_id, WriteRefusal::Rule),
                (output, coll(1), &wrong_op, WriteRefusal::Rule),
                (output, coll(1), &nonzero_pad, WriteRefusal::Rule),
                (ReportKind::GetFeature, coll(1), &valid, WriteRefusal::Rule),
                (ReportKind::SetFeature, coll(1), &valid, WriteRefusal::Rule),
            ] {
                assert_eq!(
                    hub.admit_write(
                        gid,
                        grant.id,
                        SendRequest {
                            collection,
                            kind,
                            bytes,
                            deadline: Some(100),
                        },
                        0,
                    ),
                    Err(reason)
                );
            }
            assert_eq!(sends(&hub.handle(HubInput::Tick, settle)), [valid]);
        }
    }

    #[test]
    fn feature_deadline_uses_the_request_not_an_earlier_timer_and_is_capped_by_grant() {
        for (requested, expected) in [(180, 180), (500, 200)] {
            let f = fake();
            f.borrow_mut().deadline = Some(105); // The same engine also has an earlier timer.
            let request = |deadline| Action::Send {
                grant: THIS_GRANT,
                collection: coll(1),
                kind: ReportKind::GetFeature,
                bytes: vec![0x31, 0, 0, 0],
                deadline,
            };
            f.borrow_mut().on_grant.push_back(vec![
                request(None),
                request(Some(100)),
                request(Some(requested)),
            ]);
            let mut g = group(1, 0x046D, 0xC547);
            g.collections[0].feature_len = 4;
            let mut hub = hub_with(
                Policy {
                    discovery_budget: 1,
                    discovery_ms: 100,
                    ..Policy::default()
                },
                &[(1, &f)],
            );
            let refused = HubOutput::WriteRefused {
                collection: coll(1),
                kind: ReportKind::GetFeature,
                reason: WriteRefusal::Deadline,
            };
            assert_eq!(
                hub.handle(HubInput::GroupArrived(g), 100),
                [
                    refused.clone(),
                    refused,
                    HubOutput::Send {
                        collection: coll(1),
                        kind: ReportKind::GetFeature,
                        bytes: vec![0x31, 0, 0, 0],
                        deadline: Some(expected),
                    }
                ]
            );
            assert_eq!(hub.next_deadline(), Some(105));
        }
    }

    #[test]
    fn foreign_and_expired_grants_cannot_send_to_a_live_collection() {
        let (a, b) = (fake(), fake());
        let mut hub = hub_with(
            Policy {
                discovery_ms: 100,
                discovery_budget: 1,
                ..Policy::default()
            },
            &[(1, &a), (2, &b)],
        );
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);
        let Call::Grant(first) = a.borrow().calls[0].clone() else {
            panic!("expected arrival grant");
        };
        b.borrow_mut()
            .on_report
            .push_back(vec![send(first.id, coll(2), 0xA1)]);
        let foreign = hub.handle(report(2), 1);
        a.borrow_mut()
            .on_report
            .push_back(vec![send(first.id, coll(1), 0xA1)]);
        let original = hub.handle(report(1), 100);
        let Call::Grant(second) = b.borrow().calls[0].clone() else {
            panic!("expected arrival grant");
        };
        b.borrow_mut()
            .on_report
            .push_back(vec![send(second.id, coll(2), 0xA1)]);
        let expired = hub.handle(report(2), 101);

        assert!(sends(&foreign).is_empty(), "{foreign:?}");
        assert_eq!(sends(&original), vec![vec![0xA1]]);
        assert!(sends(&expired).is_empty(), "{expired:?}");
    }

    #[test]
    fn ownership_changes_revoke_a_delayed_device_grant_for_both_engines() {
        let (a, b) = (fake(), fake());
        let key = dev("shared-unit");
        a.borrow_mut().on_grant.push_back(vec![register(&key, None)]);
        let mut hub = hub(&[(1, &a), (2, &b)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);
        hub.handle(HubInput::RefreshNow, 1);
        let Call::Grant(grant) = a.borrow().calls.last().unwrap().clone() else {
            panic!("expected device refresh grant");
        };
        assert_eq!(grant.subject, key);
        b.borrow_mut().on_report.push_back(vec![register(&key, None)]);
        hub.handle(report(2), 2);
        assert_eq!(hub.keys_of(GroupId(2)), vec![key.clone()]);
        for (id, engine) in [(1, &a), (2, &b)] {
            engine
                .borrow_mut()
                .on_report
                .push_back(vec![send(grant.id, coll(id), 0xA1)]);
            let out = hub.handle(report(id), 3);
            assert!(sends(&out).is_empty(), "{out:?}");
        }
        hub.handle(HubInput::GroupRemoved(GroupId(2)), 4);
        assert_eq!(hub.keys_of(GroupId(1)), vec![key]);
        a.borrow_mut()
            .on_report
            .push_back(vec![send(grant.id, coll(1), 0xA1)]);
        let out = hub.handle(report(1), 5);
        assert!(sends(&out).is_empty(), "{out:?}");
    }

    fn hidpp_frame(head: &[u8]) -> Vec<u8> {
        let mut bytes = head.to_vec();
        bytes.resize(20, 0);
        bytes
    }

    fn hidpp_group(id: u32, receiver: bool) -> HidGroup {
        let mut group = group(id, 0x046D, 0xC547);
        group.collections[0].output_len = 20;
        if !receiver {
            group.collections[0].product = "Fake USB Mouse".into();
        }
        group
    }

    fn hidpp_receiver() -> HubInput {
        HubInput::GroupArrived(hidpp_group(1, true))
    }

    fn hidpp_reply(hub: &mut Hub, head: &[u8]) -> Vec<HubOutput> {
        hidpp_report(hub, 1, head)
    }

    fn hidpp_report(hub: &mut Hub, group: u32, head: &[u8]) -> Vec<HubOutput> {
        hub.handle(
            HubInput::Report {
                collection: coll(group),
                origin: ReportOrigin::Input,
                bytes: hidpp_frame(head),
            },
            1,
        )
    }

    fn hidpp_try_send(hub: &mut Hub, head: &[u8]) -> Vec<HubOutput> {
        hidpp_group_send(hub, 1, GrantId(1), head)
    }

    fn hidpp_group_send(
        hub: &mut Hub,
        group: u32,
        grant: GrantId,
        head: &[u8],
    ) -> Vec<HubOutput> {
        let mut pass = Pass::default();
        hub.apply(
            GroupId(group),
            vec![Action::Send {
                grant,
                collection: coll(group),
                kind: ReportKind::Output,
                bytes: hidpp_frame(head),
                deadline: None,
            }],
            None,
            1,
            &mut pass,
        );
        pass.out
    }

    #[test]
    fn hidpp_does_not_route_a_volume_collection_from_the_same_group() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        let mut group = hidpp_group(1, true);
        let volume = CollectionId(31);
        let mut control = group.collections[0].clone();
        control.id = volume;
        (control.usage_page, control.usage) = (0x000C, 1);
        group.collections.push(control);
        assert!(!sends(&hub.handle(HubInput::GroupArrived(group), 0)).is_empty());

        let reply = [0x11, 0xFF, 0x81, 0, 0, 0, 0];
        let unrelated = hub.handle(
            HubInput::Report {
                collection: volume,
                origin: ReportOrigin::Input,
                bytes: hidpp_frame(&reply),
            },
            1,
        );
        assert!(sends(&unrelated).is_empty(), "{unrelated:?}");
        assert!(!sends(&hidpp_reply(&mut hub, &reply)).is_empty());
    }

    #[test]
    fn hidpp_feature_calls_use_the_discovered_feature_in_their_own_slot() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        hub.handle(hidpp_receiver(), 0);
        for reply in [
            &[0x10, 1, 0x41, 0, 0, 0, 0][..],
            &[0x11, 1, 0, 0x1B, 2],
            &[0x11, 1, 0, 0x0B, 5], // Name
            &[0x11, 1, 0, 0x0B, 3], // Unit ID
            &[0x11, 1, 0, 0x0B, 8], // Unified Battery
        ] {
            hidpp_reply(&mut hub, reply);
        }
        let read = [0x11, 1, 8, 0x1B];
        assert_eq!(sends(&hidpp_try_send(&mut hub, &read)), [hidpp_frame(&read)]);
        for frame in [
            [0x11, 1, 9, 0x1B, 0, 0], // Unknown feature index
            [0x11, 1, 3, 0x1B, 0, 0], // Known feature, forbidden function
            [0x11, 2, 8, 0x1B, 0, 0], // Same index in another slot
            [0x11, 1, 8, 0x1B, 1, 0], // Read with unapproved parameters
            [0x11, 1, 0, 0x0B, 0, 1], // Root lookup for an unused feature
            [0x11, 1, 5, 0x1B, 1, 0], // Name offset without the matching name read
        ] {
            let out = hidpp_try_send(&mut hub, &frame);
            assert!(sends(&out).is_empty(), "{frame:02X?}: {out:?}");
        }
        hidpp_reply(&mut hub, &[0x10, 1, 0x40, 2]);
        let out = hidpp_try_send(&mut hub, &read);
        assert!(sends(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn hidpp_notification_set_can_only_add_its_bit_to_the_matched_read() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        let get = [0x11, 0xFF, 0x81, 0];
        let set = [0x11, 0xFF, 0x80, 0, 0xA4, 0x21, 0x44];
        assert_eq!(sends(&hub.handle(hidpp_receiver(), 0)), [hidpp_frame(&get)]);
        let feature_reply = hub.handle(
            HubInput::Report {
                collection: coll(1),
                origin: ReportOrigin::FeatureReply,
                bytes: hidpp_frame(&[0x11, 0xFF, 0x81, 0, 0xA4, 0x20, 0x44]),
            },
            1,
        );
        assert!(sends(&feature_reply).is_empty(), "{feature_reply:?}");
        let unbacked = hidpp_try_send(&mut hub, &set);
        assert_eq!(
            sends(&hidpp_reply(&mut hub, &[0x11, 0xFF, 0x81, 0, 0xA4, 0x20, 0x44])),
            [hidpp_frame(&set)]
        );
        for frame in [
            [0x11, 0xFF, 0x80, 0, 0xA4, 0x23, 0x44],
            [0x11, 0xFF, 0x80, 0, 0xA0, 0x21, 0x44],
            [0x11, 1, 0x80, 0, 0xA4, 0x21, 0x44],
        ] {
            let out = hidpp_try_send(&mut hub, &frame);
            assert!(sends(&out).is_empty(), "{frame:02X?}: {out:?}");
        }
        let notify = [0x11, 0xFF, 0x80, 2, 2];
        assert_eq!(
            sends(&hidpp_reply(&mut hub, &[0x11, 0xFF, 0x80, 0])),
            [hidpp_frame(&notify)]
        );
        assert!(sends(&unbacked).is_empty(), "{unbacked:?}");
        for frame in [
            set, // Read evidence is no longer current.
            [0x11, 0xFF, 0x80, 2, 0x03, 0, 0],
            [0x11, 0xFF, 0x81, 0x09, 0, 0, 0],
            [0x11, 0xFF, 0x81, 0, 1, 0, 0],
            [0x11, 1, 0x80, 2, 2, 0, 0],
        ] {
            let out = hidpp_try_send(&mut hub, &frame);
            assert!(sends(&out).is_empty(), "{frame:02X?}: {out:?}");
        }
    }

    #[test]
    fn hidpp_reserved_index_from_root_never_emits_a_feature_call() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        hub.handle(hidpp_receiver(), 0);
        for reply in [
            &[0x10, 1, 0x41, 0, 0, 0, 0][..],
            &[0x11, 1, 0, 0x1B, 2],
            &[0x11, 1, 0, 0x0B, 0x80], // A malformed Root reply for the name feature.
            &[0x11, 1, 0, 0x0B, 0],    // No unit ID.
        ] {
            hidpp_reply(&mut hub, reply);
        }
        let out = hidpp_reply(&mut hub, &[0x11, 1, 0, 0x0B, 8]); // Unified battery.
        assert_eq!(
            out,
            [HubOutput::WriteRefused {
                collection: coll(1),
                kind: ReportKind::Output,
                reason: WriteRefusal::Rule,
            }]
        );
    }

    #[test]
    fn hidpp_receiver_slot_rejects_complemented_feature_replies_and_errors() {
        for foreign in [
            &[0x11, 0xFE, 0, 0x0B, 5][..],
            &[0x11, 0xFE, 0xFF, 0, 0x0B, 0x02],
            &[0x10, 0xFE, 0x8F, 0, 0x0B, 0x02, 0],
        ] {
            let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
            hub.handle(hidpp_receiver(), 0);
            hidpp_reply(&mut hub, &[0x10, 1, 0x41, 0, 0, 0, 0]);
            hidpp_reply(&mut hub, &[0x11, 1, 0, 0x1B, 2]);
            let out = hidpp_reply(&mut hub, foreign);
            assert!(out.is_empty(), "{foreign:02X?}: {out:?}");
            assert_eq!(
                sends(&hidpp_reply(&mut hub, &[0x11, 1, 0, 0x0B, 5])),
                [hidpp_frame(&[0x11, 1, 0, 0x0B, 0, 3])]
            );
        }
    }

    #[test]
    fn hidpp_receiver_slot_rejects_complemented_register_replies_and_errors() {
        for foreign in [
            &[0x11, 0xFE, 0x81, 0, 0xA4, 0x20, 0x44][..],
            &[0x10, 0xFE, 0x8F, 0x81, 0, 0x02, 0],
        ] {
            let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
            hub.handle(hidpp_receiver(), 0);
            hidpp_reply(&mut hub, &[0x10, 1, 0x41, 0, 0, 0, 0]);
            assert_eq!(
                sends(&hidpp_reply(&mut hub, &[0x10, 1, 0x8F, 0, 0x1B, 1, 0])),
                [hidpp_frame(&[0x11, 1, 0x81, 0])]
            );
            let out = hidpp_reply(&mut hub, foreign);
            assert!(out.is_empty(), "{foreign:02X?}: {out:?}");
            assert_eq!(
                sends(&hidpp_reply(&mut hub, &[0x11, 1, 0x81, 0, 0xA4, 0x20, 0x44])),
                [hidpp_frame(&[0x11, 1, 0x80, 0, 0xB4, 0x20, 0x44])]
            );
        }
    }

    #[test]
    fn hidpp_receiver_itself_does_not_accept_the_direct_device_reply_alias() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        hub.handle(hidpp_receiver(), 0);
        let out = hidpp_reply(&mut hub, &[0x11, 0, 0x81, 0, 0xA4, 0x20, 0x44]);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(
            sends(&hidpp_reply(&mut hub, &[0x11, 0xFF, 0x81, 0, 0xA4, 0x20, 0x44])),
            [hidpp_frame(&[0x11, 0xFF, 0x80, 0, 0xA4, 0x21, 0x44])]
        );
    }

    fn hidpp_discover_unified(hub: &mut Hub, group: u32, device: u8, reply: u8, unit: [u8; 4]) {
        let [a, b, c, d] = unit;
        if device != 0xFF {
            hidpp_report(hub, group, &[0x10, device, 0x41, 0, 0, 0, 0]);
        }
        for frame in [
            &[0x11, reply, 0, 0x1B, 2][..],
            &[0x11, reply, 0, 0x0B, 0], // No name feature.
            &[0x11, reply, 0, 0x0B, 3], // Unit ID at index 3.
            &[0x11, reply, 0, 0x0B, 8], // Unified battery at index 8.
            &[0x11, reply, 3, 0x0B, 0, a, b, c, d],
            &[0x11, reply, 8, 0x1B, 34, 4, 0, 0],
        ] {
            hidpp_report(hub, group, frame);
        }
    }

    #[test]
    fn hidpp_live_receiver_group_grant_cannot_read_a_device_owned_by_its_cable() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        let unit = [0x12, 0x34, 0x56, 0x78];
        let key = dev("logi:unit:12345678");
        hub.handle(hidpp_receiver(), 0);
        hidpp_discover_unified(&mut hub, 1, 1, 1, unit);
        assert_eq!(hub.keys_of(GroupId(1)), [key.clone()]);
        let read = [0x11, 1, 8, 0x1B];
        assert_eq!(sends(&hidpp_try_send(&mut hub, &read)), [hidpp_frame(&read)]);

        hub.handle(HubInput::GroupArrived(hidpp_group(2, false)), 1);
        // Direct devices can answer an FF request with address 00.
        hidpp_discover_unified(&mut hub, 2, 0xFF, 0, unit);
        assert!(hub.keys_of(GroupId(1)).is_empty());
        assert_eq!(hub.keys_of(GroupId(2)), [key]);

        let refused = hidpp_try_send(&mut hub, &read);
        let unknown_slot_ping = [0x11, 2, 0, 0x1B, 0, 0, 0x5A];
        assert_eq!(
            sends(&hidpp_try_send(&mut hub, &unknown_slot_ping)),
            [hidpp_frame(&unknown_slot_ping)],
            "the receiver group grant must still permit discovery of an unknown slot"
        );
        assert!(sends(&refused).is_empty(), "{refused:?}");
    }

    #[test]
    fn hidpp_receiver_group_grant_can_read_its_device_under_a_hub_alias() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        hub.handle(hidpp_receiver(), 0);
        hidpp_discover_unified(&mut hub, 1, 1, 1, [0; 4]);
        hub.handle(HubInput::GroupArrived(hidpp_group(2, true)), 1);
        hidpp_discover_unified(&mut hub, 2, 1, 1, [0; 4]);
        assert_eq!(hub.keys_of(GroupId(1)), [dev("logi:C547:1")]);
        assert_eq!(hub.keys_of(GroupId(2)), [dev("logi:C547:1#2")]);

        let read = [0x11, 1, 8, 0x1B];
        assert_eq!(
            sends(&hidpp_group_send(&mut hub, 2, GrantId(3), &read)),
            [hidpp_frame(&read)]
        );
    }

    fn with_hidpp_collection(group: &mut HidGroup, id: CollectionId, short: bool) {
        let mut collection = group.collections[0].clone();
        collection.id = id;
        if short {
            collection.usage = 1;
            collection.input_len = 7;
            collection.output_len = 7;
        }
        group.collections.push(collection);
    }

    #[test]
    fn hidpp_duplicate_short_command_collections_are_refused_without_long_fallback() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        let mut group = hidpp_group(1, true);
        with_hidpp_collection(&mut group, CollectionId(31), true);
        with_hidpp_collection(&mut group, CollectionId(32), true);
        let out = hub.handle(HubInput::GroupArrived(group), 0);
        assert!(sends(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn hidpp_duplicate_long_only_command_collections_are_refused() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        let mut group = hidpp_group(1, true);
        with_hidpp_collection(&mut group, CollectionId(31), false);
        let out = hub.handle(HubInput::GroupArrived(group), 0);
        assert!(sends(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn hidpp_unique_short_command_collection_wins_over_duplicate_longs() {
        let mut hub = Hub::new(Catalog::parse(CATALOG).unwrap(), Policy::default());
        let mut group = hidpp_group(1, true);
        with_hidpp_collection(&mut group, CollectionId(31), false);
        let short = CollectionId(32);
        with_hidpp_collection(&mut group, short, true);
        let out = hub.handle(HubInput::GroupArrived(group), 0);
        assert_eq!(
            out,
            [HubOutput::Send {
                collection: short,
                kind: ReportKind::Output,
                bytes: vec![0x10, 0xFF, 0x81, 0, 0, 0, 0],
                deadline: None,
            }]
        );
    }

    #[test]
    fn pushed_report_never_creates_grant() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register(&a, None)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![level_update(&a, 40)]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(report(1), 5);

        assert_eq!(out, vec![HubOutput::StateChanged]);
        assert_eq!(grants(&f), vec![(group_key(1), Trigger::Arrival)]);
        let s = state(&hub, &a).unwrap();
        assert_eq!(s.level, Some(Level::Percent(40)));
        assert_eq!(s.source, Source::Push);
        assert_eq!(s.updated_ms, 5);
    }

    #[test]
    fn sends_beyond_budget_are_dropped() {
        let f = fake();
        f.borrow_mut()
            .on_grant
            .push_back((0..6).map(|b| send(THIS_GRANT, coll(1), b)).collect());
        let policy = Policy {
            discovery_budget: 4,
            ..Policy::default()
        };
        let mut hub = hub_with(policy, &[(1, &f)]);

        let out = hub.handle(logi(1), 0);

        assert_eq!(sends(&out), vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn resume_and_refresh_read_registered_devices_and_rediscover_empty_groups() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register(&a, None)]);
        f.borrow_mut()
            .on_grant
            .push_back(vec![send(THIS_GRANT, coll(1), 0xD0)]);
        let idle = fake();
        idle.borrow_mut().on_grant.push_back(Vec::new());
        idle.borrow_mut()
            .on_grant
            .push_back(vec![send(THIS_GRANT, coll(2), 0xD2)]);
        let mut hub = hub(&[(1, &f), (2, &idle)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        let out = hub.handle(HubInput::Resume, 1_000);

        assert_eq!(sends(&out), vec![vec![0xD0], vec![0xD2]]);
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (a.clone(), Trigger::Resume)
            ]
        );
        // The group that registered nothing is discovered again, as on arrival.
        assert_eq!(
            grants(&idle),
            vec![
                (group_key(2), Trigger::Arrival),
                (group_key(2), Trigger::Arrival)
            ]
        );

        hub.handle(HubInput::RefreshNow, 2_000);
        assert_eq!(grants(&f).last(), Some(&(a, Trigger::RefreshNow)));
        assert_eq!(grants(&f).len(), 3);
        assert_eq!(grants(&idle).len(), 3);
        assert_eq!(
            grants(&idle).last(),
            Some(&(group_key(2), Trigger::Arrival))
        );
    }

    #[test]
    fn rediscover_is_granted_three_times_to_a_group_without_devices() {
        let f = fake();
        for _ in 0..4 {
            f.borrow_mut().on_report.push_back(vec![Action::Rediscover]);
        }
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        for now in 1..=4 {
            hub.handle(report(1), now);
        }

        let rediscovery = (group_key(1), Trigger::Rediscovery);
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                rediscovery.clone(),
                rediscovery.clone(),
                rediscovery
            ]
        );

        // A refresh starts over: one arrival grant, then three rediscoveries again.
        hub.handle(HubInput::RefreshNow, 5);
        for _ in 0..4 {
            f.borrow_mut().on_report.push_back(vec![Action::Rediscover]);
        }
        for now in 6..=9 {
            hub.handle(report(1), now);
        }
        let after: Vec<Trigger> = grants(&f)[4..].iter().map(|(_, t)| *t).collect();
        assert_eq!(
            after,
            [
                Trigger::Arrival,
                Trigger::Rediscovery,
                Trigger::Rediscovery,
                Trigger::Rediscovery
            ]
        );
    }

    #[test]
    fn rediscover_is_refused_to_a_group_with_a_registered_device() {
        let f = fake();
        f.borrow_mut()
            .on_grant
            .push_back(vec![register(&dev("dev-a"), None)]);
        f.borrow_mut().on_report.push_back(vec![Action::Rediscover]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        hub.handle(report(1), 1);

        assert_eq!(grants(&f), vec![(group_key(1), Trigger::Arrival)]);
    }

    #[test]
    fn group_with_query_wakes_quirk_is_not_rediscovered() {
        let f = fake();
        f.borrow_mut().on_report.push_back(vec![Action::Rediscover]);
        let mut hub = hub(&[(3, &f)]);
        hub.handle(HubInput::GroupArrived(group(3, 0x046D, 0x0AAB)), 0);

        hub.handle(report(3), 1);
        hub.handle(HubInput::Resume, 2);
        hub.handle(HubInput::RefreshNow, 3);

        assert_eq!(grants(&f), vec![(group_key(3), Trigger::Arrival)]);
    }

    #[test]
    fn discovery_failure_is_reported_with_the_group_key() {
        let f = fake();
        f.borrow_mut().on_report.push_back(vec![
            Action::DiscoveryFailed {
                retry_at: Some(2_001),
            },
            Action::DiscoveryFailed { retry_at: None },
        ]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(report(1), 1);

        assert_eq!(
            out,
            vec![
                HubOutput::DiscoveryFailed {
                    group: group_key(1),
                    retry_at: Some(2_001),
                },
                HubOutput::DiscoveryFailed {
                    group: group_key(1),
                    retry_at: None,
                },
            ]
        );
    }

    #[test]
    fn connected_delivers_grant() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register(&a, None)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Connected(a.clone())]);
        f.borrow_mut()
            .on_grant
            .push_back(vec![send(THIS_GRANT, coll(1), 0xC0)]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(report(1), 500);

        assert_eq!(sends(&out), vec![vec![0xC0]]);
        assert_eq!(grants(&f).last(), Some(&(a, Trigger::Connected)));
    }

    #[test]
    fn removal_keeps_state_for_grace_then_drops() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register_with(
            &a,
            activity_gap(),
            Some(Level::Percent(50)),
        )]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(HubInput::GroupRemoved(GroupId(1)), 1_000);
        assert_eq!(out, vec![HubOutput::StateChanged]);
        let s = state(&hub, &a).unwrap();
        assert_eq!(s.presence, Presence::Offline);
        assert_eq!(s.level, Some(Level::Percent(50)));

        let out = hub.handle(HubInput::Tick, 1_000 + 59_999);
        assert!(out.is_empty(), "{out:?}");
        assert!(state(&hub, &a).is_some());
        // The scheduler keeps the device through the grace.
        assert_eq!(hub.audio_ready_at(&a), Some(100_000));

        let out = hub.handle(HubInput::Tick, 1_000 + 60_000);
        assert_eq!(out, vec![HubOutput::StateChanged]);
        assert!(hub.snapshot().is_empty());
        assert_eq!(hub.audio_ready_at(&a), None, "scheduler forgot it");
    }

    #[test]
    fn rearrival_within_grace_reuses_entry() {
        let a = dev("dev-a");
        let first = fake();
        first
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&a, Some(Level::Percent(50)))]);
        let second = fake();
        second
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&a, None)]);
        let mut hub = hub(&[(1, &first), (2, &second)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::GroupRemoved(GroupId(1)), 1_000);

        let out = hub.handle(logi(2), 1_000 + 10_000);

        assert_eq!(state_changes(&out), 1);
        let snap = hub.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].key, a);
        assert_eq!(snap[0].level, Some(Level::Percent(50)));
        assert_eq!(snap[0].presence, Presence::Online);

        // The grace no longer applies, and the new engine owns the device.
        hub.handle(HubInput::Tick, 200_000);
        assert!(state(&hub, &a).is_some());
        hub.handle(HubInput::RefreshNow, 200_000);
        assert_eq!(grants(&second).last(), Some(&(a, Trigger::RefreshNow)));
    }

    #[test]
    fn next_deadline_is_min_of_engines_and_grace() {
        let (fa, fb) = (fake(), fake());
        fa.borrow_mut().deadline = Some(70_000);
        fb.borrow_mut().deadline = Some(90_000);
        fb.borrow_mut()
            .on_grant
            .push_back(vec![register(&dev("dev-b"), None)]);
        let mut hub = hub(&[(1, &fa), (2, &fb)]);
        assert_eq!(hub.next_deadline(), None);

        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);
        assert_eq!(hub.next_deadline(), Some(70_000));

        hub.handle(HubInput::GroupRemoved(GroupId(2)), 1_000);
        assert_eq!(hub.next_deadline(), Some(1_000 + REMOVAL_GRACE_MS));

        fa.borrow_mut().deadline = Some(30_000);
        assert_eq!(hub.next_deadline(), Some(30_000));

        fa.borrow_mut().deadline = None;
        hub.handle(HubInput::Tick, 61_000);
        assert_eq!(hub.next_deadline(), None);
    }

    #[test]
    fn tick_reaches_every_engine() {
        let (fa, fb) = (fake(), fake());
        let mut hub = hub(&[(1, &fa), (2, &fb)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        hub.handle(HubInput::Tick, 42);

        assert_eq!(fa.borrow().calls.last(), Some(&Call::Tick(42)));
        assert_eq!(fb.borrow().calls.last(), Some(&Call::Tick(42)));
    }

    #[test]
    fn register_verification_from_catalog() {
        let (headset, receiver, other) = (fake(), fake(), fake());
        let (h, r, o) = (dev("headset"), dev("receiver"), dev("other"));
        headset
            .borrow_mut()
            .on_grant
            .push_back(vec![register_as(&h, Via::Usb, None)]);
        receiver
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&r, None), register(&dev("second"), None)]);
        other
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&o, None)]);
        let mut hub = hub(&[(1, &headset), (2, &receiver), (3, &other)]);

        hub.handle(HubInput::GroupArrived(group(1, 0x046D, 0x0ABA)), 0);
        let out = hub.handle(HubInput::GroupArrived(group(2, 0x046D, 0xC547)), 0);
        hub.handle(HubInput::GroupArrived(group(3, 0x1532, 0x0084)), 0);

        assert_eq!(out, vec![HubOutput::StateChanged], "one per handle call");
        let verification = |k| state(&hub, k).unwrap().verification;
        assert_eq!(verification(&h), Verification::HardwareTested);
        assert_eq!(verification(&r), Verification::SourceVerified);
        assert_eq!(verification(&o), Verification::Generic);
        let s = state(&hub, &h).unwrap();
        // A catalog entry names the device, whatever it calls itself.
        assert_eq!(s.name, "PRO X Wireless");
        assert_eq!(s.kind, DeviceKind::Mouse);
        assert_eq!(s.presence, Presence::Online);
        assert_eq!(s.source, Source::Query);
    }

    #[test]
    fn receiver_device_matched_by_wireless_pid() {
        let receiver = fake();
        let (known, unknown, plain) = (dev("superlight"), dev("other-mouse"), dev("no-wpid"));
        receiver.borrow_mut().on_grant.push_back(vec![
            register_as(&known, Via::Receiver, Some(0x4093)),
            register_as(&unknown, Via::Receiver, Some(0x4074)),
            register(&plain, None),
        ]);
        let mut hub = hub(&[(1, &receiver)]);

        // A receiver whose own USB PID (0x0ABA here) has an entry: the entry is not the slots'.
        hub.handle(HubInput::GroupArrived(group(1, 0x046D, 0x0ABA)), 0);

        let s = state(&hub, &known).unwrap();
        assert_eq!(s.name, "PRO X Superlight");
        assert_eq!(s.verification, Verification::HardwareTested);
        let s = state(&hub, &unknown).unwrap();
        assert_eq!(s.name, "Mouse");
        assert_eq!(s.verification, Verification::SourceVerified);
        // Without a wireless PID a receiver device gets the family entry, never the receiver's.
        let s = state(&hub, &plain).unwrap();
        assert_eq!(s.name, "Mouse");
        assert_eq!(s.verification, Verification::SourceVerified);
    }

    #[test]
    fn catalog_name_survives_a_second_registrant() {
        let (receiver, cable) = (fake(), fake());
        let k = dev("logi:unit:1A2B3C4D");
        receiver.borrow_mut().on_grant.push_back(vec![register_as(
            &k,
            Via::Receiver,
            Some(0x4093),
        )]);
        cable
            .borrow_mut()
            .on_grant
            .push_back(vec![register_as(&k, Via::Usb, None)]);
        let mut hub = hub(&[(1, &receiver), (2, &cable)]);
        hub.handle(HubInput::GroupArrived(group(1, 0x046D, 0xC547)), 0);

        // The same mouse plugged in by its cable (046D:C094) registers the same key.
        hub.handle(HubInput::GroupArrived(group(2, 0x046D, 0xC094)), 10);
        assert_eq!(state(&hub, &k).unwrap().name, "PRO X Superlight");

        // Unplugged: the receiver owns it again, name unchanged.
        hub.handle(HubInput::GroupRemoved(GroupId(2)), 20);
        let s = state(&hub, &k).unwrap();
        assert_eq!(s.name, "PRO X Superlight");
        assert_eq!(s.presence, Presence::Online);
    }

    #[test]
    fn asleep_stops_refresh_until_pushed_update() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut()
            .on_grant
            .push_back(vec![register(&a, Some(Level::Percent(60)))]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Asleep(a.clone())]);
        f.borrow_mut()
            .on_report
            .push_back(vec![level_update(&a, 55)]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(report(1), 10);
        assert_eq!(out, vec![HubOutput::StateChanged]);
        let s = state(&hub, &a).unwrap();
        assert_eq!(s.presence, Presence::Asleep);
        assert_eq!(s.level, Some(Level::Percent(60)));

        hub.handle(HubInput::RefreshNow, 20);
        assert_eq!(grants(&f).len(), 1, "asleep device must not be asked");

        hub.handle(report(1), 30);
        assert_eq!(state(&hub, &a).unwrap().presence, Presence::Online);
        hub.handle(HubInput::RefreshNow, 40);
        assert_eq!(grants(&f).last(), Some(&(a, Trigger::RefreshNow)));
    }

    #[test]
    fn update_saying_asleep_is_not_a_sign_of_life() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register(&a, None)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Update(DeviceUpdate {
                presence: Some(Presence::Asleep),
                ..update(&a)
            })]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        hub.handle(report(1), 10);
        hub.handle(HubInput::RefreshNow, 20);

        assert_eq!(state(&hub, &a).unwrap().presence, Presence::Asleep);
        assert_eq!(grants(&f).len(), 1);
    }

    #[test]
    fn refused_connected_still_sets_online_and_clears_asleep() {
        let f = fake();
        let (a, wakes) = (dev("dev-a"), dev("wakes"));
        let gap = Capabilities {
            min_query_gap_ms: 10_000,
            ..pushing()
        };
        let query_wakes = Capabilities {
            query_wakes: true,
            ..pushing()
        };
        f.borrow_mut().on_grant.push_back(vec![
            register_with(&a, gap, None),
            register_with(&wakes, query_wakes, None),
        ]);
        f.borrow_mut().on_report.push_back(vec![
            Action::Asleep(a.clone()),
            Action::Asleep(wakes.clone()),
        ]);
        f.borrow_mut().on_report.push_back(vec![
            Action::Connected(a.clone()),
            Action::Connected(wakes.clone()),
        ]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::RefreshNow, 0);
        hub.handle(report(1), 1);
        assert!(hub.scheduler.is_asleep(&a) && hub.scheduler.is_asleep(&wakes));

        let out = hub.handle(report(1), 2);

        assert_eq!(out, vec![HubOutput::StateChanged]);
        // The min gap limits only activity; a query-waking device is never asked on a link-up.
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (a.clone(), Trigger::RefreshNow),
                (a.clone(), Trigger::Connected),
            ]
        );
        for key in [&a, &wakes] {
            assert_eq!(state(&hub, key).unwrap().presence, Presence::Online);
            assert!(!hub.scheduler.is_asleep(key));
        }
    }

    /// A device that is read on activity, at most every 100 s.
    fn activity_gap() -> Capabilities {
        Capabilities {
            pushes_battery: false,
            audio: true,
            min_query_gap_ms: 100_000,
            ..pushing()
        }
    }

    #[test]
    fn awake_signal_changes_presence_without_moving_measurement_or_interval() {
        let f = fake();
        let key = dev("dual-source");
        let caps = Capabilities {
            awake_signal: true,
            ..activity_gap()
        };
        f.borrow_mut().on_grant.extend([
            vec![register_with(&key, caps, Some(Level::Percent(60)))],
            vec![send(THIS_GRANT, coll(1), 1)],
        ]);
        f.borrow_mut().on_report.extend([
            vec![Action::Asleep(key.clone())],
            vec![Action::Awake(key.clone())],
            vec![Action::Awake(key.clone())],
            vec![Action::Awake(key.clone())],
        ]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 1_000);
        hub.handle(intervals(&key, 500_000), 1_000);
        hub.handle(report(1), 2_000);
        assert_eq!(hub.audio_ready_at(&key), None);

        assert_eq!(
            hub.handle(report(1), 100_999),
            [HubOutput::StateChanged],
            "presence changes before a read is eligible"
        );
        let device = state(&hub, &key).unwrap();
        assert_eq!(device.presence, Presence::Online);
        assert_eq!(device.updated_ms, 1_000);
        assert_eq!(device.level, Some(Level::Percent(60)));
        assert_eq!(hub.audio_ready_at(&key), Some(101_000));
        assert!(hub.handle(HubInput::Tick, 101_000).is_empty());
        assert_eq!(grants(&f), [(group_key(1), Trigger::Arrival)]);

        assert_eq!(sends(&hub.handle(report(1), 101_000)), [vec![1]]);
        assert_eq!(grants(&f).last(), Some(&(key.clone(), Trigger::Awake)));
        assert_eq!(state(&hub, &key).unwrap().updated_ms, 1_000);
        assert_eq!(hub.audio_ready_at(&key), Some(601_000));

        assert!(hub.handle(report(1), 101_001).is_empty());
        assert_eq!(hub.audio_ready_at(&key), Some(601_000));
        hub.handle(HubInput::Activity(key.clone()), 600_999);
        assert_eq!(grants(&f).len(), 2);
        hub.handle(HubInput::Activity(key.clone()), 601_000);
        assert_eq!(grants(&f).last(), Some(&(key, Trigger::Activity)));
    }

    fn awake_only() -> Capabilities {
        Capabilities {
            audio: false,
            awake_signal: true,
            query_wakes: true,
            ..activity_gap()
        }
    }

    #[test]
    fn awake_only_device_needs_a_fresh_input_even_when_the_gap_is_due() {
        let f = fake();
        let key = dev("wake-only");
        f.borrow_mut().on_grant.extend([
            vec![register_with(&key, awake_only(), None)],
            vec![send(THIS_GRANT, coll(1), 1)],
            vec![send(THIS_GRANT, coll(1), 2)],
        ]);
        f.borrow_mut()
            .on_report
            .extend((0..4).map(|_| vec![Action::Awake(key.clone())]));
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        hub.handle(intervals(&key, 10_000), 0);
        let device = state(&hub, &key).unwrap();
        assert_eq!(device.poll_min_ms, Some(100_000));
        assert!(!device.reports_changes);
        assert_eq!(hub.audio_ready_at(&key), None);

        assert!(hub.handle(report(1), 99_999).is_empty());
        assert!(hub.handle(HubInput::Tick, 100_000).is_empty());
        assert!(hub.handle(HubInput::Activity(key.clone()), 100_000).is_empty());
        assert_eq!(grants(&f), [(group_key(1), Trigger::Arrival)]);
        assert_eq!(sends(&hub.handle(report(1), 100_000)), [vec![1]]);
        assert!(hub.handle(report(1), 199_999).is_empty());
        assert_eq!(sends(&hub.handle(report(1), 200_000)), [vec![2]]);
    }

    #[test]
    fn only_a_routed_input_action_from_the_current_owner_can_signal_awake() {
        let (old, current) = (fake(), fake());
        let key = dev("shared");
        for f in [&old, &current] {
            f.borrow_mut().on_grant.push_back(vec![register_with(
                &key,
                awake_only(),
                Some(Level::Percent(60)),
            )]);
        }
        old.borrow_mut()
            .on_report
            .push_back(vec![Action::Awake(key.clone())]);
        current.borrow_mut().on_report.extend([
            vec![Action::Asleep(key.clone())],
            vec![
                Action::Awake(dev("unregistered")),
                Action::Awake(group_key(2)),
            ],
            vec![Action::Awake(key.clone())],
            vec![Action::Connected(group_key(2))],
            vec![Action::Awake(key.clone())],
        ]);
        current
            .borrow_mut()
            .on_tick
            .push_back(vec![Action::Awake(key.clone())]);
        current.borrow_mut().on_grant.extend([
            vec![Action::Awake(key.clone())],
            vec![send(THIS_GRANT, coll(2), 1)],
        ]);
        let mut hub = hub(&[(1, &old), (2, &current)]);
        hub.handle(logi(1), 0);
        let mut group = group(2, 0x046D, 0xC547);
        group.collections[0].feature_len = 1;
        hub.handle(HubInput::GroupArrived(group), 0);
        hub.handle(report(2), 1_000);

        for input in [
            report(1),
            report(2),
            HubInput::Report {
                collection: coll(2),
                origin: ReportOrigin::FeatureReply,
                bytes: vec![0x11],
            },
            HubInput::Tick,
            report(2), // Nested grant triggered from Input cannot inherit its permission.
        ] {
            let out = hub.handle(input, 100_000);
            assert!(out.is_empty(), "{out:?}");
            let device = state(&hub, &key).unwrap();
            assert_eq!(device.presence, Presence::Asleep);
            assert_eq!(device.updated_ms, 0);
        }
        assert_eq!(grants(&old), [(group_key(1), Trigger::Arrival)]);
        assert_eq!(
            grants(&current),
            [
                (group_key(2), Trigger::Arrival),
                (group_key(2), Trigger::Connected)
            ]
        );
        assert_eq!(sends(&hub.handle(report(2), 100_000)), [vec![1]]);
        assert_eq!(grants(&current).last(), Some(&(key, Trigger::Awake)));
    }

    #[test]
    fn restored_owner_does_not_inherit_the_departing_owners_activity_sources() {
        let (old, current) = (fake(), fake());
        let key = dev("shared");
        old.borrow_mut()
            .on_grant
            .push_back(vec![register(&key, Some(Level::Percent(50)))]);
        old.borrow_mut()
            .on_report
            .push_back(vec![Action::Awake(key.clone())]);
        let caps = Capabilities {
            awake_signal: true,
            ..polled()
        };
        current
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&key, caps, None)]);
        current
            .borrow_mut()
            .on_report
            .push_back(vec![charge_update(&key, Charge::Charging)]);
        let mut hub = hub(&[(1, &old), (2, &current)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);
        hub.handle(report(2), 0);
        assert_eq!(hub.next_deadline(), Some(300_000));

        hub.handle(HubInput::GroupRemoved(GroupId(2)), 1);
        let device = state(&hub, &key).unwrap();
        assert_eq!(device.charge, Charge::Charging);
        assert_eq!(device.poll_min_ms, None);
        assert!(device.reports_changes);
        assert_eq!(hub.next_deadline(), None);
        assert_eq!(hub.audio_ready_at(&key), None);
        assert!(hub.handle(report(1), 300_000).is_empty());
        assert!(hub.handle(HubInput::Activity(key), 300_000).is_empty());
        assert!(hub.handle(HubInput::Tick, 300_000).is_empty());
        assert_eq!(grants(&old), [(group_key(1), Trigger::Arrival)]);
    }

    #[test]
    fn registration_restarts_the_activity_gap() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, activity_gap(), None)]);
        let mut hub = hub(&[(1, &f)]);
        // The discovery that registers a device reads its battery: audio already playing
        // must not ask again at once.
        hub.handle(logi(1), 1_000);
        assert_eq!(hub.audio_ready_at(&a), Some(101_000));

        hub.handle(HubInput::Activity(a.clone()), 100_999);
        hub.handle(HubInput::Activity(a.clone()), 101_000);

        assert_eq!(
            grants(&f),
            vec![(group_key(1), Trigger::Arrival), (a, Trigger::Activity)]
        );
    }

    #[test]
    fn reregister_within_grace_restarts_the_activity_gap() {
        let a = dev("dev-a");
        let (first, second) = (fake(), fake());
        first
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, activity_gap(), None)]);
        second
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, activity_gap(), None)]);
        let mut hub = hub(&[(1, &first), (2, &second)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::RefreshNow, 0);
        hub.handle(HubInput::GroupRemoved(GroupId(1)), 1_000);
        hub.handle(logi(2), 2_000);

        // The refresh ended the registration's wake: the default gap, from the new read.
        hub.handle(HubInput::Activity(a.clone()), 601_999);
        assert_eq!(grants(&second), vec![(group_key(2), Trigger::Arrival)]);
        hub.handle(HubInput::Activity(a.clone()), 602_000);
        assert_eq!(grants(&second).last(), Some(&(a, Trigger::Activity)));
    }

    #[test]
    fn activity_ready_at_is_none_for_devices_activity_cannot_reach() {
        let f = fake();
        let (poll, push) = (dev("poll"), dev("push"));
        f.borrow_mut().on_grant.push_back(vec![
            register_with(&poll, activity_gap(), None),
            register(&push, None),
        ]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Asleep(poll.clone())]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        assert_eq!(hub.audio_ready_at(&poll), Some(100_000));
        assert_eq!(hub.audio_ready_at(&push), None, "pushes");
        assert_eq!(hub.audio_ready_at(&dev("unknown")), None);

        hub.handle(report(1), 1);
        assert_eq!(hub.audio_ready_at(&poll), None, "asleep");
    }

    #[test]
    fn rearrived_group_gets_a_new_arrival_grant() {
        let f = fake();
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::GroupRemoved(GroupId(1)), 1_000);

        hub.handle(logi(1), 2_000);

        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (group_key(1), Trigger::Arrival),
            ]
        );
    }

    #[test]
    fn activity_requests_grant_unless_device_pushes() {
        let f = fake();
        let (poll, push) = (dev("poll"), dev("push"));
        f.borrow_mut().on_grant.push_back(vec![
            register_with(&poll, activity_gap(), None),
            register(&push, None),
        ]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        hub.handle(HubInput::Activity(push), 100_000);
        hub.handle(HubInput::Activity(poll.clone()), 100_000);

        assert_eq!(
            grants(&f),
            vec![(group_key(1), Trigger::Arrival), (poll, Trigger::Activity)]
        );
    }

    #[test]
    fn a_reading_restarts_the_activity_gap() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, activity_gap(), None)]);
        // A headset switched on during playback broadcasts its level: no read right after.
        f.borrow_mut()
            .on_report
            .push_back(vec![level_update(&a, 80)]);
        // A presence change alone is no reading.
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Update(DeviceUpdate {
                presence: Some(Presence::Online),
                ..update(&a)
            })]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        assert_eq!(hub.audio_ready_at(&a), Some(100_000));

        hub.handle(report(1), 50_000);
        assert_eq!(hub.audio_ready_at(&a), Some(150_000));
        hub.handle(report(1), 60_000);
        assert_eq!(hub.audio_ready_at(&a), Some(150_000));
    }

    /// A device asked during use, with the headset's 5-minute minimum.
    fn polled() -> Capabilities {
        Capabilities {
            pushes_battery: false,
            audio: true,
            charging: true,
            min_query_gap_ms: 300_000,
            ..pushing()
        }
    }

    fn intervals(key: &DeviceKey, gap: Ms) -> HubInput {
        HubInput::SetIntervals([(key.clone(), gap)].into_iter().collect())
    }

    #[test]
    fn snapshot_reports_poll_minimum() {
        let f = fake();
        let (poll, push, no_gap) = (dev("poll"), dev("push"), dev("no-gap"));
        let without_gap = Capabilities {
            min_query_gap_ms: 0,
            ..polled()
        };
        f.borrow_mut().on_grant.push_back(vec![
            register_with(&poll, polled(), None),
            register(&push, None),
            register_with(&no_gap, without_gap, None),
        ]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        assert_eq!(state(&hub, &poll).unwrap().poll_min_ms, Some(300_000));
        assert_eq!(state(&hub, &push).unwrap().poll_min_ms, None);
        assert_eq!(state(&hub, &no_gap).unwrap().poll_min_ms, None);
    }

    #[test]
    fn set_intervals_reaches_scheduler() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, polled(), None)]);
        let mut hub = hub(&[(1, &f)]);
        // The registration reads the battery at 100 000; the post-wake grant follows at
        // the minimum.
        hub.handle(logi(1), 100_000);
        hub.handle(HubInput::Activity(a.clone()), 400_000);
        assert_eq!(grants(&f).last(), Some(&(a.clone(), Trigger::Activity)));

        assert_eq!(hub.handle(intervals(&a, 1_800_000), 500_000), vec![]);
        assert_eq!(hub.audio_ready_at(&a), Some(2_200_000));
    }

    #[test]
    fn wake_push_then_minimum_then_user_interval() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, polled(), None)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Asleep(a.clone())]);
        // The headset switched on again pushes its level.
        f.borrow_mut()
            .on_report
            .push_back(vec![level_update(&a, 90)]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::Activity(a.clone()), 300_000);
        hub.handle(intervals(&a, 1_800_000), 300_000);
        hub.handle(report(1), 400_000);
        assert_eq!(hub.audio_ready_at(&a), None, "asleep");

        let t = 1_000_000;
        hub.handle(report(1), t);
        assert_eq!(hub.audio_ready_at(&a), Some(t + 300_000));
        hub.handle(HubInput::Activity(a.clone()), t + 300_000);
        assert_eq!(hub.audio_ready_at(&a), Some(t + 300_000 + 1_800_000));
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (a.clone(), Trigger::Activity),
                (a, Trigger::Activity),
            ]
        );
    }

    #[test]
    fn link_up_wake_then_minimum_then_user_interval() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, polled(), None)]);
        // The post-wake activity grant reads nothing; the link-up's grant reads the level.
        f.borrow_mut().on_grant.push_back(vec![]);
        f.borrow_mut()
            .on_grant
            .push_back(vec![level_update(&a, 90)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Asleep(a.clone())]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Connected(a.clone())]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::Activity(a.clone()), 300_000);
        hub.handle(intervals(&a, 1_800_000), 300_000);
        hub.handle(report(1), 400_000);

        let t = 1_000_000;
        hub.handle(report(1), t);
        assert_eq!(grants(&f).last(), Some(&(a.clone(), Trigger::Connected)));
        assert_eq!(hub.audio_ready_at(&a), Some(t + 300_000));
        hub.handle(HubInput::Activity(a.clone()), t + 300_000);
        assert_eq!(grants(&f).last(), Some(&(a.clone(), Trigger::Activity)));
        assert_eq!(hub.audio_ready_at(&a), Some(t + 300_000 + 1_800_000));
    }

    /// A reading of `key` with status `charge`.
    fn charge_update(key: &DeviceKey, charge: Charge) -> Action {
        Action::Update(DeviceUpdate {
            level: Some(Level::Percent(50)),
            charge: Some(charge),
            presence: Some(Presence::Online),
            ..update(key)
        })
    }

    /// `a` registered at 0 with `caps` and pushing a reading with status `charge` at 0.
    fn registered_with_charge(f: &Fake, a: &DeviceKey, caps: Capabilities, charge: Charge) -> Hub {
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(a, caps, None)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![charge_update(a, charge)]);
        let mut hub = hub(&[(1, f)]);
        hub.handle(logi(1), 0);
        hub.handle(report(1), 0);
        hub
    }

    #[test]
    fn charging_device_is_read_at_the_interval_without_audio() {
        let f = fake();
        let a = dev("dev-a");
        let mut hub = registered_with_charge(&f, &a, polled(), Charge::Charging);
        // Each read finds it still charging.
        for _ in 0..2 {
            f.borrow_mut()
                .on_grant
                .push_back(vec![charge_update(&a, Charge::Charging)]);
        }

        // First after the wake at the minimum, then at the default interval.
        assert_eq!(hub.next_deadline(), Some(300_000));
        hub.handle(HubInput::Tick, 299_999);
        assert_eq!(grants(&f).len(), 1, "not yet");
        hub.handle(HubInput::Tick, 300_000);
        assert_eq!(grants(&f).last(), Some(&(a.clone(), Trigger::Activity)));
        assert_eq!(hub.next_deadline(), Some(900_000));
        hub.handle(HubInput::Tick, 900_000);
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (a.clone(), Trigger::Activity),
                (a, Trigger::Activity),
            ]
        );
        assert_eq!(hub.next_deadline(), Some(1_500_000));
    }

    #[test]
    fn charging_reads_stop_once_the_device_stops_charging() {
        for (end, what) in [
            (
                charge_update(&dev("dev-a"), Charge::Discharging),
                "discharging",
            ),
            (charge_update(&dev("dev-a"), Charge::Full), "full"),
            (
                charge_update(&dev("dev-a"), Charge::NotCharging),
                "not charging",
            ),
            (Action::Asleep(dev("dev-a")), "asleep"),
        ] {
            let f = fake();
            let a = dev("dev-a");
            let mut hub = registered_with_charge(&f, &a, polled(), Charge::Charging);
            f.borrow_mut().on_grant.push_back(vec![end]);
            hub.handle(HubInput::Tick, 300_000);
            assert_eq!(grants(&f).len(), 2, "{what}");

            assert_eq!(hub.next_deadline(), None, "{what}");
            hub.handle(HubInput::Tick, 900_000);
            assert_eq!(grants(&f).len(), 2, "{what}");
            // Audio asks it again, except while asleep.
            hub.handle(HubInput::Activity(a.clone()), 900_000);
            assert_eq!(
                grants(&f).len(),
                2 + usize::from(what != "asleep"),
                "{what}"
            );
        }
    }

    #[test]
    fn devices_not_charging_or_pushing_get_no_timed_reads() {
        for (caps, charge) in [
            (pushing(), Charge::Charging),
            (polled(), Charge::Discharging),
            (polled(), Charge::Full),
        ] {
            let f = fake();
            let a = dev("dev-a");
            let mut hub = registered_with_charge(&f, &a, caps, charge);
            assert_eq!(hub.next_deadline(), None, "{caps:?} {charge:?}");
            hub.handle(HubInput::Tick, 10_000_000);
            assert_eq!(grants(&f).len(), 1, "{caps:?} {charge:?}");
        }
    }

    #[test]
    fn polled_device_without_charging_opt_in_has_no_silent_timer() {
        let f = fake();
        let key = dev("activity-only");
        let caps = Capabilities {
            awake_signal: true,
            ..activity_gap()
        };
        let mut hub = registered_with_charge(&f, &key, caps, Charge::Charging);

        assert_eq!(hub.next_deadline(), None);
        assert!(hub.handle(HubInput::Tick, 100_000).is_empty());
        assert_eq!(grants(&f), [(group_key(1), Trigger::Arrival)]);
        hub.handle(HubInput::Activity(key.clone()), 100_000);
        assert_eq!(grants(&f).last(), Some(&(key, Trigger::Activity)));
    }

    #[test]
    fn charging_opt_in_does_not_expose_an_audio_interval() {
        let f = fake();
        let key = dev("charging-only");
        let caps = Capabilities {
            audio: false,
            ..polled()
        };
        let mut hub = registered_with_charge(&f, &key, caps, Charge::Charging);

        assert_eq!(state(&hub, &key).unwrap().poll_min_ms, None);
        assert_eq!(hub.audio_ready_at(&key), None);
        assert_eq!(hub.next_deadline(), Some(300_000));
        hub.handle(HubInput::Activity(key.clone()), 300_000);
        assert_eq!(grants(&f), [(group_key(1), Trigger::Arrival)]);
        hub.handle(HubInput::Tick, 300_000);
        assert_eq!(grants(&f).last(), Some(&(key, Trigger::Activity)));
        assert_eq!(hub.next_deadline(), Some(900_000));
    }

    #[test]
    fn keys_of_lists_devices_the_group_owns() {
        let (f1, f2) = (fake(), fake());
        let (a, b) = (dev("a"), dev("b"));
        f1.borrow_mut()
            .on_grant
            .push_back(vec![register(&a, None), register(&b, None)]);
        f2.borrow_mut().on_grant.push_back(vec![register(&b, None)]);
        let mut hub = hub(&[(1, &f1), (2, &f2)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        // `b` moved to group 2; group keys are not devices.
        assert_eq!(hub.keys_of(GroupId(1)), std::slice::from_ref(&a));
        assert_eq!(hub.keys_of(GroupId(2)), std::slice::from_ref(&b));
        assert!(hub.keys_of(GroupId(3)).is_empty());

        hub.handle(HubInput::GroupRemoved(GroupId(2)), 1);
        assert_eq!(hub.keys_of(GroupId(1)), [a, b]);
    }

    #[test]
    fn report_for_unknown_collection_is_ignored() {
        let f = fake();
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        let out = hub.handle(
            HubInput::Report {
                collection: CollectionId(77),
                origin: ReportOrigin::Input,
                bytes: vec![1],
            },
            5,
        );

        assert!(out.is_empty());
        assert_eq!(f.borrow().calls.len(), 1);
    }

    #[test]
    fn engine_cannot_touch_another_engines_device() {
        let (fa, fb) = (fake(), fake());
        let a = dev("dev-a");
        fa.borrow_mut()
            .on_grant
            .push_back(vec![register(&a, Some(Level::Percent(80)))]);
        fb.borrow_mut().on_report.push_back(vec![
            level_update(&a, 1),
            Action::Asleep(a.clone()),
            Action::Connected(a.clone()),
            Action::Connected(group_key(1)),
            send(GrantId(1), coll(1), 0xEE),
        ]);
        let mut hub = hub(&[(1, &fa), (2, &fb)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        let out = hub.handle(report(2), 5);

        assert_eq!(
            out,
            [HubOutput::WriteRefused {
                kind: ReportKind::Output,
                collection: coll(1),
                reason: WriteRefusal::Collection,
            }]
        );
        let s = state(&hub, &a).unwrap();
        assert_eq!(s.level, Some(Level::Percent(80)));
        assert_eq!(s.presence, Presence::Online);
        assert_eq!(grants(&fa).len(), 1);
        assert_eq!(grants(&fb).len(), 1);
    }

    #[test]
    fn connected_on_group_key_delivers_grant_to_owning_engine() {
        let (fa, fb) = (fake(), fake());
        fb.borrow_mut()
            .on_report
            .push_back(vec![Action::Connected(group_key(2))]);
        fb.borrow_mut().on_grant.push_back(Vec::new());
        fb.borrow_mut()
            .on_grant
            .push_back(vec![send(THIS_GRANT, coll(2), 0xC2)]);
        let mut hub = hub(&[(1, &fa), (2, &fb)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        let out = hub.handle(report(2), 5);

        assert_eq!(sends(&out), vec![vec![0xC2]]);
        assert_eq!(
            grants(&fb),
            vec![
                (group_key(2), Trigger::Arrival),
                (group_key(2), Trigger::Connected),
            ]
        );
        assert_eq!(grants(&fa), vec![(group_key(1), Trigger::Arrival)]);
    }

    #[test]
    fn takeover_then_removal_falls_back_to_remaining_registrant() {
        let k = dev("logi:unit:1A2B3C4D");
        let (receiver, wired) = (fake(), fake());
        receiver
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, Some(Level::Percent(50)))]);
        receiver
            .borrow_mut()
            .on_report
            .push_back(vec![level_update(&k, 30)]);
        wired
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, None)]);
        let mut hub = hub(&[(1, &receiver), (2, &wired)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 100);

        let out = hub.handle(HubInput::GroupRemoved(GroupId(2)), 1_000);
        assert!(out.is_empty(), "state kept as is: {out:?}");
        assert_eq!(state(&hub, &k).unwrap().presence, Presence::Online);
        assert!(hub.grace.is_empty());
        assert_eq!(hub.next_deadline(), None);

        hub.handle(report(1), 2_000);
        assert_eq!(state(&hub, &k).unwrap().level, Some(Level::Percent(30)));

        hub.handle(HubInput::RefreshNow, 3_000);
        assert_eq!(
            grants(&receiver).last(),
            Some(&(k.clone(), Trigger::RefreshNow))
        );
        assert_eq!(grants(&wired), vec![(group_key(2), Trigger::Arrival)]);

        hub.handle(HubInput::Tick, 100_000);
        assert!(state(&hub, &k).is_some());
    }

    #[test]
    fn unregister_by_last_registrant_starts_grace() {
        let k = dev("logi:unit:1A2B3C4D");
        let (receiver, wired) = (fake(), fake());
        receiver
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, None)]);
        receiver
            .borrow_mut()
            .on_report
            .push_back(vec![Action::Unregister(k.clone())]);
        wired
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, None)]);
        wired
            .borrow_mut()
            .on_report
            .push_back(vec![Action::Unregister(k.clone())]);
        let mut hub = hub(&[(1, &receiver), (2, &wired)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        hub.handle(report(1), 10);
        assert_eq!(state(&hub, &k).unwrap().presence, Presence::Online);
        assert!(hub.grace.is_empty());

        let out = hub.handle(report(2), 20);
        assert_eq!(out, vec![HubOutput::StateChanged]);
        assert_eq!(state(&hub, &k).unwrap().presence, Presence::Offline);
        assert_eq!(hub.next_deadline(), Some(20 + REMOVAL_GRACE_MS));
    }

    #[test]
    fn nested_connected_grants_are_capped() {
        let f = fake();
        f.borrow_mut().connected_on_grant = true;
        let mut hub = hub(&[(1, &f)]);

        hub.handle(logi(1), 0);

        assert_eq!(grants(&f).len(), 1 + MAX_NESTED_GRANTS);
    }

    #[test]
    fn duplicate_fallback_keys_get_suffix() {
        let k = dev("logi:0ABA:255");
        let (k2, k3) = (dev("logi:0ABA:255#2"), dev("logi:0ABA:255#3"));
        let (first, second, third) = (fake(), fake(), fake());
        first
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, Some(Level::Percent(90)))]);
        for f in [&second, &third] {
            f.borrow_mut().on_grant.push_back(vec![register(&k, None)]);
        }
        second
            .borrow_mut()
            .on_report
            .push_back(vec![level_update(&k, 40)]);
        second
            .borrow_mut()
            .on_report
            .push_back(vec![level_update(&k, 39)]);
        // Registers again without a reading (e.g. the device is asleep).
        let Action::Register {
            caps,
            mut initial,
            wpid,
            ..
        } = register(&k, None)
        else {
            unreachable!()
        };
        initial.presence = Some(Presence::Asleep);
        second
            .borrow_mut()
            .on_report
            .push_back(vec![Action::Register {
                key: k.clone(),
                caps,
                initial,
                wpid,
            }]);
        third
            .borrow_mut()
            .on_report
            .push_back(vec![Action::Unregister(k.clone())]);
        let mut hub = hub(&[(1, &first), (2, &second), (3, &third)]);
        // Two identical dongles without unit ids, then a third.
        for id in 1..=3 {
            hub.handle(HubInput::GroupArrived(group(id, 0x046D, 0x0ABA)), 0);
        }
        let keys: Vec<DeviceKey> = hub.snapshot().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, [k.clone(), k2.clone(), k3.clone()]);
        assert_eq!(hub.keys_of(GroupId(2)), std::slice::from_ref(&k2));

        // The engine keeps speaking its own key; the hub maps it both ways.
        hub.handle(report(2), 10);
        assert_eq!(state(&hub, &k2).unwrap().level, Some(Level::Percent(40)));
        assert_eq!(state(&hub, &k).unwrap().level, Some(Level::Percent(90)));
        hub.handle(HubInput::RefreshNow, 20);
        assert_eq!(
            grants(&second).last(),
            Some(&(k.clone(), Trigger::RefreshNow))
        );
        assert_eq!(
            grants(&first).last(),
            Some(&(k.clone(), Trigger::RefreshNow))
        );
        hub.handle(report(3), 30);
        assert_eq!(state(&hub, &k3).unwrap().presence, Presence::Offline);

        // The first leaves: the second keeps `#2` until it registers again.
        hub.handle(HubInput::GroupRemoved(GroupId(1)), 40);
        hub.handle(report(2), 50);
        assert_eq!(state(&hub, &k2).unwrap().level, Some(Level::Percent(39)));
        assert_eq!(state(&hub, &k).unwrap().presence, Presence::Offline);
        hub.handle(report(2), 60);
        assert_eq!(hub.keys_of(GroupId(2)), std::slice::from_ref(&k));
        // It takes its own last values along, not those the first device left behind.
        let s = state(&hub, &k).unwrap();
        assert_eq!(s.level, Some(Level::Percent(39)));
        assert_eq!(s.presence, Presence::Asleep);
        let keys: Vec<DeviceKey> = hub.snapshot().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, [k.clone(), k3.clone()], "no `#2` entry left behind");
        // The first device's grace no longer applies to the key.
        hub.handle(HubInput::Tick, 40 + REMOVAL_GRACE_MS);
        assert!(state(&hub, &k).is_some());
    }

    #[test]
    fn unit_keys_are_never_suffixed() {
        let k = dev("logi:unit:1A2B3C4D");
        let (receiver, cable) = (fake(), fake());
        receiver
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, None)]);
        cable
            .borrow_mut()
            .on_grant
            .push_back(vec![register(&k, None)]);
        let mut hub = hub(&[(1, &receiver), (2, &cable)]);
        hub.handle(HubInput::GroupArrived(group(1, 0x046D, 0xC547)), 0);
        hub.handle(HubInput::GroupArrived(group(2, 0x046D, 0xC547)), 0);

        let keys: Vec<DeviceKey> = hub.snapshot().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, std::slice::from_ref(&k));
    }
}
