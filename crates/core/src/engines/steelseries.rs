//! SteelSeries Nova dongles (unnumbered) and stations (numbered), battery only.

use crate::catalog::{CollectionSelector, DeviceEntry, EngineKind, SteelseriesReport};
use crate::engine::{Action, BytePattern, Engine, WriteRule};
use crate::hid::{
    Collection, CollectionId, CollectionRole, GroupId, HidGroup, ReportKind, ReportOrigin,
};
use crate::model::{Charge, DeviceKey, DeviceKind, DeviceUpdate, Level, Presence, Source, Via};
use crate::scheduler::{Capabilities, Grant, GrantId, Trigger};
use crate::time::Ms;

use super::Match;

pub(super) const COMMAND_ROLE: CollectionRole = CollectionRole(1);
const CAPS: Capabilities = Capabilities {
    pushes_battery: true,
    query_wakes: false,
    audio: false,
    awake_signal: false,
    charging: false,
    min_query_gap_ms: 0,
};

// Only the first compatible reply inside the status window is attributed to our
// query. B0 lacks a transaction ID; an interleaved GG reply is indistinguishable.
const REPLY_MS: Ms = 2_000;

const fn b0<const N: usize>(id: u8) -> [u8; N] {
    let mut bytes = [0; N];
    bytes[0] = id;
    bytes[1] = 0xB0;
    bytes
}

const DONGLE_B0: [u8; 65] = b0(0);
const STATION_06_B0: [u8; 64] = b0(6);
const STATION_01_B0: [u8; 64] = b0(1);

const fn rule(bytes: &'static [u8]) -> [WriteRule; 1] {
    [WriteRule::Bytes {
        role: COMMAND_ROLE,
        kind: ReportKind::Output,
        wire_len: bytes.len(),
        pattern: BytePattern::Exact(bytes),
    }]
}

impl SteelseriesReport {
    fn station(self) -> bool {
        matches!(self, Self::Station06Steps | Self::Station01Percent)
    }

    fn events_on_command(self) -> bool {
        self.station() || self == Self::Arctis7Plus
    }

    fn request(self) -> &'static [u8] {
        match self {
            Self::Station06Steps => &STATION_06_B0,
            Self::Station01Percent => &STATION_01_B0,
            _ => &DONGLE_B0,
        }
    }

    fn status_len(self) -> u16 {
        match self {
            Self::GameBuds => 8,
            _ if self.station() => 16,
            _ => 6,
        }
    }

    fn event_len(self) -> u16 {
        match self {
            Self::GameBuds => 6,
            _ if self.station() => 5,
            _ => 3,
        }
    }

    fn level(self, raw: u8) -> Option<Level> {
        // Linux hid-steelseries-arctis.c and LAM: Nova 7 uses quarter steps;
        // LAM/omni-linux show Nova Pro's eight steps, truncated to whole percent.
        let percent = match self {
            Self::Nova7Steps | Self::Arctis7Plus if raw <= 4 => raw * 25,
            Self::Station06Steps if raw <= 8 => (u16::from(raw) * 100 / 8) as u8,
            Self::Nova7Percent | Self::Nova5Percent | Self::Station01Percent if raw <= 100 => raw,
            _ => return None,
        };
        Some(Level::Percent(percent))
    }
}

/// A headset uses bit 0; GameBuds use bits 0 and 1 for the active buds.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Link(u8);

impl Link {
    const DOWN: Self = Self(0);

    fn headset(up: bool) -> Self {
        Self(u8::from(up))
    }

    fn buds(left: u8, right: u8) -> Option<Self> {
        if left > 3 || right > 3 {
            return None;
        }
        Some(Self(u8::from(left == 3) | (u8::from(right == 3) << 1)))
    }

    fn up(self) -> bool {
        self != Self::DOWN
    }

    fn bud_level(self, left: u8, right: u8) -> Option<Level> {
        match self.0 {
            1 => Some(Level::Percent(left)),
            2 => Some(Level::Percent(right)),
            3 => Some(Level::Percent(left.min(right))),
            _ => None,
        }
    }
}

