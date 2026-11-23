//! Sans-IO orchestrator: owns the engines, the device state and the scheduler.
//!
//! The platform runtime feeds [`HubInput`]s with the current time and executes
//! the returned [`HubOutput`]s; nothing here performs I/O or reads a clock.

use std::collections::{BTreeMap, HashMap};

use crate::catalog::{Catalog, EngineKind};
use crate::engine::{Action, Engine};
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
        timeout_ms: u32,
    },
    /// The snapshot changed during this call.
    StateChanged,
}

type EngineFactory = Box<dyn Fn(&HidGroup, &Catalog) -> Option<Box<dyn Engine>>>;

/// A group key only ever gets its one arrival conversation, plus `Connected`
/// grants its engine asks for (e.g. to discover a newly paired device).
const GROUP_CAPS: Capabilities = Capabilities {
    pushes_battery: false,
    query_wakes: false,
    min_query_gap_ms: 0,
};

/// One arrived group and its engine.
struct Slot {
    engine: Box<dyn Engine>,
    key: DeviceKey,
    /// `(vid, pid)` of the group's first collection, for catalog lookups.
    ids: Option<(u16, u16)>,
    /// HID product string, the name of a device whose engine gives none.
    product: String,
    collections: Vec<CollectionId>,
}

/// What one `handle` call produces.
#[derive(Default)]
struct Pass {
    out: Vec<HubOutput>,
    changed: bool,
}

