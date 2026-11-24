//! Built-in protocol engines.

pub mod hidpp;

use crate::catalog::{Catalog, CollectionSelector, DeviceEntry, EngineKind};
use crate::hid::{Collection, CollectionId, CollectionRole, GroupId, HidGroup};
use crate::model::DeviceKey;

use hidpp::HidppEngine;

/// A resolved protocol and its collection roles. An open failure never reselects an entry
/// or promotes another collection to the command role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    group: GroupId,
    kind: EngineKind,
    direct: Option<usize>,
    receive: Vec<CollectionId>,
    command: CollectionId,
    report_id: u8,
}

impl Selection {
    pub fn reads(&self, id: CollectionId) -> bool {
        self.receive.contains(&id)
    }

    pub fn opens(&self, id: CollectionId) -> bool {
        self.sends(id) || self.reads(id)
    }

    pub fn sends(&self, id: CollectionId) -> bool {
        id == self.command
    }

    pub fn kind(&self) -> EngineKind {
        self.kind
    }

    /// The selected entry's stable index within the hub's immutable catalog, if direct.
    pub fn direct(&self) -> Option<usize> {
        self.direct
    }

    /// The report ID used to frame commands for this selection.
    pub fn command_report_id(&self) -> u8 {
        self.report_id
    }

    pub(crate) fn send_role(&self, id: CollectionId) -> Option<CollectionRole> {
        if !self.sends(id) {
            return None;
        }
        Some(match self.kind {
            EngineKind::Hidpp => hidpp::engine::COMMAND_ROLE,
        })
    }

    pub fn ready(&self, group: &HidGroup) -> bool {
        group.id == self.group
            && group.collections.iter().any(|c| c.id == self.command)
            && group.collections.iter().any(|c| self.reads(c.id))
    }

    pub(crate) fn hidpp(&self, group: &HidGroup, catalog: &Catalog) -> Option<HidppEngine> {
        if !self.ready(group) {
            return None;
        }
        let out = group.collections.iter().find(|c| c.id == self.command)?;
        let quirks = self
            .direct
            .and_then(|index| catalog.devices().get(index))
            .map(|d| &d.quirks);
        Some(HidppEngine::from_selection(
            group,
            (self.report_id, out),
            &self.receive,
            quirks,
        ))
    }
}

/// Whether a catalog admits the group with the product metadata already on its collections.
pub fn wants(group: &HidGroup, catalog: &Catalog) -> bool {
    select(group, catalog).is_some()
}

/// Whether the selected engine receives input from this collection of the group.
pub fn reads(group: &HidGroup, collection: &Collection, catalog: &Catalog) -> bool {
    group.collections.contains(collection)
        && select(group, catalog).is_some_and(|matched| matched.reads(collection.id))
}

pub(crate) fn select(group: &HidGroup, catalog: &Catalog) -> Option<Selection> {
    select_with(group, catalog, |index, selector| {
        let name = &group.collections[index].product;
        (!name.is_empty()).then(|| selector.matches_product(name))
    })
}

/// Resolve descriptors before asking for cached product metadata by collection index.
/// The callback returns `None` if the name is unavailable, not a product mismatch.
pub(crate) fn select_with(
    group: &HidGroup,
    catalog: &Catalog,
    mut product: impl FnMut(usize, &CollectionSelector) -> Option<bool>,
) -> Option<Selection> {
    let first = group.collections.first()?;
    let (vid, pid) = (first.vid, first.pid);
    if group.collections.iter().any(|c| (c.vid, c.pid) != (vid, pid)) {
        return None;
    }

    let mut direct = catalog.direct_entries(vid, pid).peekable();
    if direct.peek().is_some() {
        let mut selected = None;
        for (index, entry) in direct {
            match match_roles(group, entry.engine, Some((index, entry)), &mut product) {
                Match::Absent => {}
                Match::Denied => return None,
                Match::Unique(candidate) => {
                    if selected.is_some() {
                        return None;
                    }
                    selected = Some(candidate);
                }
            }
        }
        // A catalogued USB ID must pass its own gates, not the broad family default.
        return selected;
    }

    let family = catalog.family(vid, EngineKind::Hidpp)?;
    match match_roles(group, family.engine, None, &mut product) {
        Match::Unique(selection) => Some(selection),
        Match::Absent | Match::Denied => None,
    }
}

enum Match {
    Absent,
    /// A plausible entry whose identity or explicit collection role is not unique.
    Denied,
    Unique(Selection),
}

