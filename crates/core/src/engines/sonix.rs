//! Pulsar SONiX report-0 mailbox and (Fusion2) report-5 events.

use crate::catalog::{CollectionSelector, DeviceEntry, EngineKind};
use crate::engine::{Action, BytePattern, Engine, FeatureSetId, WriteRule};
use crate::hid::{
    Collection, CollectionId, CollectionRole, GroupId, HidGroup, ReportKind, ReportOrigin,
};
use crate::model::{Charge, DeviceKey, DeviceKind, DeviceUpdate, Level, Presence, Source, Via};
use crate::scheduler::{Capabilities, Grant, GrantId, Trigger};
use crate::time::Ms;

use super::Match;

pub(super) const COMMAND_ROLE: CollectionRole = CollectionRole(1);
pub(super) const FEATURE_ID: u8 = 0;
const FEATURE_LEN: usize = 65;
const CHECK_MS: Ms = 20;
const TRANSACTION_MS: Ms = 1_000;
// Includes the preflight GET (at time zero), subsequent GETs, and at most two SETs.
pub(crate) const OPERATION_BUDGET: u16 = (TRANSACTION_MS / CHECK_MS) as u16 + 2;
const RESET_MS: Ms = 40;
// Leave the documented recovery room for the same transaction, not a fresh deadline.
const RESEND_ROOM_MS: Ms = 350;
const EDGE_GRACE_MS: Ms = 2_000;
const MIN_QUERY_GAP_MS: Ms = 60_000;

const BATTERY_ECHO: [u8; 3] = [0x08, 0x81, 0x01];
const GET_MAILBOX: [u8; FEATURE_LEN] = [FEATURE_ID; FEATURE_LEN];
// Research §8.2: Windows ID byte, then 64 wire bytes. The little-endian sum
// of wire bytes 0..62 is 0x008A, not a checksum over the Windows prefix.
const READ_BATTERY: [u8; FEATURE_LEN] = {
    let [category, register, sub] = BATTERY_ECHO;
    let mut report = GET_MAILBOX;
    report[2] = category;
    report[3] = register;
    report[4] = sub;
    report[63] = 0x8A;
    report
};
const RULES: [WriteRule; 2] = [
    WriteRule::Bytes {
        role: COMMAND_ROLE,
        kind: ReportKind::GetFeature,
        wire_len: FEATURE_LEN,
        pattern: BytePattern::Exact(&GET_MAILBOX),
    },
    WriteRule::Bytes {
        role: COMMAND_ROLE,
        kind: ReportKind::SetFeature,
        wire_len: FEATURE_LEN,
        pattern: BytePattern::Exact(&READ_BATTERY),
    },
];

struct WakeGrace {
    at: Ms,
    grant: Option<(GrantId, Ms)>,
}

struct Query {
    // A completed response may be latched in the shared mailbox before our SET.
    preflight: [u8; FEATURE_LEN],
    matched_busy: bool,
    reset_since: Option<Ms>,
    resent: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MailboxIo {
    Reading,
    Writing(FeatureSetId),
    CheckAt(Ms),
}

struct Transaction {
    grant: GrantId,
    until: Ms,
    io: MailboxIo,
    /// No SET before the preflight GET finds a known nonbusy buffer.
    query: Option<Query>,
}

pub(super) struct SonixEngine {
    group_key: DeviceKey,
    key: DeviceKey,
    command: CollectionId,
    events: Option<(CollectionId, usize)>,
    linked: bool,
    registered: bool,
    grace: Option<WakeGrace>,
    transaction: Option<Transaction>,
    // A link-down cancels the conversation, but an already submitted overlapped
    // GET may still complete. Don't open a new conversation before its deadline.
    quiet_until: Ms,
}

impl SonixEngine {
    pub(super) fn match_collections(
        group: &HidGroup,
        entry: &DeviceEntry,
        product: &mut impl FnMut(usize, &CollectionSelector) -> Option<bool>,
    ) -> Match<(CollectionId, Option<CollectionId>)> {
        let Some(send) = entry.send.as_ref() else {
            return Match::Absent;
        };
        let command = match unique(group, send, |c| Some(command_descriptor(c)), product) {
            Ok(Some(command)) => command,
            Ok(None) => return Match::Absent,
            Err(()) => return Match::Denied,
        };
        let events = if let Some(receive) = &entry.receive {
            match unique(group, receive, event_descriptor, product) {
                Ok(Some(events)) => Some(events),
                Ok(None) => return Match::Absent,
                Err(()) => return Match::Denied,
            }
        } else {
            None
        };
        Match::Unique((command, events))
    }