pub struct Hub {
    catalog: Catalog,
    factory: EngineFactory,
    scheduler: Scheduler,
    engines: BTreeMap<GroupId, Slot>,
    routes: HashMap<CollectionId, GroupId>,
    /// Registered devices and the group whose engine owns them.
    owners: BTreeMap<DeviceKey, GroupId>,
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
                if let Some(&gid) = self.owners.get(&key) {
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
            },
        );
        self.scheduler.register(key.clone(), GROUP_CAPS);
        self.grant(group.id, &key, Trigger::Arrival, now, pass);
    }

    /// Drops the engine; its devices go offline and enter the removal grace.
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
        let orphans: Vec<DeviceKey> = self
            .owners
            .iter()
            .filter(|&(_, &gid)| gid == id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in orphans {
            self.release(&key, now, pass);
        }
    }

    /// `key` loses its owner: offline, kept until the grace expires. The
    /// scheduler keeps its history so a reconnect within grace cannot
    /// bypass the min gap or get a second arrival.
    fn release(&mut self, key: &DeviceKey, now: Ms, pass: &mut Pass) {
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

    /// Asks the scheduler for `trigger` on every registered device (never group keys).
    fn request_all(&mut self, trigger: Trigger, now: Ms, pass: &mut Pass) {
        let devices: Vec<(DeviceKey, GroupId)> = self
            .owners
            .iter()
            .map(|(key, &gid)| (key.clone(), gid))
            .collect();
        for (key, gid) in devices {
            // An earlier grant's actions may have moved or released the device.
            if self.owners.get(&key) == Some(&gid) {
                self.grant(gid, &key, trigger, now, pass);
            }
        }
    }

    /// Requests a grant on `key` and, if the scheduler allows it, hands it to `gid`'s engine.
    fn grant(&mut self, gid: GroupId, key: &DeviceKey, trigger: Trigger, now: Ms, pass: &mut Pass) {
        let Some(grant) = self.scheduler.request(key, trigger, now) else {
            return;
        };
        if let Some(slot) = self.engines.get_mut(&gid) {
            let actions = slot.engine.on_grant(&grant, now);
            self.apply(gid, actions, now, pass);
        }
    }

    /// Group whose engine receives grants on `key`: the registrant of a
    /// device, or the engine a group key belongs to.
    fn owner(&self, key: &DeviceKey) -> Option<GroupId> {
        self.owners.get(key).copied().or_else(|| {
            self.engines
                .iter()
                .find(|(_, slot)| slot.key == *key)
                .map(|(&gid, _)| gid)
        })
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
                    timeout_ms,
                } => {
                    if self.routes.get(&collection) == Some(&gid)
                        && self.scheduler.admit_send(grant, now)
                    {
                        pass.out.push(HubOutput::Send {
                            collection,
                            bytes,
                            timeout_ms,
                        });
                    }
                }
                Action::Register { key, caps, initial } => {
                    self.register(gid, key, caps, &initial, now, pass);
                }
                Action::Unregister(key) => {
                    if self.owners.get(&key) == Some(&gid) {
                        self.release(&key, now, pass);
                    }
                }
                Action::Update(update) => {
                    if self.owners.get(&update.key) != Some(&gid) {
                        continue;
                    }
                    self.note_presence(&update.key, update.presence);
                    if let Some(state) = self.states.get_mut(&update.key) {
                        state.apply(&update, now);
                        if update.presence.is_none() {
                            state.presence = Presence::Online;
                        }
                        pass.changed = true;
                    }
                }
                Action::Asleep(key) => {
                    if self.owners.get(&key) != Some(&gid) {
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
                Action::WantGrant { key, trigger } => {
                    if self.owner(&key) != Some(gid) {
                        continue;
                    }
                    // A connect is a sign of life even when the min gap refuses the grant.
                    if trigger == Trigger::Connected {
                        self.scheduler.mark_awake(&key);
                    }
                    self.grant(gid, &key, trigger, now, pass);
                }
            }
        }
    }

    /// Registers `key` for `gid`'s engine. A key known from an earlier
    /// registration (e.g. a reconnect within grace) keeps its entry, its last
    /// values and its scheduler history.
    fn register(
        &mut self,
        gid: GroupId,
        key: DeviceKey,
        caps: Capabilities,
        initial: &DeviceUpdate,
        now: Ms,
        pass: &mut Pass,
    ) {
        let Some(slot) = self.engines.get(&gid) else {
            return;
        };
        let verification = self.verification(slot.ids);
        let product = slot.product.clone();

        self.owners.insert(key.clone(), gid);
        self.grace.remove(&key);
        self.scheduler.register(key.clone(), caps);
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
        state.apply(initial, now);
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

    /// Catalog status for a group's `(vid, pid)`: device entry, else vendor family, else `Generic`.
    fn verification(&self, ids: Option<(u16, u16)>) -> Verification {
        let Some((vid, pid)) = ids else {
            return Verification::Generic;
        };
        if let Some(device) = self.catalog.device(vid, pid) {
            return device.status;
        }
        // M1: only HID++ engines exist; the engine's kind joins this lookup with the second one.
        self.catalog
            .family(vid, EngineKind::Hidpp)
            .map_or(Verification::Generic, |family| family.status)
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
            timeout_ms: 1_000,
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

    fn pushing() -> Capabilities {
        Capabilities {
            pushes_battery: true,
            query_wakes: false,
            min_query_gap_ms: 0,
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
        }
    }

    fn register(key: &DeviceKey, level: Option<Level>) -> Action {
        register_with(key, pushing(), level)
    }

    fn sends(out: &[HubOutput]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|o| match o {
                HubOutput::Send { bytes, .. } => Some(bytes.clone()),
                HubOutput::StateChanged => None,
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
                    timeout_ms: 1_000,
                },
                HubOutput::Send {
                    collection: coll(1),
                    bytes: vec![0xA2],
                    timeout_ms: 1_000,
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
    fn resume_requests_grants_for_registered_devices_only() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register(&a, None)]);
        f.borrow_mut()
            .on_grant
            .push_back(vec![send(THIS_GRANT, coll(1), 0xD0)]);
        let idle = fake();
        let mut hub = hub(&[(1, &f), (2, &idle)]);
        hub.handle(logi(1), 0);
        hub.handle(logi(2), 0);

        let out = hub.handle(HubInput::Resume, 1_000);

        assert_eq!(sends(&out), vec![vec![0xD0]]);
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (a.clone(), Trigger::Resume)
            ]
        );
        assert_eq!(grants(&idle), vec![(group_key(2), Trigger::Arrival)]);

        hub.handle(HubInput::RefreshNow, 2_000);
        assert_eq!(grants(&f).last(), Some(&(a, Trigger::RefreshNow)));
        assert_eq!(grants(&idle).len(), 1);
    }

    #[test]
    fn want_grant_connected_delivers_grant() {
        let f = fake();
        let a = dev("dev-a");
        f.borrow_mut().on_grant.push_back(vec![register(&a, None)]);
        f.borrow_mut().on_report.push_back(vec![Action::WantGrant {
            key: a.clone(),
            trigger: Trigger::Connected,
        }]);
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
        f.borrow_mut()
            .on_grant
            .push_back(vec![register(&a, Some(Level::Percent(50)))]);
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

        let out = hub.handle(HubInput::Tick, 1_000 + 60_000);
        assert_eq!(out, vec![HubOutput::StateChanged]);
        assert!(hub.snapshot().is_empty());
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
            .push_back(vec![register(&h, None)]);
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
        assert_eq!(s.name, "Mouse");
        assert_eq!(s.kind, DeviceKind::Mouse);
        assert_eq!(s.presence, Presence::Online);
        assert_eq!(s.source, Source::Query);
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
    fn refused_connected_still_clears_asleep() {
        let f = fake();
        let a = dev("dev-a");
        let gap = Capabilities {
            min_query_gap_ms: 10_000,
            ..pushing()
        };
        f.borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, gap, None)]);
        f.borrow_mut()
            .on_report
            .push_back(vec![Action::Asleep(a.clone())]);
        f.borrow_mut().on_report.push_back(vec![Action::WantGrant {
            key: a.clone(),
            trigger: Trigger::Connected,
        }]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::RefreshNow, 0);

        hub.handle(report(1), 1);
        hub.handle(report(1), 2);
        assert_eq!(grants(&f).len(), 2, "Connected inside the gap is refused");

        hub.handle(HubInput::RefreshNow, 10_000);
        assert_eq!(
            grants(&f),
            vec![
                (group_key(1), Trigger::Arrival),
                (a.clone(), Trigger::RefreshNow),
                (a, Trigger::RefreshNow),
            ]
        );
    }

    #[test]
    fn reregister_within_grace_keeps_scheduler_history() {
        let a = dev("dev-a");
        let gap = Capabilities {
            min_query_gap_ms: 100_000,
            ..pushing()
        };
        let (first, second) = (fake(), fake());
        first
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, gap, None)]);
        second
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, gap, None)]);
        let mut hub = hub(&[(1, &first), (2, &second)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::RefreshNow, 0);
        hub.handle(HubInput::GroupRemoved(GroupId(1)), 1_000);
        hub.handle(logi(2), 2_000);

        hub.handle(HubInput::RefreshNow, 3_000);

        assert_eq!(grants(&second), vec![(group_key(2), Trigger::Arrival)]);
    }

    #[test]
    fn grace_expiry_forgets_scheduler_history() {
        let a = dev("dev-a");
        let gap = Capabilities {
            min_query_gap_ms: 100_000,
            ..pushing()
        };
        let (first, second) = (fake(), fake());
        first
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, gap, None)]);
        second
            .borrow_mut()
            .on_grant
            .push_back(vec![register_with(&a, gap, None)]);
        let mut hub = hub(&[(1, &first), (2, &second)]);
        hub.handle(logi(1), 0);
        hub.handle(HubInput::RefreshNow, 0);
        hub.handle(HubInput::GroupRemoved(GroupId(1)), 1_000);
        hub.handle(HubInput::Tick, 61_000);
        hub.handle(logi(2), 62_000);

        hub.handle(HubInput::RefreshNow, 63_000);

        assert_eq!(grants(&second).last(), Some(&(a, Trigger::RefreshNow)));
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
        let polled = Capabilities {
            pushes_battery: false,
            ..pushing()
        };
        f.borrow_mut().on_grant.push_back(vec![
            register_with(&poll, polled, None),
            register(&push, None),
        ]);
        let mut hub = hub(&[(1, &f)]);
        hub.handle(logi(1), 0);

        hub.handle(HubInput::Activity(push), 10);
        hub.handle(HubInput::Activity(poll.clone()), 10);

        assert_eq!(
            grants(&f),
            vec![(group_key(1), Trigger::Arrival), (poll, Trigger::Activity)]
        );
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
            Action::WantGrant {
                key: a.clone(),
                trigger: Trigger::Connected,
            },
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
}