fn match_roles(
    group: &HidGroup,
    kind: EngineKind,
    direct: Option<(usize, &DeviceEntry)>,
    product: &mut impl FnMut(usize, &CollectionSelector) -> Option<bool>,
) -> Match {
    let receive = direct.and_then(|(_, entry)| entry.receive.as_ref());
    let send = direct.and_then(|(_, entry)| entry.send.as_ref());
    let eligible = |c: &Collection, selector: Option<&CollectionSelector>| {
        selector.is_none_or(|selector| selector.matches_descriptor(c))
    };
    let report = |c: &Collection| match kind {
        EngineKind::Hidpp => HidppEngine::command_report(c),
    };
    let rx = |c: &Collection| {
        // Explicit receive roles may be on an input-only interface. The ungated Logitech
        // family keeps its original collection recognition for both widths.
        let compatible = match kind {
            EngineKind::Hidpp if receive.is_some() => HidppEngine::receives(c),
            EngineKind::Hidpp => report(c).is_some(),
        };
        compatible && eligible(c, receive)
    };
    let tx = |c: &Collection| report(c).filter(|_| eligible(c, send));
    // If either role is missing by descriptor alone, no product lookup can admit the group.
    if !group.collections.iter().any(rx) || !group.collections.iter().any(|c| tx(c).is_some()) {
        return Match::Absent;
    }
    let mut product_ok = |index, selector: Option<&CollectionSelector>| match selector {
        Some(selector) if selector.product.is_some() => product(index, selector),
        _ => Some(true),
    };

    let mut receiving = Vec::new();
    for (index, c) in group.collections.iter().enumerate().filter(|(_, c)| rx(c)) {
        match product_ok(index, receive) {
            Some(true) => receiving.push(c.id),
            Some(false) => {}
            None => return Match::Denied,
        }
    }
    if receiving.is_empty() {
        return Match::Absent;
    }

    // The HID++ short report wins over any number of long reports, but ties at the
    // preferred width are denied. A selector can restrict the candidates, not the rule.
    let mut command = None;
    let mut tied = false;
    for (index, c) in group.collections.iter().enumerate() {
        let Some(report_id) = tx(c) else { continue };
        match product_ok(index, send) {
            Some(true) => {}
            Some(false) => continue,
            None => return Match::Denied,
        }
        match command {
            Some((_, current)) if current == report_id => tied = true,
            Some((_, current)) if current < report_id => {}
            _ => {
                command = Some((c.id, report_id));
                tied = false;
            }
        }
    }
    let Some((command, report_id)) = command else {
        return Match::Absent;
    };
    if tied || (receive.is_some() && receiving.len() > 1) {
        return Match::Denied;
    }
    Match::Unique(Selection {
        group: group.id,
        kind,
        direct: direct.map(|(index, _)| index),
        receive: receiving,
        command,
        report_id,
    })
}

