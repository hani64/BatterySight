//! Rapoo VT battery status from interrupt input only.

use crate::catalog::{CollectionSelector, DeviceEntry, EngineKind, RapooConfig, RapooReport};
use crate::engine::{Action, Engine};
use crate::hid::{Collection, CollectionId, GroupId, HidGroup, ReportOrigin};
use crate::model::{Charge, DeviceKey, DeviceKind, DeviceUpdate, Level, Presence, Source, Via};
use crate::scheduler::{Capabilities, Grant};
use crate::time::Ms;

use super::Match;

const SILENCE_MS: Ms = 60_000;
const CAPS: Capabilities = Capabilities {
    pushes_battery: true,
    query_wakes: false,
    audio: false,
    awake_signal: false,
    charging: false,
    min_query_gap_ms: 0,
};

pub struct RapooEngine {
    group: GroupId,
    rx: CollectionId,
    input_len: usize,
    pid: u16,
    config: RapooConfig,
    key: DeviceKey,
    registered: bool,
    expires: Option<Ms>,
}

impl RapooEngine {
    /// Selects only the matched entry's unique receive collection; never assigns a command role.
    pub fn new(group: &HidGroup, matched: &DeviceEntry) -> Option<Self> {
        super::usb_ids(group)?;
        let Match::Unique(rx) = Self::match_receive(group, matched, |index, selector| {
            let name = &group.collections[index].product;
            (!name.is_empty()).then(|| selector.matches_product(name))
        }) else {
            return None;
        };
        Self::from_selection(group.id, rx, matched)
    }

    pub(super) fn match_receive<'a>(
        group: &'a HidGroup,
        matched: &DeviceEntry,
        mut product: impl FnMut(usize, &CollectionSelector) -> Option<bool>,
    ) -> Match<&'a Collection> {
        let Some((config, selector)) = Self::receive_role(matched) else {
            return Match::Absent;
        };
        let mut selected = None;
        for (index, rx) in group.collections.iter().enumerate() {
            if !Self::matches_descriptor(rx, matched, config, selector) {
                continue;
            }
            if selector.product.is_some() {
                match product(index, selector) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return Match::Denied,
                }
            }
            if selected.replace(rx).is_some() {
                return Match::Denied;
            }
        }
        selected.map_or(Match::Absent, Match::Unique)
    }

    pub(super) fn from_selection(
        group: GroupId,
        rx: &Collection,
        entry: &DeviceEntry,
    ) -> Option<Self> {
        // The runtime retained the preselected collection. Recheck its descriptor, not its
        // product string: only pre-open cached metadata can satisfy a catalog product gate.
        let (config, selector) = Self::receive_role(entry)?;
        if !Self::matches_descriptor(rx, entry, config, selector) {
            return None;
        }
        Some(Self {
            group,
            rx: rx.id,
            input_len: usize::from(rx.input_len),
            pid: rx.pid,
            config,
            key: DeviceKey(format!("rapoo:{:04x}", rx.pid)),
            registered: false,
            expires: None,
        })
    }

    fn receive_role(entry: &DeviceEntry) -> Option<(RapooConfig, &CollectionSelector)> {
        let selector = entry.receive.as_ref()?;
        if entry.engine != EngineKind::Rapoo
            || entry.vid != 0x24AE
            || entry.send.is_some()
            || selector.usage_page.is_none()
            || selector.usage.is_none()
        {
            return None;
        }
        Some((entry.rapoo?, selector))
    }

    fn matches_descriptor(
        rx: &Collection,
        entry: &DeviceEntry,
        config: RapooConfig,
        selector: &CollectionSelector,
    ) -> bool {
        rx.vid == entry.vid
            && entry.pids.contains(&rx.pid)
            && selector.matches_descriptor(rx)
            && match config.report {
                RapooReport::Nordic | RapooReport::Ch584 => rx.input_len >= 9,
                RapooReport::Legacy | RapooReport::LegacyWithUnknownLevel => rx.input_len == 7,
            }
    }

    pub(super) fn is_fallback_key(key: &DeviceKey) -> bool {
        key.0.starts_with("rapoo:")
    }

    /// `None` rejects a frame; `Some(None)` is a documented unavailable percentage.
    fn decode(&self, bytes: &[u8]) -> Option<Option<Level>> {
        if bytes.len() != self.input_len {
            return None;
        }
        let percent = match self.config.report {
            RapooReport::Nordic | RapooReport::Ch584 => {
                let report_id = match self.config.report {
                    RapooReport::Nordic => 0x07,
                    _ => 0x08,
                };
                let &[id, link, _, _, _, _, _, 1 | 2, percent, ..] = bytes else {
                    return None;
                };
                if id != report_id || matches!(link, 0xB0..=0xB3) || !matches!(link & 0x0F, 0 | 2) {
                    return None;
                }
                if let Some(&[lo, hi, ..]) = bytes.get(13..) {
                    let pid = u16::from_le_bytes([lo, hi]);
                    // The optional field is a model PID, not a unit identifier; zero may be
                    // absent data on a padded Windows HID read.
                    if pid != 0 && pid != self.pid && Some(pid) != self.config.wired_pid {
                        return None;
                    }
                }
                percent
            }
            RapooReport::Legacy | RapooReport::LegacyWithUnknownLevel => {
                let &[0xBB, 0xB0, _, _, _, state @ (1 | 2), percent] = bytes else {
                    return None;
                };
                if self.config.report == RapooReport::LegacyWithUnknownLevel
                    && state == 2
                    && percent == 0x7F
                {
                    return Some(None);
                }
                percent
            }
        };
        (percent <= 100).then_some(Some(Level::Percent(percent)))
    }
}