    pub(super) fn from_selection(
        group: GroupId,
        command: &Collection,
        events: Option<&Collection>,
        entry: &DeviceEntry,
    ) -> Option<Self> {
        if entry.engine != EngineKind::Sonix
            || command.vid != entry.vid
            || !entry.pids.contains(&command.pid)
            || !command_descriptor(command)
            || !entry.send.as_ref()?.matches_descriptor(command)
        {
            return None;
        }
        let events = match (events, &entry.receive) {
            (Some(c), Some(selector))
                if c.vid == command.vid
                    && c.pid == command.pid
                    && event_descriptor(c) == Some(true)
                    && selector.matches_descriptor(c) =>
            {
                Some((c.id, usize::from(c.input_len)))
            }
            (None, None) => None,
            _ => return None,
        };
        Some(Self {
            group_key: DeviceKey(format!("sonix-group:{}", group.0)),
            key: DeviceKey(format!("sonix:{:04x}", command.pid)),
            command: command.id,
            events,
            linked: false,
            registered: false,
            grace: None,
            transaction: None,
            quiet_until: 0,
        })
    }

    pub(super) fn is_fallback_key(key: &DeviceKey) -> bool {
        key.0.starts_with("sonix:")
    }

    fn send(
        command: CollectionId,
        transaction: &Transaction,
        kind: ReportKind,
        bytes: &[u8],
    ) -> Action {
        Action::Send {
            grant: transaction.grant,
            collection: command,
            kind,
            bytes: bytes.to_vec(),
            deadline: Some(transaction.until),
        }
    }

    fn begin(&mut self, grant: GrantId, until: Ms, now: Ms) -> Vec<Action> {
        if self.transaction.is_some() || now < self.quiet_until || now >= until {
            return Vec::new();
        }
        let transaction = Transaction {
            grant,
            until,
            io: MailboxIo::Reading,
            query: None,
        };
        let get = Self::send(
            self.command,
            &transaction,
            ReportKind::GetFeature,
            &GET_MAILBOX,
        );
        self.transaction = Some(transaction);
        vec![get]
    }

    fn expire(&mut self, now: Ms) {
        if self.transaction.as_ref().is_some_and(|t| now >= t.until) {
            self.transaction = None;
        }
    }

    fn observe(
        &mut self,
        presence: Presence,
        level: Option<u8>,
        charge: Option<Charge>,
        source: Source,
    ) -> Action {
        let new = !self.registered;
        if !new && level.is_none() {
            return match presence {
                Presence::Asleep => Action::Asleep(self.key.clone()),
                _ => Action::Presence {
                    key: self.key.clone(),
                    presence,
                },
            };
        }
        let update = DeviceUpdate {
            key: self.key.clone(),
            name: None,
            kind: new.then_some(DeviceKind::Mouse),
            via: new.then_some(Via::Receiver),
            level: level.map(Level::Percent),
            millivolts: None,
            // A newly registered receiver cannot vouch for the previous owner's charge.
            charge: charge.or(new.then_some(Charge::Unknown)),
            presence: Some(presence),
            source,
        };
        if new {
            self.registered = true;
            Action::Register {
                key: self.key.clone(),
                caps: Capabilities {
                    pushes_battery: false, // 05 07 at wake/cable, not proven on each change.
                    query_wakes: false,
                    audio: false,
                    awake_signal: self.events.is_some(),
                    charging: false,
                    min_query_gap_ms: if self.events.is_some() {
                        MIN_QUERY_GAP_MS
                    } else {
                        0
                    },
                },
                initial: update,
                wpid: None,
            }
        } else {
            Action::Update(update)
        }
    }

    fn unlinked(&mut self, presence: Presence, source: Source) -> Vec<Action> {
        self.linked = false;
        self.grace = None;
        if let Some(cancelled) = self.transaction.take() {
            self.quiet_until = cancelled.until;
        }
        vec![self.observe(presence, None, None, source)]
    }