/// Whether two devices present at once can be registered under the same `key`: a key an
/// engine builds without a serial or unit id. The hub tells such devices apart.
pub fn shared_key(key: &DeviceKey) -> bool {
    HidppEngine::is_fallback_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{Hub, HubInput, HubOutput};
    use crate::scheduler::Policy;

    fn group(vid: u16, usage_page: u16, usage: u16, len: u16) -> HidGroup {
        HidGroup {
            id: GroupId(1),
            collections: vec![Collection {
                id: CollectionId(1),
                vid,
                pid: 0xC547,
                usage_page,
                usage,
                interface: None,
                input_len: len,
                output_len: len,
                feature_len: 0,
                product: String::new(),
            }],
        }
    }

    const DIRECT: &str = r#"
[[device]]
name = "Synthetic gated device"
vid = 0x046D
pids = [0xBEEF]
kind = "headset"
engine = "hidpp"
status = "hardware-tested"
source = "synthetic descriptor fixture"
receive = { usage_page = 0xFF00, usage = 2, interface = 3, input_len = 20, output_len = 20, feature_len = 0 }
send = { usage_page = 0xFF00, usage = 1, interface = 4, output_len = 7, product = "FiXtUrE" }
"#;

    const FAMILY: &str = r#"
[[family]]
vid = 0x046D
engine = "hidpp"
status = "source-verified"
source = "Linux hid-logitech-hidpp"
"#;

    #[test]
    fn overlapping_direct_entries_do_not_fall_through_to_family() {
        let mut g = group(0x046D, 0xFF00, 0x0002, 20);
        g.collections[0].pid = 0xBEEF;
        let ungated: String = DIRECT
            .lines()
            .filter(|line| !line.starts_with("receive") && !line.starts_with("send"))
            .collect::<Vec<_>>()
            .join("\n");
        let conflicting = ungated.replace("Synthetic gated device", "Conflicting device");
        let catalog = Catalog::parse(&format!("{FAMILY}{ungated}\n{conflicting}")).unwrap();
        assert!(!wants(&g, &catalog));
        assert!(
            Hub::new(catalog, Policy::default())
                .handle(HubInput::GroupArrived(g), 0)
                .is_empty()
        );
    }

    fn gated_group() -> HidGroup {
        let mut g = group(0x046D, 0xFF00, 0x0002, 20);
        g.collections[0].pid = 0xBEEF;
        g.collections[0].interface = Some(3);
        let mut tx = group(0x046D, 0xFF00, 0x0001, 7).collections.remove(0);
        tx.id = CollectionId(2);
        tx.pid = 0xBEEF;
        tx.interface = Some(4);
        g.collections.push(tx);
        g
    }

    #[test]
    fn exact_direct_match_requires_both_collection_roles() {
        let catalog = Catalog::parse(&format!("{FAMILY}{DIRECT}")).unwrap();
        let mut g = gated_group();
        assert!(!wants(&g, &catalog), "missing product name");
        g.collections[1].product = "The fixture USB device".into();
        assert!(wants(&g, &catalog));
        assert!(reads(&g, &g.collections[0], &catalog));
        assert!(!reads(&g, &g.collections[1], &catalog), "send-only collection");
        for (description, alter) in [
            (
                "usage page",
                (|c: &mut Collection| c.usage_page = 0x000C) as fn(&mut Collection),
            ),
            ("usage", |c: &mut Collection| c.usage = 1),
            ("interface", |c: &mut Collection| c.interface = Some(4)),
            ("missing interface", |c: &mut Collection| c.interface = None),
            ("input length", |c: &mut Collection| c.input_len = 7),
            ("output length", |c: &mut Collection| c.output_len = 7),
            ("feature length", |c: &mut Collection| c.feature_len = 1),
        ] {
            let mut wrong = g.clone();
            alter(&mut wrong.collections[0]);
            assert!(!wants(&wrong, &catalog), "{description}");
        }
        g.collections[1].product = "Different model".into();
        assert!(
            !wants(&g, &catalog),
            "wrong product does not fall back to family"
        );
    }

    #[test]
    fn equally_eligible_explicit_receive_collections_deny_selection() {
        let catalog = Catalog::parse(&format!("{FAMILY}{DIRECT}")).unwrap();
        let mut g = gated_group();
        g.collections[1].product = "Fixture USB device".into();
        assert!(wants(&g, &catalog));
        let mut second_rx = g.collections[0].clone();
        second_rx.id = CollectionId(3);
        g.collections.push(second_rx);
        assert!(!wants(&g, &catalog), "an explicit RX role must be unique");
    }

    #[test]
    fn cached_product_is_requested_only_for_a_descriptor_candidate() {
        let catalog = Catalog::parse(&format!("{FAMILY}{DIRECT}")).unwrap();
        let mut g = gated_group();
        g.collections[0].interface = None;
        assert!(
            select_with(&g, &catalog, |_, _| panic!("no eligible receive collection")).is_none()
        );
        g.collections[0].interface = Some(3);
        let selection = select_with(&g, &catalog, |index, selector| {
            assert_eq!(index, 1, "only the command collection needs a name");
            Some(selector.matches_product("Fixture from PnP"))
        });
        assert!(selection.is_some());
    }

    #[test]
    fn preselected_command_is_not_replaced_or_regated_after_open() {
        let catalog = Catalog::parse(&format!("{FAMILY}{DIRECT}")).unwrap();
        let mut hub = Hub::new(catalog, Policy::default());
        let mut g = gated_group();
        let selection = hub
            .select_group(&g, |_, selector| {
                Some(selector.matches_product("Fixture from PnP"))
            })
            .unwrap();
        g.collections[1].product = "Legacy USB Receiver".into();
        let command = g.collections[1].id;
        let out = hub.handle(HubInput::MatchedGroupArrived(g.clone(), selection.clone()), 0);
        assert_eq!(
            out,
            [HubOutput::Send {
                collection: command,
                kind: crate::hid::ReportKind::Output,
                bytes: vec![0x10, 0xFF, 0x81, 0, 0, 0, 0],
            }]
        );
        // Only the receive interface opened; it could send under the family default,
        // but the command role selected during admission must not silently switch.
        g.collections.pop();
        assert!(
            hub.handle(HubInput::MatchedGroupArrived(g, selection), 0)
                .is_empty()
        );
    }

    #[test]
    fn wants_logitech_hidpp_without_product() {
        let catalog = Catalog::parse(FAMILY).unwrap();
        assert!(wants(&group(0x046D, 0xFF00, 0x0001, 7), &catalog));
        assert!(wants(&group(0x046D, 0xFF43, 0x0202, 20), &catalog));
    }

    #[test]
    fn does_not_want_other_groups() {
        let catalog = Catalog::parse(FAMILY).unwrap();
        assert!(!wants(&group(0x1234, 0xFF00, 0x0001, 7), &catalog));
        assert!(!wants(&group(0x046D, 0x0001, 0x0002, 7), &catalog));
    }

    #[test]
    fn reads_only_hidpp_collections() {
        let catalog = Catalog::parse(FAMILY).unwrap();
        let mut g = group(0x046D, 0xFF00, 0x0001, 7);
        for (index, (page, usage, len, expected)) in [
            (0xFF00, 0x0002, 20, true),
            (0xFF43, 0x0202, 20, true),
            (0x000C, 0x0001, 3, false),
            (0x0001, 0x0005, 20, false),
            (0xFF00, 0x0001, 20, false),
        ]
        .into_iter()
        .enumerate()
        {
            let mut collection = group(0x046D, page, usage, len).collections.remove(0);
            collection.id = CollectionId(index as u32 + 2);
            g.collections.push(collection);
            assert_eq!(reads(&g, g.collections.last().unwrap(), &catalog), expected);
        }
        assert!(reads(&g, &g.collections[0], &catalog));
    }
}