impl Engine for RapooEngine {
    fn group_key(&self) -> DeviceKey {
        DeviceKey(format!("rapoo-group:{}", self.group.0))
    }

    fn on_grant(&mut self, _: &Grant, _: Ms) -> Vec<Action> {
        // Refresh observes the existing reader just like arrival and resume. There is no
        // refresh deadline to schedule: the end of its observation window changes nothing.
        Vec::new()
    }

    fn on_report(
        &mut self,
        collection: CollectionId,
        origin: ReportOrigin,
        bytes: &[u8],
        now: Ms,
    ) -> Vec<Action> {
        if collection != self.rx || origin != ReportOrigin::Input {
            return Vec::new();
        }
        let Some(level) = self.decode(bytes) else {
            return Vec::new();
        };
        self.expires = Some(now.saturating_add(SILENCE_MS));
        if self.registered && level.is_none() {
            // This heartbeat proves presence but does not refresh the last measurement.
            return vec![Action::Awake(self.key.clone())];
        }
        let initial = DeviceUpdate {
            key: self.key.clone(),
            name: None,
            kind: Some(DeviceKind::Mouse),
            via: Some(if Some(self.pid) == self.config.wired_pid {
                Via::Usb
            } else {
                Via::Receiver
            }),
            level,
            millivolts: None,
            // The sources disagree on whether the status byte encodes charging at all.
            charge: Some(Charge::Unknown),
            presence: Some(Presence::Online),
            source: Source::Push,
        };
        if self.registered {
            vec![Action::Update(initial)]
        } else {
            self.registered = true;
            vec![Action::Register {
                key: self.key.clone(),
                caps: CAPS,
                initial,
                wpid: None,
            }]
        }
    }

    fn on_tick(&mut self, now: Ms) -> Vec<Action> {
        if self.expires.is_some_and(|at| now >= at) {
            self.expires = None;
            return vec![Action::Asleep(self.key.clone())];
        }
        Vec::new()
    }

    fn next_deadline(&self) -> Option<Ms> {
        self.expires
    }
}