    fn input(&mut self, bytes: &[u8], now: Ms) -> Vec<Action> {
        match bytes {
            [0x05, 0x07, charge @ 0..=1, level @ 0..=100, 1, ..] => {
                // A valid push before dispatch makes the fallback unnecessary. Only the
                // actual link edge below can arm it, or admit later level-triggered reads.
                self.grace = None;
                let charge = if *charge == 1 {
                    Charge::Charging
                } else {
                    Charge::NotCharging
                };
                vec![self.observe(Presence::Online, Some(*level), Some(charge), Source::Push)]
            }
            [0x05, 0x08, 1, 1, ..] if !self.linked => {
                self.linked = true;
                self.grace = Some(WakeGrace {
                    at: now.saturating_add(EDGE_GRACE_MS),
                    grant: None,
                });
                vec![
                    self.observe(Presence::Online, None, None, Source::Push),
                    // Get authority on the edge itself; the timer can only spend it.
                    Action::Connected(self.key.clone()),
                ]
            }
            [0x05, 0x08, 2, 1, ..] => self.unlinked(Presence::Offline, Source::Push),
            [0x05, 0x0D, 1..=100, ..]
                if self.linked
                    && self.grace.is_none()
                    && self.transaction.is_none()
                    && now >= self.quiet_until =>
            {
                vec![Action::Awake(self.key.clone())]
            }
            _ => Vec::new(),
        }
    }

    fn mailbox(&mut self, bytes: &[u8], now: Ms) -> Vec<Action> {
        let [FEATURE_ID, status, category, register, sub, _, _, level, ..] = bytes else {
            return Vec::new();
        };
        if bytes.len() != FEATURE_LEN {
            return Vec::new();
        }
        let Some(mut transaction) = self.transaction.take() else {
            return Vec::new();
        };
        if transaction.io != MailboxIo::Reading {
            self.transaction = Some(transaction);
            return Vec::new();
        }
        let echo = [*category, *register, *sub];
        let matched = echo == BATTERY_ECHO;
        let check_at = now.saturating_add(CHECK_MS);
        transaction.io = MailboxIo::CheckAt(check_at);

        let mut send = false;
        match transaction.query.as_mut() {
            // The preflight may be looking at someone else's completed or pending request.
            // It never supplies a battery value or a link-down result.
            None if check_at < transaction.until
                && (matches!(*status, 0x01 | 0x05) || bytes == GET_MAILBOX) =>
            {
                transaction.query = Some(Query {
                    preflight: bytes.try_into().expect("validated mailbox width"),
                    matched_busy: false,
                    reset_since: None,
                    resent: false,
                });
                send = true;
            }
            None => {}
            Some(query) => {
                let stale_terminal = !query.matched_busy
                    && query.preflight[1] == *status
                    && query.preflight[2..5] == BATTERY_ECHO
                    && (*status == 0x05 || query.preflight[7] == *level);
                match (*status, matched) {
                    (0x05, true) if !stale_terminal => {
                        return self.unlinked(Presence::Asleep, Source::Query);
                    }
                    (0x01, true) if !stale_terminal => match level {
                        0..=100 => {
                            // Reachability is not an input edge and cannot arm the fallback.
                            self.linked = self.events.is_some();
                            return vec![self.observe(
                                Presence::Online,
                                Some(*level),
                                None,
                                Source::Query,
                            )];
                        }
                        0xFF => return self.unlinked(Presence::Offline, Source::Query),
                        _ => query.reset_since = None,
                    },
                    (0x02, _) => {
                        // Any busy report blocks a SET. Only ours proves this command ran.
                        query.matched_busy = matched;
                        query.reset_since = None;
                    }
                    (0x01, false)
                        if self.events.is_some()
                            && query.matched_busy
                            && !query.resent
                            && bytes == query.preflight =>
                    {
                        let since = *query.reset_since.get_or_insert(now);
                        if now.saturating_sub(since) >= RESET_MS
                            && now.saturating_add(RESEND_ROOM_MS) < transaction.until
                        {
                            query.resent = true;
                            query.matched_busy = false;
                            send = true;
                        }
                    }
                    _ => query.reset_since = None,
                }
            }
        }
        let action = send.then(|| {
            transaction.io = MailboxIo::Writing(FeatureSetId {
                grant: transaction.grant,
                issued_ms: now,
            });
            Self::send(
                self.command,
                &transaction,
                ReportKind::SetFeature,
                &READ_BATTERY,
            )
        });
        self.transaction = Some(transaction);
        action.into_iter().collect()
    }
}

fn command_descriptor(c: &Collection) -> bool {
    c.interface == Some(3)
        && (c.usage_page, c.usage) == (0xFFFF, 1)
        && usize::from(c.feature_len) == FEATURE_LEN
}

fn event_descriptor(c: &Collection) -> Option<bool> {
    if c.interface == Some(1) && c.usage_page >= 0xFF00 && c.input_len >= 5 {
        c.input_report_05
    } else {
        Some(false)
    }
}

fn unique(
    group: &HidGroup,
    selector: &CollectionSelector,
    descriptor: impl Fn(&Collection) -> Option<bool>,
    product: &mut impl FnMut(usize, &CollectionSelector) -> Option<bool>,
) -> Result<Option<CollectionId>, ()> {
    let mut selected = None;
    for (index, collection) in group.collections.iter().enumerate() {
        if !selector.matches_descriptor(collection) || !descriptor(collection).ok_or(())? {
            continue;
        }
        if selector.product.is_some() && !product(index, selector).ok_or(())? {
            continue;
        }
        if selected.replace(collection.id).is_some() {
            return Err(());
        }
    }
    Ok(selected)
}

impl Engine for SonixEngine {
    fn group_key(&self) -> DeviceKey {
        self.group_key.clone()
    }