fn dongle_charge(value: u8) -> Option<Charge> {
    Some(match value {
        0 => Charge::Unknown,
        1 => Charge::Charging,
        2 => Charge::NotCharging,
        3 => Charge::Discharging,
        _ => return None,
    })
}

fn radio_link(value: u8) -> Option<Link> {
    match value {
        1 | 2 | 4 | 8 => Some(Link::headset(value == 8)),
        _ => None,
    }
}

fn station_charge(value: u8) -> Option<Charge> {
    Some(match value {
        1 => Charge::Unknown, // Not connected.
        2 => Charge::Charging,
        4 => Charge::Full,
        8 => Charge::Discharging,
        _ => return None,
    })
}

fn unique_collection<'a>(
    group: &'a HidGroup,
    selector: &CollectionSelector,
    mut eligible: impl FnMut(&Collection) -> bool,
    product: &mut impl FnMut(usize, &CollectionSelector) -> Option<bool>,
) -> Match<&'a Collection> {
    let mut chosen = None;
    for (index, collection) in group.collections.iter().enumerate() {
        if eligible(collection) && selector.matches_descriptor(collection) {
            if selector.product.is_some() {
                match product(index, selector) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return Match::Denied,
                }
            }
            if chosen.replace(collection).is_some() {
                return Match::Denied;
            }
        }
    }
    chosen.map_or(Match::Absent, Match::Unique)
}

struct Arrival {
    grant: GrantId,
    expires: Ms,
}

pub(super) struct SteelseriesEngine {
    group_key: DeviceKey,
    key: DeviceKey,
    report: SteelseriesReport,
    command: CollectionId,
    events: Option<CollectionId>,
    arrival: Option<Arrival>,
    settle_until: Ms,
    reply_until: Option<Ms>,
    link: Option<Link>,
    registered: bool,
}

impl SteelseriesEngine {
    pub(super) fn match_collections(
        group: &HidGroup,
        entry: &DeviceEntry,
        product: &mut impl FnMut(usize, &CollectionSelector) -> Option<bool>,
    ) -> Match<(CollectionId, Option<CollectionId>, u8)> {
        let Some((report, command, events)) = Self::config(entry) else {
            return Match::Absent;
        };
        let command =
            match unique_collection(group, command, |c| Self::command_width(c, report), product) {
                Match::Unique(c) => c.id,
                Match::Absent => return Match::Absent,
                Match::Denied => return Match::Denied,
            };
        let events = match unique_collection(
            group,
            events,
            |c| c.input_len >= report.event_len(),
            product,
        ) {
            Match::Unique(c) if c.id != command => Some(c.id),
            Match::Absent => None, // The command collection also carries B0 input.
            _ => return Match::Denied,
        };
        Match::Unique((command, events, report.request()[0]))
    }

    fn config(
        entry: &DeviceEntry,
    ) -> Option<(SteelseriesReport, &CollectionSelector, &CollectionSelector)> {
        let command = entry.send.as_ref()?;
        let events = entry.receive.as_ref()?;
        if entry.engine != EngineKind::Steelseries
            || entry.vid != 0x1038
            || [command, events].iter().any(|selector| {
                selector.usage_page.is_none()
                    || selector.usage.is_none()
                    || selector.interface.is_none()
            })
        {
            return None;
        }
        Some((entry.steelseries?, command, events))
    }

    fn command_width(c: &Collection, report: SteelseriesReport) -> bool {
        c.input_len >= report.status_len() && usize::from(c.output_len) == report.request().len()
    }

    pub(super) fn from_selection(
        group: GroupId,
        command: &Collection,
        events: Option<&Collection>,
        entry: &DeviceEntry,
    ) -> Option<Self> {
        let (report, command_selector, event_selector) = Self::config(entry)?;
        if command.vid != entry.vid
            || !entry.pids.contains(&command.pid)
            || !command_selector.matches_descriptor(command)
            || !Self::command_width(command, report)
            || events.is_some_and(|c| {
                c.vid != command.vid
                    || c.pid != command.pid
                    || c.id == command.id
                    || !event_selector.matches_descriptor(c)
                    || c.input_len < report.event_len()
            })
        {
            return None;
        }
        Some(Self {
            group_key: DeviceKey(format!("steelseries-group:{}", group.0)),
            key: DeviceKey(format!("steelseries:{:04x}", command.pid)),
            report,
            command: command.id,
            events: events.map(|c| c.id),
            arrival: None,
            settle_until: 0,
            reply_until: None,
            link: None,
            registered: false,
        })
    }

