//! HyperX HX66 and Cloud Alpha 21 BB dongles, battery and presence only.

use crate::catalog::{CollectionSelector, DeviceEntry, EngineKind, HyperxReport};
use crate::engine::{Action, BytePattern, Engine, WriteRule};
use crate::hid::{
    Collection, CollectionId, CollectionRole, GroupId, HidGroup, ReportKind, ReportOrigin,
};
use crate::model::{Charge, DeviceKey, DeviceKind, DeviceUpdate, Level, Presence, Source, Via};
use crate::scheduler::{Capabilities, Grant, GrantId, Trigger};
use crate::time::Ms;

use super::Match;

pub(super) const COMMAND_ROLE: CollectionRole = CollectionRole(1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Query {
    Link,
    Battery,
    Charge,
}

const fn packet<const N: usize>(prefix: &[u8]) -> [u8; N] {
    let mut bytes = [0; N];
    let mut i = 0;
    while i < prefix.len() {
        bytes[i] = prefix[i];
        i += 1;
    }
    bytes
}

const HX66: [&[u8]; 3] = [
    &packet::<62>(&[0x66, 0x82]),
    &packet::<62>(&[0x66, 0x89]),
    &packet::<62>(&[0x66, 0x8A]),
];
const ALPHA: [&[u8]; 3] = [
    &packet::<31>(&[0x21, 0xBB, 0x03]),
    &packet::<31>(&[0x21, 0xBB, 0x0B]),
    &packet::<31>(&[0x21, 0xBB, 0x0C]),
];

const fn rules(packets: [&'static [u8]; 3]) -> [WriteRule; 3] {
    const fn rule(bytes: &'static [u8]) -> WriteRule {
        WriteRule::Bytes {
            role: COMMAND_ROLE,
            kind: ReportKind::Output,
            wire_len: bytes.len(),
            pattern: BytePattern::Exact(bytes),
        }
    }
    [rule(packets[0]), rule(packets[1]), rule(packets[2])]
}

impl HyperxReport {
    fn command(self, query: Query) -> &'static [u8] {
        (match self {
            Self::Hx66 => HX66,
            Self::Alpha21bb => ALPHA,
        })[query as usize]
    }

    fn matches(self, selector: &CollectionSelector, c: &Collection) -> bool {
        let width = self.command(Query::Link).len();
        usize::from(c.input_len) == width
            && usize::from(c.output_len) == width
            && selector.matches_descriptor(c)
    }

    fn caps(self) -> Capabilities {
        let alpha = self == Self::Alpha21bb;
        Capabilities {
            pushes_battery: !alpha,
            query_wakes: false,
            audio: alpha,
            awake_signal: false,
            charging: false,
            min_query_gap_ms: if alpha { 300_000 } else { 0 },
        }
    }

    fn decode(self, bytes: &[u8]) -> Option<(Option<Query>, Event)> {
        if bytes.len() != self.command(Query::Link).len() {
            return None;
        }
        // Replies and pushes have different selectors, even on the same open handle.
        // Only documented fields are read; e.g. Alpha's 0B has opaque trailing bytes.
        use Event::{Battery, Charging, Link, Mute};
        Some(match (self, bytes) {
            (Self::Hx66, [0x66, 0x82, state @ (0 | 1), ..]) => {
                (Some(Query::Link), Link(*state == 1))
            }
            (Self::Hx66, [0x66, 0x0B | 0x0F, state @ (0 | 1), ..]) => (None, Link(*state == 1)),
            (Self::Hx66, [0x66, cmd @ (0x89 | 0x0D), hi, lo, level, ..]) => {
                let reply = *cmd == 0x89;
                let millivolts = u16::from_be_bytes([*hi, *lo]);
                let event = if millivolts == 0 {
                    if !reply {
                        return None; // The off burst includes a zero-voltage battery push.
                    }
                    Link(false)
                } else if *level <= 100 {
                    Battery(*level, Some(millivolts))
                } else {
                    return None;
                };
                (reply.then_some(Query::Battery), event)
            }
            (Self::Hx66, [0x66, cmd @ (0x8A | 0x0C), value, ..]) => (
                (*cmd == 0x8A).then_some(Query::Charge),
                Charging(charge(*value, true)?),
            ),
            (Self::Hx66, [0x66, 0x0A, 0 | 1, ..]) => (None, Mute),
            (Self::Alpha21bb, [0x21, 0xBB, cmd @ (0x03 | 0x24), state @ (1 | 2), ..]) => {
                ((*cmd == 0x03).then_some(Query::Link), Link(*state == 2))
            }
            (Self::Alpha21bb, [0x21, 0xBB, cmd @ (0x0B | 0x25), level @ 0..=100, ..]) => (
                (*cmd == 0x0B).then_some(Query::Battery),
                Battery(*level, None),
            ),
            (Self::Alpha21bb, [0x21, 0xBB, cmd @ (0x0C | 0x26), value, ..]) => (
                (*cmd == 0x0C).then_some(Query::Charge),
                Charging(charge(*value, false)?),
            ),
            (Self::Alpha21bb, [0x21, 0xBB, 0x23, 0 | 1, ..]) => (None, Mute),
            _ => return None,
        })
    }
}

fn charge(value: u8, has_full_state: bool) -> Option<Charge> {
    Some(match value {
        0 => Charge::NotCharging,
        1 => Charge::Charging,
        2 if has_full_state => Charge::Full,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum Event {
    Link(bool),
    Battery(u8, Option<u16>),
    Charging(Charge),
    Mute,
}

#[derive(Clone, Copy)]
struct PendingRead {
    grant: GrantId,
    query: Query,
    expires: Ms,
    remaining: u16,
}

pub(super) struct HyperxEngine {
    group_key: DeviceKey,
    key: DeviceKey,
    report: HyperxReport,
    command: CollectionId,
    pending: Option<PendingRead>,
    online: bool,
    registered: bool,
    charge: Option<Charge>,
}

impl HyperxEngine {
    fn config(entry: &DeviceEntry) -> Option<(HyperxReport, &CollectionSelector)> {
        let command = entry.send.as_ref()?;
        if entry.engine != EngineKind::Hyperx
            || entry.vid != 0x03F0
            || entry.receive.is_some()
            || command.usage_page.is_none()
            || command.usage.is_none()
            || command.interface.is_none()
        {
            return None;
        }
        Some((entry.hyperx?, command))
    }

    pub(super) fn match_collection(
        group: &HidGroup,
        entry: &DeviceEntry,
        product: &mut impl FnMut(usize, &CollectionSelector) -> Option<bool>,
    ) -> Match<(CollectionId, u8)> {
        let Some((report, selector)) = Self::config(entry) else {
            return Match::Absent;
        };
        let mut command = None;
        for (index, collection) in group.collections.iter().enumerate() {
            if !report.matches(selector, collection) {
                continue;
            }
            if selector.product.is_some() {
                match product(index, selector) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return Match::Denied,
                }
            }
            if command.replace(collection.id).is_some() {
                return Match::Denied;
            }
        }
        command.map_or(Match::Absent, |id| {
            Match::Unique((id, report.command(Query::Link)[0]))
        })
    }

    pub(super) fn from_selection(
        group: GroupId,
        command: &Collection,
        entry: &DeviceEntry,
    ) -> Option<Self> {
        let (report, selector) = Self::config(entry)?;
        if command.vid != entry.vid
            || !entry.pids.contains(&command.pid)
            || !report.matches(selector, command)
        {
            return None;
        }
        Some(Self {
            group_key: DeviceKey(format!("hyperx-group:{}", group.0)),
            key: DeviceKey(format!("hyperx:{:04x}", command.pid)),
            report,
            command: command.id,
            pending: None,
            online: false,
            registered: false,
            charge: None,
        })
    }

    pub(super) fn is_fallback_key(key: &DeviceKey) -> bool {
        key.0.starts_with("hyperx:")
    }

    fn send(&mut self, mut pending: PendingRead) -> Option<Action> {
        pending.remaining = pending.remaining.checked_sub(1)?;
        self.pending = Some(pending);
        Some(Action::Send {
            grant: pending.grant,
            collection: self.command,
            kind: ReportKind::Output,
            bytes: self.report.command(pending.query).to_vec(),
            deadline: None,
        })
    }

    fn observe(&mut self, event: Event, source: Source) -> Option<Action> {
        let (level, millivolts, charge) = match event {
            Event::Link(up) => {
                if !up {
                    self.pending = None;
                    self.charge = None;
                }
                if self.online == up || !self.registered {
                    self.online = up;
                    return None;
                }
                self.online = up;
                (None, None, (!up).then_some(Charge::Unknown))
            }
            Event::Battery(level, millivolts) => {
                self.online = true;
                (Some(Level::Percent(level)), millivolts, self.charge.take())
            }
            Event::Charging(value) => {
                if !self.online {
                    return None;
                }
                if !self.registered {
                    self.charge = Some(value);
                    return None;
                }
                (None, None, Some(value))
            }
            Event::Mute => return None, // The battery model has no mute state.
        };
        let registering = !self.registered;
        let update = DeviceUpdate {
            key: self.key.clone(),
            name: None,
            kind: registering.then_some(DeviceKind::Headset),
            via: registering.then_some(Via::Receiver),
            level,
            millivolts,
            charge,
            presence: Some(if self.online {
                Presence::Online
            } else {
                Presence::Offline
            }),
            source,
        };
        if registering {
            self.registered = true;
            Some(Action::Register {
                key: self.key.clone(),
                caps: self.report.caps(),
                initial: update,
                wpid: None,
            })
        } else {
            Some(Action::Update(update))
        }
    }
}

impl Engine for HyperxEngine {
    fn group_key(&self) -> DeviceKey {
        self.group_key.clone()
    }

    fn group_one_shots(&self) -> bool {
        true
    }

    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action> {
        let allowed = match grant.trigger {
            Trigger::Arrival | Trigger::Resume | Trigger::RefreshNow => {
                grant.subject == self.group_key
            }
            Trigger::Activity => {
                self.report == HyperxReport::Alpha21bb && self.online && grant.subject == self.key
            }
            _ => false,
        };
        if !allowed || now > grant.expires_ms || self.pending.is_some_and(|p| now <= p.expires) {
            return Vec::new();
        }
        self.send(PendingRead {
            grant: grant.id,
            query: Query::Link,
            expires: grant.expires_ms,
            remaining: grant.max_requests,
        })
        .into_iter()
        .collect()
    }

    fn on_report(
        &mut self,
        collection: CollectionId,
        origin: ReportOrigin,
        bytes: &[u8],
        now: Ms,
    ) -> Vec<Action> {
        if collection != self.command || origin != ReportOrigin::Input {
            return Vec::new();
        }
        let Some((reply, event)) = self.report.decode(bytes) else {
            return Vec::new();
        };
        let Some(query) = reply else {
            return self.observe(event, Source::Push).into_iter().collect();
        };
        let Some(pending) = self
            .pending
            .filter(|p| p.query == query && now <= p.expires)
        else {
            return Vec::new();
        };
        // Same-selector traffic from another open handle is indistinguishable. It can
        // satisfy this step only once, never open (or extend/restart) a grant.
        self.pending = None;
        let mut actions: Vec<_> = self.observe(event, Source::Query).into_iter().collect();
        let next = match (self.report, event) {
            (HyperxReport::Hx66, Event::Link(true)) => Some(Query::Battery),
            (HyperxReport::Hx66, Event::Battery(..))
            | (HyperxReport::Alpha21bb, Event::Link(true)) => Some(Query::Charge),
            (HyperxReport::Alpha21bb, Event::Charging(_)) => Some(Query::Battery),
            _ => None,
        };
        if let Some(query) = next
            && let Some(action) = self.send(PendingRead { query, ..pending })
        {
            actions.push(action);
        }
        actions
    }

    fn on_tick(&mut self, _now: Ms) -> Vec<Action> {
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Ms> {
        None
    }

    fn allowed_writes(&self) -> &'static [WriteRule] {
        const HX66_RULES: [WriteRule; 3] = rules(HX66);
        const ALPHA_RULES: [WriteRule; 3] = rules(ALPHA);
        match self.report {
            HyperxReport::Hx66 => &HX66_RULES,
            HyperxReport::Alpha21bb => &ALPHA_RULES,
        }
    }

    fn collection_role(&self, collection: CollectionId) -> Option<CollectionRole> {
        (collection == self.command).then_some(COMMAND_ROLE)
    }
}