    fn group_one_shots(&self) -> bool {
        // Feinmann has no verified wake event to clear a previous terminal 05.
        self.events.is_none()
    }

    fn on_grant(&mut self, grant: &Grant, now: Ms) -> Vec<Action> {
        self.expire(now);
        if grant.max_requests == 0 || now >= grant.expires_ms {
            return Vec::new();
        }
        let allowed = match grant.trigger {
            Trigger::Connected if grant.subject == self.key => {
                if let Some(grace) = &mut self.grace
                    && grace.grant.is_none()
                {
                    grace.grant = Some((grant.id, grant.expires_ms));
                }
                false
            }
            Trigger::Arrival => grant.subject == self.group_key,
            Trigger::Resume | Trigger::RefreshNow => {
                let subject = if self.group_one_shots() {
                    &self.group_key
                } else {
                    &self.key
                };
                grant.subject == *subject && self.grace.is_none()
            }
            Trigger::Awake => self.linked && self.grace.is_none() && grant.subject == self.key,
            _ => false,
        };
        if !allowed {
            return Vec::new();
        }
        self.begin(
            grant.id,
            grant.expires_ms.min(now.saturating_add(TRANSACTION_MS)),
            now,
        )
    }

    fn on_report(
        &mut self,
        collection: CollectionId,
        origin: ReportOrigin,
        bytes: &[u8],
        now: Ms,
    ) -> Vec<Action> {
        self.expire(now);
        match origin {
            ReportOrigin::Input
                if self
                    .events
                    .is_some_and(|(id, width)| id == collection && bytes.len() <= width) =>
            {
                self.input(bytes, now)
            }
            ReportOrigin::FeatureReply if collection == self.command => self.mailbox(bytes, now),
            _ => Vec::new(),
        }
    }

    fn on_feature_set_complete(
        &mut self,
        collection: CollectionId,
        id: FeatureSetId,
        now: Ms,
    ) -> Vec<Action> {
        self.expire(now);
        if collection == self.command
            && id.issued_ms <= now
            && let Some(transaction) = &mut self.transaction
            && transaction.io == MailboxIo::Writing(id)
        {
            transaction.io = MailboxIo::CheckAt(now.saturating_add(CHECK_MS));
        }
        Vec::new()
    }

    fn on_tick(&mut self, now: Ms) -> Vec<Action> {
        self.expire(now);
        if let Some(WakeGrace { at, grant }) = self
            .grace
            .take_if(|g| now >= g.at || g.grant.is_some_and(|(_, until)| now >= until))
            && now >= at
            && self.linked
            && self.transaction.is_none()
            && let Some((grant, expires)) = grant
        {
            // A late Tick cannot revive an old edge or give it another full second.
            let until = expires.min(at.saturating_add(TRANSACTION_MS));
            return self.begin(grant, until, now);
        }
        if let Some(transaction) = &mut self.transaction
            && matches!(transaction.io, MailboxIo::CheckAt(at) if now >= at)
        {
            transaction.io = MailboxIo::Reading;
            return vec![Self::send(
                self.command,
                transaction,
                ReportKind::GetFeature,
                &GET_MAILBOX,
            )];
        }
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Ms> {
        let grace = self
            .grace
            .as_ref()
            .map(|g| g.grant.map_or(g.at, |(_, expires)| g.at.min(expires)));
        let mailbox = self.transaction.as_ref().map(|t| match t.io {
            MailboxIo::CheckAt(at) => at.min(t.until),
            MailboxIo::Reading | MailboxIo::Writing(_) => t.until,
        });
        grace.into_iter().chain(mailbox).min()
    }

    fn allowed_writes(&self) -> &'static [WriteRule] {
        &RULES
    }

    fn collection_role(&self, collection: CollectionId) -> Option<CollectionRole> {
        (collection == self.command).then_some(COMMAND_ROLE)
    }
}