    pub(super) fn is_fallback_key(key: &DeviceKey) -> bool {
        key.0.starts_with("steelseries:")
    }

    fn request(&mut self, grant: GrantId, now: Ms) -> Vec<Action> {
        self.reply_until = Some(now.saturating_add(REPLY_MS));
        vec![Action::Send {
            grant,
            collection: self.command,
            kind: ReportKind::Output,
            bytes: self.report.request().to_vec(),
            deadline: None,
        }]
    }

    fn update(
        &mut self,
        up: bool,
        level: Option<Level>,
        charge: Option<Charge>,
        source: Source,
    ) -> Vec<Action> {
        if !up && !self.registered {
            return Vec::new();
        }
        let mut update = DeviceUpdate {
            key: self.key.clone(),
            name: None,
            kind: None,
            via: None,
            level: if up { level } else { None },
            millivolts: None,
            charge: if up { charge } else { Some(Charge::Unknown) },
            presence: Some(if up {
                Presence::Online
            } else {
                Presence::Offline
            }),
            source,
        };
        if self.registered {
            vec![Action::Update(update)]
        } else {
            self.registered = true;
            update.kind = Some(DeviceKind::Headset);
            update.via = Some(Via::Receiver);
            vec![Action::Register {
                key: self.key.clone(),
                caps: CAPS,
                initial: update,
                wpid: None,
            }]
        }
    }

    fn link_event(&mut self, link: Link) -> Vec<Action> {
        let previous = self.link.replace(link);
        if previous.is_some_and(|previous| previous.up() == link.up()) {
            return Vec::new();
        }
        let mut actions = self.update(link.up(), None, None, Source::Push);
        if link.up() && previous == Some(Link::DOWN) {
            actions.push(Action::Connected(self.group_key.clone()));
        }
        actions
    }

    fn status(&mut self, bytes: &[u8], now: Ms) -> Option<Vec<Action>> {
        if bytes.len() < usize::from(self.report.status_len()) {
            return None;
        }
        let (link, level, charge) = match self.report {
            SteelseriesReport::GameBuds => {
                // LAM's GameBuds RF fields; BT states at [2..4] do not describe this link.
                let link = Link::buds(bytes[4], bytes[5])?;
                let (left, right) = (bytes[6], bytes[7]);
                if left > 100 || right > 100 {
                    return None;
                }
                (link, link.bud_level(left, right), Charge::Unknown)
            }
            report if report.station() => {
                let link = radio_link(bytes[14])?;
                let charge = station_charge(bytes[15])?;
                let link = if charge == Charge::Unknown {
                    Link::DOWN
                } else {
                    link
                };
                (link, Some(report.level(bytes[6])?), charge)
            }
            report => {
                // Windows supplies a zero before B0; transport code must not strip it.
                let conn = bytes[2];
                if conn > 3 {
                    return None;
                }
                let offset = if report == SteelseriesReport::Nova5Percent {
                    4
                } else {
                    3
                };
                (
                    Link::headset(conn == 3),
                    Some(report.level(bytes[offset])?),
                    dongle_charge(bytes[offset + 1])?,
                )
            }
        };
        self.link = Some(link);
        let source = if self.reply_until.take().is_some_and(|until| now <= until) {
            Source::Query
        } else {
            Source::Push
        };
        // A status is an observation, even when it changes the known link state.
        Some(self.update(link.up(), level, Some(charge), source))
    }

