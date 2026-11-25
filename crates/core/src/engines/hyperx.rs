//! HyperX HX66, Cloud Alpha 21 BB and NGENUITY universal receivers.

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

const WAKE_GRACE_MS: Ms = 2_000;
const UNIVERSAL_REPLY_MS: Ms = 1_000;

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
const UNIVERSAL: [u8; 64] = packet(&[0x50, 0x02]);

const fn output_rule(bytes: &'static [u8]) -> WriteRule {
    WriteRule::Bytes {
        role: COMMAND_ROLE,
        kind: ReportKind::Output,
        wire_len: bytes.len(),
        pattern: BytePattern::Exact(bytes),
    }
}

const fn rules(packets: [&'static [u8]; 3]) -> [WriteRule; 3] {
    [
        output_rule(packets[0]),
        output_rule(packets[1]),
        output_rule(packets[2]),
    ]
}

impl HyperxReport {
    fn legacy_commands(self) -> Option<&'static [&'static [u8]; 3]> {
        match self {
            Self::Hx66 => Some(&HX66),
            Self::Alpha21bb => Some(&ALPHA),
            Self::Universal => None,
        }
    }

    fn first_output(self) -> &'static [u8] {
        self.legacy_commands()
            .map_or(&UNIVERSAL, |commands| commands[0])
    }

    fn matches(self, selector: &CollectionSelector, c: &Collection) -> bool {
        let width = self.first_output().len();
        usize::from(c.input_len) == width
            && usize::from(c.output_len) == width
            && selector.matches_descriptor(c)
    }

    fn caps(self) -> Capabilities {
        let alpha = self == Self::Alpha21bb;
        Capabilities {
            pushes_battery: self == Self::Hx66,
            query_wakes: self == Self::Universal,
            audio: alpha,
            awake_signal: false,
            charging: false,
            min_query_gap_ms: if alpha { 300_000 } else { 0 },
        }
    }

    fn decode_legacy(self, bytes: &[u8]) -> Option<(Option<Query>, Event)> {
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

#[derive(Clone, Copy)]
enum Wake {
    Grace {
        at: Ms,
        grant: Option<(GrantId, Ms)>,
    },
    Reply {
        until: Ms,
    },
}

pub(super) struct HyperxEngine {
    group_key: DeviceKey,
    key: DeviceKey,
    report: HyperxReport,
    kind: DeviceKind,
    command: CollectionId,
    pending: Option<PendingRead>,
    wake: Option<Wake>,
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
            Match::Unique((id, report.first_output()[0]))
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
            kind: entry.kind,
            command: command.id,
            pending: None,
            wake: None,
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
        let command = self.report.legacy_commands()?[pending.query as usize];
        self.pending = Some(pending);
        Some(Action::Send {
            grant: pending.grant,
            collection: self.command,
            kind: ReportKind::Output,
            bytes: command.to_vec(),
            deadline: None,
        })
    }

    fn universal_input(&mut self, bytes: &[u8], now: Ms) -> Vec<Action> {
        // Only dedicated target zero is admitted. Do not mask the routing bits.
        // FF ACKs/NACKs are not a battery response and never advance this state.
        match bytes {
            // Another handle's link-status reply can disprove a link, never establish one.
            [0xFB, 0x0A, state @ (0 | 1), ..] | [0x53, 0x01, state @ 0, ..] => {
                let up = *state == 1;
                if self.online == up {
                    return Vec::new();
                }
                self.wake = up.then(|| Wake::Grace {
                    at: now.saturating_add(WAKE_GRACE_MS),
                    grant: None,
                });
                self.observe(Event::Link(up), Source::Push)
                    .into_iter()
                    // Obtain authority on this input edge, but only consume it at grace expiry.
                    .chain(up.then(|| Action::Connected(self.group_key.clone())))
                    .collect()
            }
            [0x51, 0x02, level @ 0..=100, charge, _, _, lo, hi, ..] => {
                let charge = match charge {
                    0 => Charge::NotCharging,
                    1 => Charge::Charging,
                    2 => Charge::Full,
                    3 | 0xFF => Charge::Unknown,
                    _ => return Vec::new(),
                };
                // Push and reply share the same frame, with no transaction ID.
                let source = match self.wake {
                    Some(Wake::Grace { at, .. }) if now <= at => Source::Push,
                    Some(Wake::Reply { until }) if now <= until => Source::Query,
                    _ => return Vec::new(),
                };
                self.wake = None;
                let mv = u16::from_le_bytes([*lo, *hi]);
                vec![self.update(
                    Some(Level::Percent(*level)),
                    (mv != 0).then_some(mv),
                    Some(charge),
                    source,
                )]
            }
            _ => Vec::new(),
        }
    }

    fn observe(&mut self, event: Event, source: Source) -> Option<Action> {
        let (level, millivolts, charge) = match event {
            Event::Link(up) => {
                if !up {
                    self.pending = None;
                    self.charge = None;
                }
                let changed = self.online != up;
                self.online = up;
                return (changed && self.registered).then(|| Action::Presence {
                    key: self.key.clone(),
                    presence: if up {
                        Presence::Online
                    } else {
                        Presence::Offline
                    },
                });
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
        Some(self.update(level, millivolts, charge, source))
    }

    fn update(
        &mut self,
        level: Option<Level>,
        millivolts: Option<u16>,
        charge: Option<Charge>,
        source: Source,
    ) -> Action {
        let registering = !self.registered;
        let update = DeviceUpdate {
            key: self.key.clone(),
            name: None,
            kind: registering.then_some(self.kind),
            via: registering.then_some(Via::Receiver),
            level,
            millivolts,
            charge,
            presence: Some(Presence::Online),
            source,
        };
        if registering {
            self.registered = true;
            Action::Register {
                key: self.key.clone(),
                caps: self.report.caps(),
                initial: update,
                wpid: None,
            }
        } else {
            Action::Update(update)
        }
    }
}

impl Engine for HyperxEngine {
    fn group_key(&self) -> DeviceKey {
        self.group_key.clone()
    }

    fn group_one_shots(&self) -> bool {
        self.report != HyperxReport::Universal
    }

    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action> {
        if self.report == HyperxReport::Universal {
            if grant.trigger == Trigger::Connected
                && grant.subject == self.group_key
                && grant.max_requests != 0
                && let Some(Wake::Grace {
                    grant: permit @ None,
                    ..
                }) = &mut self.wake
            {
                *permit = Some((grant.id, grant.expires_ms));
            }
            return Vec::new();
        }
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
        if collection != self.command
            || origin != ReportOrigin::Input
            || bytes.len() != self.report.first_output().len()
        {
            return Vec::new();
        }
        if self.report == HyperxReport::Universal {
            return self.universal_input(bytes, now);
        }
        let Some((reply, event)) = self.report.decode_legacy(bytes) else {
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

    fn on_tick(&mut self, now: Ms) -> Vec<Action> {
        let Some(Wake::Grace { at, grant }) = self.wake else {
            return Vec::new();
        };
        if now < at {
            return Vec::new();
        }
        self.wake = None;
        let Some((grant, expires)) = grant.filter(|&(_, expires)| now <= expires) else {
            return Vec::new();
        };
        self.wake = Some(Wake::Reply {
            until: expires.min(now.saturating_add(UNIVERSAL_REPLY_MS)),
        });
        vec![Action::Send {
            grant,
            collection: self.command,
            kind: ReportKind::Output,
            bytes: UNIVERSAL.to_vec(),
            deadline: None,
        }]
    }

    fn next_deadline(&self) -> Option<Ms> {
        match self.wake {
            Some(Wake::Grace { at, .. }) => Some(at),
            _ => None,
        }
    }

    fn allowed_writes(&self) -> &'static [WriteRule] {
        const HX66_RULES: [WriteRule; 3] = rules(HX66);
        const ALPHA_RULES: [WriteRule; 3] = rules(ALPHA);
        const UNIVERSAL_RULES: [WriteRule; 1] = [output_rule(&UNIVERSAL)];
        match self.report {
            HyperxReport::Hx66 => &HX66_RULES,
            HyperxReport::Alpha21bb => &ALPHA_RULES,
            HyperxReport::Universal => &UNIVERSAL_RULES,
        }
    }

    fn collection_role(&self, collection: CollectionId) -> Option<CollectionRole> {
        (collection == self.command).then_some(COMMAND_ROLE)
    }
}
