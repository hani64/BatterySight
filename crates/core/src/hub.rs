//! Sans-IO orchestrator: owns the engines, the device state and the scheduler.
//!
//! The platform runtime feeds [`HubInput`]s with the current time and executes
//! the returned [`HubOutput`]s; nothing here performs I/O or reads a clock.

use std::collections::{BTreeMap, HashMap};

use crate::catalog::{Catalog, EngineKind};
use crate::engine::{Action, Engine};
use crate::engines;
use crate::hid::{CollectionId, GroupId, HidGroup};
use crate::model::{
    Charge, DeviceKey, DeviceKind, DeviceState, DeviceUpdate, Presence, Verification, Via,
};
use crate::scheduler::{Capabilities, Policy, Scheduler, Trigger};
use crate::time::Ms;

/// How long a removed device keeps its last values before it disappears.
pub const REMOVAL_GRACE_MS: Ms = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubInput {
    GroupArrived(HidGroup),
    GroupRemoved(GroupId),
    Report {
        collection: CollectionId,
        bytes: Vec<u8>,
    },
    Resume,
    RefreshNow,
    Activity(DeviceKey),
    Tick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubOutput {
    Send {
        collection: CollectionId,
        bytes: Vec<u8>,
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

type EngineFactory = Box<dyn Fn(&HidGroup, &Catalog) -> Option<Box<dyn Engine>>>;

/// A group key gets its arrival conversation, `Rediscovery` grants while its engine
/// registered no device, an arrival again on resume or refresh in that case, and the
/// `Connected` grants its engine asks for (e.g. to discover a newly paired device).
const GROUP_CAPS: Capabilities = Capabilities {
    pushes_battery: false,
    query_wakes: false,
    min_query_gap_ms: 0,
};

/// How deep grant deliveries may nest within one `handle` call (an engine
/// reporting `Connected` from inside `on_grant`); deeper requests are dropped.
const MAX_NESTED_GRANTS: usize = 4;

/// One arrived group and its engine.
struct Slot {
    engine: Box<dyn Engine>,
    key: DeviceKey,
    /// `(vid, pid)` of the group's first collection, for catalog lookups.
    ids: Option<(u16, u16)>,
    /// HID product string, the name of a device whose engine gives none.
    product: String,
    collections: Vec<CollectionId>,
    /// Hub keys of devices this engine registered under a shared key another live group
    /// already held (see [`Hub::claim`]), by the engine's own key, which it keeps using.
    aliases: BTreeMap<DeviceKey, DeviceKey>,
}

/// The fields of an [`Action::Register`].
struct Registration {
    key: DeviceKey,
    caps: Capabilities,
    initial: DeviceUpdate,
    /// Wireless product id of a device behind a receiver, if the receiver reported one.
    wpid: Option<u16>,
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
    factory: EngineFactory,
    scheduler: Scheduler,
    engines: BTreeMap<GroupId, Slot>,
    routes: HashMap<CollectionId, GroupId>,
    /// Registered devices and every live group whose engine registered them;
    /// the last one is the current owner. The same device can be served by
    /// several groups at once (e.g. receiver and cable).
    owners: BTreeMap<DeviceKey, Vec<GroupId>>,
    states: BTreeMap<DeviceKey, DeviceState>,
    /// Devices no engine owns any more, and when their entries expire.
    grace: BTreeMap<DeviceKey, Ms>,
}

impl Hub {
    pub fn new(catalog: Catalog, policy: Policy) -> Self {
        Self {
            catalog,
            factory: Box::new(crate::engines::engine_for),
            scheduler: Scheduler::new(policy),
            engines: BTreeMap::new(),
            routes: HashMap::new(),
            owners: BTreeMap::new(),
            states: BTreeMap::new(),
            grace: BTreeMap::new(),
        }
    }

    pub fn with_engine_factory(self, f: EngineFactory) -> Self {
        Self { factory: f, ..self }
    }

    pub fn handle(&mut self, input: HubInput, now: Ms) -> Vec<HubOutput> {
        let mut pass = Pass::default();
        match input {
            HubInput::GroupArrived(group) => self.arrive(&group, now, &mut pass),
            HubInput::GroupRemoved(id) => self.remove_group(id, now, &mut pass),
            HubInput::Report { collection, bytes } => {
                if let Some(&gid) = self.routes.get(&collection)
                    && let Some(slot) = self.engines.get_mut(&gid)
                {
                    let actions = slot.engine.on_report(collection, &bytes, now);
                    self.apply(gid, actions, now, &mut pass);
                }
            }
            HubInput::Resume => self.request_all(Trigger::Resume, now, &mut pass),
            HubInput::RefreshNow => self.request_all(Trigger::RefreshNow, now, &mut pass),
            HubInput::Activity(key) => {
                if let Some(gid) = self.device_owner(&key) {
                    self.grant(gid, &key, Trigger::Activity, now, &mut pass);
                }
            }
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

    /// When the next `Tick` is due: the earliest engine deadline or grace expiry.
    pub fn next_deadline(&self) -> Option<Ms> {
        self.engines
            .values()
            .filter_map(|slot| slot.engine.next_deadline())
            .chain(self.grace.values().copied())
            .min()
    }

    /// The registered devices `group`'s engine currently owns, ordered by key: those an
    /// [`HubInput::Activity`] on the group's physical device would reach.
    pub fn keys_of(&self, group: GroupId) -> Vec<DeviceKey> {
        self.owners
            .iter()
            .filter(|(_, groups)| groups.last() == Some(&group))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// When an [`HubInput::Activity`] for `key` may next be granted; `None` while none can
    /// be (unknown device, one that pushes its battery, or one asleep).
    pub fn activity_ready_at(&self, key: &DeviceKey) -> Option<Ms> {
        self.scheduler.activity_ready_at(key)
    }

    fn arrive(&mut self, group: &HidGroup, now: Ms, pass: &mut Pass) {
        // The same id again means the old handles are gone.
        self.remove_group(group.id, now, pass);
        let Some(engine) = (self.factory)(group, &self.catalog) else {
            return;
        };
        let key = engine.group_key();
        let collections: Vec<CollectionId> = group.collections.iter().map(|c| c.id).collect();
        for &c in &collections {
            self.routes.insert(c, group.id);
        }
        let first = group.collections.first();
        self.engines.insert(
            group.id,
            Slot {
                engine,
                key: key.clone(),
                ids: first.map(|c| (c.vid, c.pid)),
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
            if self.routes.get(c) == Some(&id) {
                self.routes.remove(c);
            }
        }
        // Group keys have no grace: a new arrival is a new conversation.
        self.scheduler.unregister(&slot.key);
        let served: Vec<DeviceKey> = self
            .owners
            .iter()
            .filter(|(_, groups)| groups.contains(&id))
            .map(|(key, _)| key.clone())
            .collect();
        for key in served {
            self.drop_registrant(&key, id, now, pass);
        }
    }

    /// `gid` no longer serves `key`. Ownership falls back to the latest
    /// remaining registrant, state untouched. With none left, the device goes
    /// offline and is kept until the grace expires; the scheduler keeps its
    /// history so a reconnect within grace cannot bypass the min gap or get a
    /// second arrival.
    fn drop_registrant(&mut self, key: &DeviceKey, gid: GroupId, now: Ms, pass: &mut Pass) {
        let Some(groups) = self.owners.get_mut(key) else {
            return;
        };
        groups.retain(|&g| g != gid);
        if !groups.is_empty() {
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
                self.apply(gid, actions, now, pass);
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

    /// Asks the scheduler for `trigger` on every registered device, then discovers each
    /// group whose engine registered no device again, as on its arrival.
    fn request_all(&mut self, trigger: Trigger, now: Ms, pass: &mut Pass) {
        let devices: Vec<(DeviceKey, GroupId)> = self
            .owners
            .keys()
            .filter_map(|key| Some((key.clone(), self.device_owner(key)?)))
            .collect();
        for (key, gid) in devices {
            // An earlier grant's actions may have moved or released the device.
            if self.device_owner(&key) == Some(gid) {
                self.grant(gid, &key, trigger, now, pass);
            }
        }
        let empty: Vec<(GroupId, DeviceKey)> = self
            .engines
            .iter()
            .filter(|&(&gid, _)| !self.registers_any(gid))
            .map(|(&gid, slot)| (gid, slot.key.clone()))
            .collect();
        for (gid, key) in empty {
            if self.engines.contains_key(&gid) && !self.registers_any(gid) {
                self.scheduler.rearm_arrival(&key);
                self.grant(gid, &key, Trigger::Arrival, now, pass);
            }
        }
    }

    /// Whether `gid`'s engine is a registrant of any device.
    fn registers_any(&self, gid: GroupId) -> bool {
        self.owners.values().any(|groups| groups.contains(&gid))
    }

    /// Requests a grant on `key` and, if the scheduler allows it, hands it to `gid`'s engine.
    fn grant(&mut self, gid: GroupId, key: &DeviceKey, trigger: Trigger, now: Ms, pass: &mut Pass) {
        // An engine that asks again from inside `on_grant` must not recurse without end.
        if pass.depth > MAX_NESTED_GRANTS {
            return;
        }
        let Some(mut grant) = self.scheduler.request(key, trigger, now) else {
            return;
        };
        if let Some(slot) = self.engines.get_mut(&gid) {
            if let Some((own, _)) = slot.aliases.iter().find(|(_, hub)| **hub == grant.subject) {
                grant.subject = own.clone();
            }
            let actions = slot.engine.on_grant(&grant, now);
            pass.depth += 1;
            self.apply(gid, actions, now, pass);
            pass.depth -= 1;
        }
    }

    /// Group whose engine currently owns device `key`: its latest live registrant.
    fn device_owner(&self, key: &DeviceKey) -> Option<GroupId> {
        self.owners
            .get(key)
            .and_then(|groups| groups.last().copied())
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
                .is_some_and(|groups| groups.iter().any(|&g| g != gid))
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
                .is_some_and(|groups| groups.as_slice() == [gid])
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

    /// Carries out `actions` from `gid`'s engine. Actions on devices or
    /// collections the engine does not own are ignored.
    fn apply(&mut self, gid: GroupId, actions: Vec<Action>, now: Ms, pass: &mut Pass) {
        for action in actions {
            match action {
                Action::Send {
                    grant,
                    collection,
                    bytes,
                } => {
                    if self.routes.get(&collection) == Some(&gid)
                        && self.scheduler.admit_send(grant, now)
                    {
                        pass.out.push(HubOutput::Send { collection, bytes });
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
                    if self
                        .owners
                        .get(&key)
                        .is_some_and(|groups| groups.contains(&gid))
                    {
                        self.drop_registrant(&key, gid, now, pass);
                    }
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
                    self.scheduler.mark_awake(&key);
                    if let Some(state) = self.states.get_mut(&key)
                        && state.presence != Presence::Online
                    {
                        state.presence = Presence::Online;
                        pass.changed = true;
                    }
                    self.grant(gid, &key, Trigger::Connected, now, pass);
                }
                Action::Rediscover => {
                    if !self.registers_any(gid)
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
        let (verification, catalog_name) = self.identity(slot.ids, behind_receiver, wpid);
        let product = slot.product.clone();

        let groups = self.owners.entry(key.clone()).or_default();
        groups.retain(|&g| g != gid);
        groups.push(gid);
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
            });
        state.verification = verification;
        state.apply(&initial, now);
        if let Some(name) = catalog_name {
            state.name = name;
        }
        if initial.presence.is_none() {
            state.presence = Presence::Online;
        }
        pass.changed = true;
    }

    /// A reading is a sign of life unless it says the device is away.
    fn note_presence(&mut self, key: &DeviceKey, presence: Option<Presence>) {
        match presence {
            Some(Presence::Asleep | Presence::Offline) => self.scheduler.mark_asleep(key),
            Some(Presence::Online) | None => self.scheduler.mark_awake(key),
        }
    }

    /// Catalog status and name of a device registered by a group with `ids`: its device
    /// entry, else the vendor family's status and no name, else `Generic`. A device behind a
    /// receiver is looked up by its wireless PID only (the receiver's USB PID says nothing
    /// about it); any other device by the group's USB PID.
    fn identity(
        &self,
        ids: Option<(u16, u16)>,
        behind_receiver: bool,
        wpid: Option<u16>,
    ) -> (Verification, Option<String>) {
        let Some((vid, pid)) = ids else {
            return (Verification::Generic, None);
        };
        let entry = if behind_receiver {
            wpid.and_then(|wpid| self.catalog.device_by_wpid(vid, wpid))
        } else {
            self.catalog.device(vid, pid)
        };
        if let Some(device) = entry {
            return (device.status, Some(device.name.clone()));
        }
        // M1: only HID++ engines exist; the engine's kind joins this lookup with the second one.
        let status = self
            .catalog
            .family(vid, EngineKind::Hidpp)
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
    use crate::engine::Action;
    use crate::hid::Collection;
    use crate::model::{DeviceKind, DeviceUpdate, Level, Presence, Source, Verification, Via};
    use crate::scheduler::{Capabilities, Grant, GrantId, Trigger};

    /// Stands for "the grant being delivered" in scripted `on_grant` sends;
    /// the scheduler never issues id 0.
    const THIS_GRANT: GrantId = GrantId(0);

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Grant(Grant),
        Report(CollectionId, Vec<u8>),
        Tick(Ms),
    }

    /// Script and call log of one [`FakeEngine`], shared with the test.
    #[derive(Default)]
    struct Script {
        on_grant: VecDeque<Vec<Action>>,
        on_report: VecDeque<Vec<Action>>,
        deadline: Option<Ms>,
        /// Report `Connected(group key)` from every `on_grant`.
        connected_on_grant: bool,
        calls: Vec<Call>,
    }

    type Fake = Rc<RefCell<Script>>;

    /// Records every call and replays the next scripted action list per call.
    struct FakeEngine {
        key: DeviceKey,
        script: Fake,
    }

    impl Engine for FakeEngine {
        fn group_key(&self) -> DeviceKey {
            self.key.clone()
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

        fn on_report(&mut self, collection: CollectionId, bytes: &[u8], _now: Ms) -> Vec<Action> {
            let mut s = self.script.borrow_mut();
            s.calls.push(Call::Report(collection, bytes.to_vec()));
            s.on_report.pop_front().unwrap_or_default()
        }

        fn on_tick(&mut self, now: Ms) -> Vec<Action> {
            self.script.borrow_mut().calls.push(Call::Tick(now));
            Vec::new()
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
                input_len: 20,
                output_len: 20,
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
            bytes: vec![0x11],
        }
    }

    fn send(grant: GrantId, collection: CollectionId, byte: u8) -> Action {
        Action::Send {
            grant,
            collection,
            bytes: vec![byte],
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
                HubOutput::StateChanged | HubOutput::DiscoveryFailed { .. } => None,
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
                    bytes: vec![0xA1],
                },
                HubOutput::Send {
                    collection: coll(1),
                    bytes: vec![0xA2],
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
            Some(&Call::Report(coll(1), vec![0x11]))
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
        assert_eq!(hub.activity_ready_at(&a), Some(100_000));

        let out = hub.handle(HubInput::Tick, 1_000 + 60_000);
        assert_eq!(out, vec![HubOutput::StateChanged]);
        assert!(hub.snapshot().is_empty());
        assert_eq!(hub.activity_ready_at(&a), None, "scheduler forgot it");
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
            min_query_gap_ms: 100_000,
            ..pushing()
        }
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
        assert_eq!(hub.activity_ready_at(&a), Some(101_000));

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

        hub.handle(HubInput::Activity(a.clone()), 101_999);
        assert_eq!(grants(&second), vec![(group_key(2), Trigger::Arrival)]);
        hub.handle(HubInput::Activity(a.clone()), 102_000);
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
        assert_eq!(hub.activity_ready_at(&poll), Some(100_000));
        assert_eq!(hub.activity_ready_at(&push), None, "pushes");
        assert_eq!(hub.activity_ready_at(&dev("unknown")), None);

        hub.handle(report(1), 1);
        assert_eq!(hub.activity_ready_at(&poll), None, "asleep");
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
        assert_eq!(hub.activity_ready_at(&a), Some(100_000));

        hub.handle(report(1), 50_000);
        assert_eq!(hub.activity_ready_at(&a), Some(150_000));
        hub.handle(report(1), 60_000);
        assert_eq!(hub.activity_ready_at(&a), Some(150_000));
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

        assert!(out.is_empty(), "{out:?}");
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