    fn dongle_event(&mut self, bytes: &[u8]) -> Option<Vec<Action>> {
        let &[0, op, value, ..] = bytes else {
            return None;
        };
        if self.report == SteelseriesReport::GameBuds {
            // GameBuds B9/BB are settings; only B5 carries the RF link here.
            return match bytes {
                &[0, 0xB5, _, _, left, right, ..] => {
                    Some(self.link_event(Link::buds(left, right)?))
                }
                &[0, 0xB7, left @ 0..=100, right @ 0..=100, ..] => {
                    let level = self.link?.bud_level(left, right)?;
                    Some(self.update(true, Some(level), None, Source::Push))
                }
                _ => None,
            };
        }
        match op {
            0xB9 if value <= 3 => Some(self.link_event(Link::headset(value == 3))),
            0xB7 if self.link != Some(Link::DOWN) => {
                Some(self.update(true, Some(self.report.level(value)?), None, Source::Push))
            }
            0xBB if self.link.is_some_and(Link::up) || (self.link.is_none() && self.registered) => {
                Some(self.update(true, None, Some(dongle_charge(value)?), Source::Push))
            }
            _ => None,
        }
    }

    fn station_event(&mut self, bytes: &[u8]) -> Option<Vec<Action>> {
        match bytes {
            &[0x07, 0xB5, _, _, link, ..] => Some(self.link_event(radio_link(link)?)),
            &[0x07, 0xB7, raw, _, state, ..] => {
                let level = self.report.level(raw)?;
                let charge = station_charge(state)?;
                if charge == Charge::Unknown {
                    // Disconnected status is an observation, not a link-up trigger.
                    return Some(self.link_event(Link::DOWN));
                }
                if self.link == Some(Link::DOWN) {
                    return None;
                }
                Some(self.update(true, Some(level), Some(charge), Source::Push))
            }
            _ => None,
        }
    }
}

impl Engine for SteelseriesEngine {
    fn group_key(&self) -> DeviceKey {
        self.group_key.clone()
    }

    fn group_one_shots(&self) -> bool {
        true
    }

    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action> {
        if grant.subject != self.group_key {
            return Vec::new();
        }
        match grant.trigger {
            Trigger::Arrival => {
                let settle = if self.report.station() { 5_000 } else { 1_500 };
                self.settle_until = now.saturating_add(settle);
                self.arrival = (grant.max_requests > 0 && self.settle_until <= grant.expires_ms)
                    .then_some(Arrival {
                        grant: grant.id,
                        expires: grant.expires_ms,
                    });
                Vec::new()
            }
            Trigger::Connected | Trigger::Resume | Trigger::RefreshNow
                if now >= self.settle_until && grant.max_requests > 0 =>
            {
                // If this event beat a due arrival Tick, a single request serves both.
                self.arrival = None;
                self.request(grant.id, now)
            }
            _ => Vec::new(),
        }
    }

    fn on_report(
        &mut self,
        collection: CollectionId,
        origin: ReportOrigin,
        bytes: &[u8],
        now: Ms,
    ) -> Vec<Action> {
        if origin != ReportOrigin::Input {
            return Vec::new();
        }
        let command = collection == self.command;
        if command && bytes.starts_with(&self.report.request()[..2]) {
            return self.status(bytes, now).unwrap_or_default();
        }
        if !((command && self.report.events_on_command()) || self.events == Some(collection)) {
            return Vec::new();
        }
        if self.report.station() {
            self.station_event(bytes)
        } else {
            self.dongle_event(bytes)
        }
        .unwrap_or_default()
    }

    fn on_tick(&mut self, now: Ms) -> Vec<Action> {
        if now >= self.settle_until
            && let Some(pending) = self.arrival.take()
            && now <= pending.expires
        {
            return self.request(pending.grant, now);
        }
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Ms> {
        self.arrival.as_ref().map(|_| self.settle_until)
    }

    fn allowed_writes(&self) -> &'static [WriteRule] {
        const DONGLE: [WriteRule; 1] = rule(&DONGLE_B0);
        const STATION_06: [WriteRule; 1] = rule(&STATION_06_B0);
        const STATION_01: [WriteRule; 1] = rule(&STATION_01_B0);
        match self.report {
            SteelseriesReport::Station06Steps => &STATION_06,
            SteelseriesReport::Station01Percent => &STATION_01,
            _ => &DONGLE,
        }
    }

    fn collection_role(&self, collection: CollectionId) -> Option<CollectionRole> {
        (collection == self.command).then_some(COMMAND_ROLE)
    }
}
