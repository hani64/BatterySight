//! Cached Bluetooth devnodes only. Instance IDs (which can contain radio addresses)
//! are used in-place for lookup and never kept in observations or diagnostics.

use std::collections::{BTreeMap, BTreeSet};

use batterysight_core::hub::{Hub, HubInput};
use batterysight_core::model::{
    DeviceKey, DeviceKind, DeviceUpdate, Level, Presence, Source as SampleSource,
    SystemObservation, Verification, Via,
};
use batterysight_core::time::Ms;
use windows::Win32::Devices::Bluetooth::{
    COD_AUDIO_MINOR_HANDS_FREE, COD_AUDIO_MINOR_HEADPHONES, COD_AUDIO_MINOR_HEADSET,
    COD_AUDIO_MINOR_HEADSET_HANDS_FREE, COD_AUDIO_MINOR_LOUDSPEAKER, COD_MAJOR_AUDIO,
    COD_MAJOR_MASK, COD_MAJOR_PERIPHERAL, COD_MINOR_BIT_OFFSET, COD_MINOR_MASK,
    COD_PERIPHERAL_MINOR_GAMEPAD, COD_PERIPHERAL_MINOR_JOYSTICK,
    COD_PERIPHERAL_MINOR_KEYBOARD_MASK, COD_PERIPHERAL_MINOR_POINTER_MASK,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GETIDLIST_FILTER_NONE, CM_Get_DevNode_PropertyW, CM_Get_Device_ID_List_SizeW,
    CM_Get_Device_ID_ListW, CM_LOCATE_DEVNODE_FLAGS, CM_LOCATE_DEVNODE_NORMAL,
    CM_LOCATE_DEVNODE_PHANTOM, CM_Locate_DevNodeW, CR_BUFFER_SMALL, CR_NO_SUCH_DEVNODE,
    CR_NO_SUCH_VALUE, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_FriendlyName, DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_BYTE, DEVPROP_TYPE_STRING,
    DEVPROP_TYPE_UINT32, DEVPROPTYPE,
};
use windows::Win32::Foundation::{DEVPROPKEY, ERROR_INSUFFICIENT_BUFFER};
use windows::core::{GUID, PCWSTR, Result};

use crate::hid_enum::{ContainerId, cm_result, node_container};

// Undocumented cache key: confirmed on HFP service nodes and BLE peer nodes.
const BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5),
    pid: 2,
};
const IS_CONNECTED: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29),
    pid: 15,
};
// DEVPKEY_Bluetooth_ClassOfDevice, published in the Windows Driver Kit.
const CLASS_OF_DEVICE: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x2BD67D8B_8BEB_48D5_87E0_6CDA3428040A),
    pid: 10,
};

const CONNECT_FOLLOW_UP_MS: Ms = 4_000;
const CONNECTED_REREAD_MS: Ms = 60_000;

/// One Bluetooth container, with presence independent of its cached battery value.
pub struct Observation {
    pub container: ContainerId,
    pub name: String,
    pub kind: DeviceKind,
    pub connected: bool,
    /// May still be populated after disconnect. Do not publish it as a new reading then.
    pub level: Option<u8>,
}

/// The identifier-free projection for diagnostics. A present property need not be valid.
pub struct Metadata {
    pub name: String,
    pub has_battery_property: bool,
}

/// Enumerates local Windows state. An error is not evidence that any device disconnected.
pub fn scan() -> Result<Vec<Observation>> {
    Ok(observations(read_groups(false)?))
}

/// Reads names and checks for property existence without getting its value or opening devices.
pub fn scan_metadata() -> Result<Vec<Metadata>> {
    Ok(read_groups(true)?
        .into_values()
        .map(|group| Metadata {
            has_battery_property: group.has_property,
            name: group.name(),
        })
        .collect())
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum NodeRole {
    #[default]
    Other,
    ClassicPeer,
    LePeer,
    HandsFree,
    Hogp,
}

impl NodeRole {
    fn peer(self) -> bool {
        matches!(self, Self::ClassicPeer | Self::LePeer)
    }
}

#[derive(Default)]
struct Node {
    container: ContainerId,
    role: NodeRole,
    name: Option<String>,
    kind: Option<DeviceKind>,
    connected: bool,
    level: Option<u8>,
    has_property: bool,
}

#[derive(Default)]
struct Group {
    name: Option<(bool, String)>,
    class: Option<DeviceKind>,
    classic: bool,
    le: bool,
    hogp: Option<bool>,
    hands_free: bool,
    hfp_level: Option<u8>,
    le_level: Option<u8>,
    other_level: Option<u8>,
    has_property: bool,
}

impl Group {
    fn name(self) -> String {
        self.name
            .map_or_else(|| "Bluetooth device".into(), |(_, name)| name)
    }

    fn add(&mut self, node: Node) {
        if let Some(name) = node.name {
            let peer = node.role.peer();
            if self.name.as_ref().is_none_or(|(current_peer, current)| {
                (peer && !current_peer) || (peer == *current_peer && name < *current)
            }) {
                self.name = Some((peer, name));
            }
        }
        self.class = self.class.or(node.kind);
        self.has_property |= node.has_property;
        let level = match node.role {
            NodeRole::ClassicPeer => {
                self.classic |= node.connected;
                &mut self.other_level
            }
            NodeRole::LePeer => {
                self.le |= node.connected;
                &mut self.le_level
            }
            NodeRole::HandsFree => {
                self.hands_free = true;
                &mut self.hfp_level
            }
            NodeRole::Hogp => {
                self.hogp = Some(self.hogp.unwrap_or(false) || node.connected);
                &mut self.other_level
            }
            NodeRole::Other => &mut self.other_level,
        };
        if let Some(value) = node.level {
            // Conflicting cached nodes of the same rank have no timestamps to break a tie.
            *level = Some(level.map_or(value, |previous| previous.min(value)));
        }
    }

    fn observation(self, container: ContainerId) -> Observation {
        // For installed BLE HID devices the HOGP node, not the persistent peer node,
        // indicates presence. For Classic use the peer, not the HFP profile's own flag.
        let le_connected = self.hogp.unwrap_or(self.le);
        let level = if le_connected && !self.classic {
            self.le_level.or(self.hfp_level)
        } else {
            self.hfp_level.or(self.le_level)
        };
        Observation {
            container,
            connected: self.classic || le_connected,
            level: level.or(self.other_level),
            kind: self.class.unwrap_or(if self.hands_free {
                DeviceKind::Headset
            } else {
                DeviceKind::Other
            }),
            name: self.name(),
        }
    }
}

fn add_node(groups: &mut BTreeMap<ContainerId, Group>, node: Node) {
    groups.entry(node.container).or_default().add(node);
}

fn observations(groups: BTreeMap<ContainerId, Group>) -> Vec<Observation> {
    groups
        .into_iter()
        .map(|(id, group)| group.observation(id))
        .collect()
}

fn starts_with(id: &[u16], prefix: &[u8]) -> bool {
    id.get(..prefix.len()).is_some_and(|start| {
        start.iter().zip(prefix).all(|(&unit, byte)| {
            u8::try_from(unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(byte))
        })
    })
}

fn node_role(id: &[u16]) -> Option<NodeRole> {
    if starts_with(id, br"BTHENUM\DEV_") {
        Some(NodeRole::ClassicPeer)
    } else if starts_with(id, br"BTHLE\DEV_") {
        Some(NodeRole::LePeer)
    } else if starts_with(id, br"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}") {
        Some(NodeRole::HandsFree)
    } else if starts_with(id, br"BTHLEDEVICE\{00001812-0000-1000-8000-00805F9B34FB}") {
        Some(NodeRole::Hogp)
    } else if [br"BTHENUM\".as_slice(), br"BTHLE\", br"BTHLEDEVICE\"]
        .into_iter()
        .any(|prefix| starts_with(id, prefix))
    {
        Some(NodeRole::Other)
    } else {
        None
    }
}

fn instance_ids() -> Result<Vec<u16>> {
    // A non-filtered list also succeeds on systems that have no Bluetooth enumerators.
    // Do not use the service filter; by default it can create devnodes.
    for _ in 0..4 {
        let mut len = 0;
        // SAFETY: `len` is writable and the unfiltered call takes no input string.
        cm_result(unsafe {
            CM_Get_Device_ID_List_SizeW(&mut len, PCWSTR::null(), CM_GETIDLIST_FILTER_NONE)
        })?;
        let mut ids = vec![0; len as usize];
        // SAFETY: the slice bounds specify the writable buffer in UTF-16 units.
        let result =
            unsafe { CM_Get_Device_ID_ListW(PCWSTR::null(), &mut ids, CM_GETIDLIST_FILTER_NONE) };
        if result != CR_BUFFER_SMALL {
            cm_result(result)?;
            return Ok(ids);
        }
    }
    Err(ERROR_INSUFFICIENT_BUFFER.to_hresult().into())
}

fn read_groups(metadata_only: bool) -> Result<BTreeMap<ContainerId, Group>> {
    let ids = instance_ids()?;
    let mut groups = BTreeMap::new();
    for id in ids
        .split_inclusive(|&unit| unit == 0)
        .take_while(|id| id.len() > 1)
    {
        let Some(role) = node_role(id) else { continue };
        // PHANTOM includes installed but absent nodes, for cached metadata and to recognize
        // an absent HOGP service. It does not request a device start or re-enumeration.
        let Some(node) = locate(id, CM_LOCATE_DEVNODE_PHANTOM)? else {
            continue;
        };
        let Some(container) = node_container(node)? else {
            continue;
        };
        let name = friendly_name(node)?;
        let mut entry = Node {
            container,
            role,
            name,
            ..Node::default()
        };
        if metadata_only {
            entry.has_property = property(node, &BATTERY, &mut [])?.is_some();
        } else {
            let mut value = [0];
            entry.level = property(node, &BATTERY, &mut value)?
                .and_then(|(kind, size)| battery_level(kind, size, value[0]));
            if role.peer() {
                entry.connected = property(node, &IS_CONNECTED, &mut value)?
                    == Some((DEVPROP_TYPE_BOOLEAN, 1))
                    && value[0] == 0xff;
                let mut class = [0; 4];
                if property(node, &CLASS_OF_DEVICE, &mut class)? == Some((DEVPROP_TYPE_UINT32, 4)) {
                    entry.kind = device_kind(u32::from_le_bytes(class));
                }
            } else if role == NodeRole::Hogp {
                entry.connected = locate(id, CM_LOCATE_DEVNODE_NORMAL)?.is_some();
            }
        }
        add_node(&mut groups, entry);
    }
    Ok(groups)
}

fn locate(id: &[u16], flags: CM_LOCATE_DEVNODE_FLAGS) -> Result<Option<u32>> {
    if id.last() != Some(&0) {
        return Ok(None);
    }
    let mut node = 0;
    // SAFETY: `id` is NUL-terminated and `node` is writable.
    let status = unsafe { CM_Locate_DevNodeW(&mut node, PCWSTR(id.as_ptr()), flags) };
    if status == CR_NO_SUCH_DEVNODE {
        return Ok(None);
    }
    cm_result(status)?;
    Ok(Some(node))
}

fn property(node: u32, key: &DEVPROPKEY, data: &mut [u8]) -> Result<Option<(DEVPROPTYPE, u32)>> {
    let mut kind = DEVPROPTYPE::default();
    let mut size = data.len() as u32;
    let buffer = (!data.is_empty()).then_some(data.as_mut_ptr());
    // SAFETY: `buffer` points to `size` writable bytes (or is absent for a size query).
    let status = unsafe { CM_Get_DevNode_PropertyW(node, key, &mut kind, buffer, &mut size, 0) };
    match status {
        CR_NO_SUCH_VALUE => Ok(None),
        // A size query uses this result as proof of presence. Never decode unread bytes.
        CR_BUFFER_SMALL => Ok(Some((DEVPROPTYPE::default(), size))),
        CR_SUCCESS => Ok(Some((kind, size))),
        _ => cm_result(status).map(|()| None),
    }
}

fn battery_level(kind: DEVPROPTYPE, size: u32, value: u8) -> Option<u8> {
    (kind == DEVPROP_TYPE_BYTE && size == 1 && value <= 100).then_some(value)
}

fn friendly_name(node: u32) -> Result<Option<String>> {
    let mut data = [0; 512];
    let Some((DEVPROP_TYPE_STRING, size)) =
        property(node, &DEVPKEY_Device_FriendlyName, &mut data)?
    else {
        return Ok(None);
    };
    let Some(data) = data
        .get(..size as usize)
        .filter(|data| data.len() >= 2 && data.len() % 2 == 0)
    else {
        return Ok(None);
    };
    let name: String = char::decode_utf16(
        data.as_chunks::<2>()
            .0
            .iter()
            .map(|&u| u16::from_le_bytes(u))
            .take_while(|&u| u != 0),
    )
    .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
    .collect();
    // Windows or a device may use an address itself as a fallback name.
    let trimmed = name.trim();
    let mut digits = trimmed.bytes().filter(|&b| b != b':' && b != b'-');
    let address = digits.clone().count() == 12 && digits.all(|b| b.is_ascii_hexdigit());
    Ok((!trimmed.is_empty() && !address).then_some(name))
}

fn device_kind(class: u32) -> Option<DeviceKind> {
    let minor = (class & COD_MINOR_MASK) >> COD_MINOR_BIT_OFFSET;
    match (class & COD_MAJOR_MASK) >> 8 {
        COD_MAJOR_AUDIO => match minor {
            COD_AUDIO_MINOR_HEADSET
            | COD_AUDIO_MINOR_HANDS_FREE
            | COD_AUDIO_MINOR_HEADSET_HANDS_FREE
            | COD_AUDIO_MINOR_HEADPHONES => Some(DeviceKind::Headset),
            COD_AUDIO_MINOR_LOUDSPEAKER => Some(DeviceKind::Speaker),
            _ => Some(DeviceKind::Other),
        },
        COD_MAJOR_PERIPHERAL if minor & COD_PERIPHERAL_MINOR_KEYBOARD_MASK != 0 => {
            Some(DeviceKind::Keyboard)
        }
        COD_MAJOR_PERIPHERAL if minor & COD_PERIPHERAL_MINOR_POINTER_MASK != 0 => {
            Some(DeviceKind::Mouse)
        }
        COD_MAJOR_PERIPHERAL
            if matches!(
                minor,
                COD_PERIPHERAL_MINOR_JOYSTICK | COD_PERIPHERAL_MINOR_GAMEPAD
            ) =>
        {
            Some(DeviceKind::Controller)
        }
        _ => None,
    }
}

pub(crate) fn key_for(container: &ContainerId) -> DeviceKey {
    DeviceKey(format!("bt:{:032X}", u128::from_be_bytes(*container)))
}

pub(crate) fn container_of(key: &DeviceKey) -> Option<ContainerId> {
    let hex = key.0.strip_prefix("bt:")?;
    (hex.len() == 32).then_some(())?;
    Some(u128::from_str_radix(hex, 16).ok()?.to_be_bytes())
}

/// Runtime state of this source. Neither visibility nor HID ownership affects its sampling.
#[derive(Default)]
pub(crate) struct Source {
    active: BTreeSet<ContainerId>,
    requested: bool,
    delayed: Option<Ms>,
    trailing: Option<Ms>,
    periodic: Option<Ms>,
}

impl Source {
    pub(crate) fn request(&mut self, now: Ms, follow_up: bool) {
        self.requested = true;
        if follow_up {
            let at = now.saturating_add(CONNECT_FOLLOW_UP_MS);
            // Keep the first follow-up, then cover every later connection with
            // at most one trailing scan at the newest connection's deadline.
            if let Some(first) = self.delayed {
                if at > first {
                    self.trailing = Some(at);
                }
            } else {
                self.delayed = Some(at);
            }
        }
    }

    pub(crate) fn deadline(&self) -> Option<Ms> {
        self.requested
            .then_some(0)
            .into_iter()
            .chain(self.delayed)
            .chain(self.periodic)
            .min()
    }

    pub(crate) fn take_due(&mut self, now: Ms) -> bool {
        let mut due = std::mem::take(&mut self.requested);
        if self.delayed.is_some_and(|at| at <= now) {
            self.delayed = self.trailing.take().filter(|&at| at > now);
            due = true;
        }
        if self.periodic.is_some_and(|at| at <= now) {
            // Preserve a connected-only deadline if the next scan fails.
            self.periodic = Some(now.saturating_add(CONNECTED_REREAD_MS));
            due = true;
        }
        due
    }

    pub(crate) fn reconcile(
        &mut self,
        scan: Result<Vec<Observation>>,
        now: Ms,
        hub: &Hub,
    ) -> Vec<HubInput> {
        let Ok(observed) = scan else {
            // Retry a failed scan unless one is already due; a success while idle ends this.
            if self.delayed.is_none() {
                self.periodic
                    .get_or_insert(now.saturating_add(CONNECTED_REREAD_MS));
            }
            return Vec::new();
        };
        if observed.iter().any(|node| node.connected) {
            self.periodic
                .get_or_insert(now.saturating_add(CONNECTED_REREAD_MS));
        } else {
            self.periodic = None;
        }
        let mut active = BTreeSet::new();
        let mut inputs = Vec::new();
        for observation in observed.into_iter().filter(|node| node.connected) {
            let key = key_for(&observation.container);
            if observation.level.is_none() && !hub.retains_system(&key, now) {
                continue;
            }
            self.active.remove(&observation.container);
            active.insert(observation.container);
            inputs.push(HubInput::System(SystemObservation {
                update: DeviceUpdate {
                    key,
                    name: Some(observation.name),
                    kind: Some(observation.kind),
                    via: Some(Via::Bluetooth),
                    level: observation.level.map(Level::Percent),
                    millivolts: None,
                    charge: None,
                    presence: Some(Presence::Online),
                    source: SampleSource::Os,
                },
                verification: Verification::SourceVerified,
                reports_changes: true,
            }));
        }
        inputs.extend(
            self.active
                .iter()
                .map(|id| HubInput::SystemGone(key_for(id))),
        );
        self.active = active;
        inputs
    }
}

#[cfg(test)]
mod tests {
    fn group_nodes(nodes: impl IntoIterator<Item = Node>) -> Vec<Observation> {
        let mut groups = BTreeMap::new();
        for node in nodes {
            add_node(&mut groups, node);
        }
        observations(groups)
    }

    use super::*;
    use batterysight_core::{
        catalog::Catalog,
        hub::{Hub, HubOutput},
        scheduler::Policy,
    };
    use windows::Win32::Foundation::E_FAIL;

    const ID: [u8; 16] = [0x12; 16];

    fn classic(connected: bool, level: Option<u8>) -> Vec<Observation> {
        group_nodes([
            Node {
                container: ID,
                role: NodeRole::ClassicPeer,
                connected,
                name: Some("Example Headphones".into()),
                ..Node::default()
            },
            Node {
                container: ID,
                role: NodeRole::HandsFree,
                level,
                ..Node::default()
            },
            Node {
                container: ID,
                role: NodeRole::Other,
                name: Some("Service name".into()),
                ..Node::default()
            },
        ])
    }

    #[test]
    fn only_exactly_one_byte_in_range_is_a_level() {
        for (kind, size, value, expected) in [
            (DEVPROP_TYPE_BYTE, 1, 0, Some(0)),
            (DEVPROP_TYPE_BYTE, 1, 100, Some(100)),
            (DEVPROP_TYPE_BYTE, 1, 101, None),
            (DEVPROP_TYPE_BYTE, 0, 42, None),
            (DEVPROP_TYPE_BYTE, 2, 42, None),
            (DEVPROP_TYPE_UINT32, 1, 42, None),
        ] {
            assert_eq!(battery_level(kind, size, value), expected);
        }
        assert!(classic(true, None)[0].level.is_none());
    }

    #[test]
    fn classic_peer_presence_and_hands_free_battery_form_one_device() {
        let nodes = classic(true, Some(75));
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].container, ID);
        assert_eq!(nodes[0].name, "Example Headphones");
        assert_eq!(nodes[0].kind, DeviceKind::Headset);
        assert!(nodes[0].connected);
        assert_eq!(nodes[0].level, Some(75));
        assert!(!classic(false, Some(75))[0].connected);
    }

    #[test]
    fn present_hogp_links_to_le_peer_battery_without_using_peer_presence() {
        let peer = || Node {
            container: ID,
            role: NodeRole::LePeer,
            level: Some(34),
            ..Node::default()
        };
        let hogp = Node {
            container: ID,
            role: NodeRole::Hogp,
            connected: true,
            ..Node::default()
        };
        assert!(!group_nodes([peer()])[0].connected);
        let nodes = group_nodes([peer(), hogp]);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].connected);
        assert_eq!(nodes[0].level, Some(34));
    }

    #[test]
    fn error_is_not_a_disconnect_and_cached_off_values_are_not_samples() {
        let mut source = Source::default();
        let mut hub = Hub::new(Catalog::builtin(), Policy::default());
        let mut apply = |scan, now| {
            source
                .reconcile(scan, now, &hub)
                .into_iter()
                .flat_map(|input| hub.handle(input, now))
                .collect::<Vec<_>>()
        };
        assert!(apply(Ok(classic(false, Some(99))), 0).is_empty());
        assert!(apply(Ok(classic(true, None)), 1).is_empty());
        assert_eq!(
            apply(Ok(classic(true, Some(75))), 10),
            [HubOutput::StateChanged]
        );
        assert!(apply(Ok(classic(true, Some(75))), 20).is_empty());
        assert!(apply(Ok(classic(true, None)), 21).is_empty());
        assert!(apply(Err(windows::core::Error::from_hresult(E_FAIL)), 30).is_empty());
        assert_eq!(
            apply(Ok(classic(false, Some(12))), 40),
            [HubOutput::StateChanged]
        );
        assert!(apply(Ok(vec![]), 50).is_empty());
        let last = &hub.snapshot()[0];
        assert_eq!(last.key.0, "bt:12121212121212121212121212121212");
        assert_eq!(last.level, Some(Level::Percent(75)));
        assert_eq!(last.presence, Presence::Offline);
        assert_eq!(last.updated_ms, 10);
        hub.handle(HubInput::Tick, 60_040);
        assert!(hub.snapshot().is_empty());
    }

    #[test]
    fn a_successfully_empty_scan_marks_the_active_device_gone() {
        let mut source = Source::default();
        let hub = Hub::new(Catalog::builtin(), Policy::default());
        assert!(matches!(
            &source.reconcile(Ok(classic(true, Some(75))), 0, &hub)[..],
            [HubInput::System(_)]
        ));
        assert!(matches!(
            &source.reconcile(Ok(vec![]), 1, &hub)[..],
            [HubInput::SystemGone(_)]
        ));
    }

    #[test]
    fn bluetooth_loudspeaker_class_is_not_generic_audio() {
        assert_eq!(device_kind(0x0414), Some(DeviceKind::Speaker));
        assert_eq!(device_kind(0x0400), Some(DeviceKind::Other));
    }

    #[test]
    fn bluetooth_late_connections_keep_one_trailing_scan_while_idle() {
        let mut source = Source::default();
        let hub = Hub::new(Catalog::builtin(), Policy::default());
        for now in [0, 3_800, 3_900] {
            source.request(now, true);
            assert!(source.take_due(now));
            source.reconcile(Ok(classic(false, Some(99))), now, &hub);
        }
        assert_eq!(source.deadline(), Some(4_000));
        assert!(source.take_due(4_000));
        source.reconcile(Ok(classic(false, Some(99))), 4_000, &hub);
        assert_eq!(source.deadline(), Some(7_900));
        assert!(!source.take_due(7_899));
        assert!(source.take_due(7_900));
        source.reconcile(Ok(classic(false, Some(99))), 7_900, &hub);
        assert_eq!(source.deadline(), None);
    }

    #[test]
    fn idle_connect_event_wakes_discovery_with_one_bounded_follow_up() {
        let mut source = Source::default();
        let hub = Hub::new(Catalog::builtin(), Policy::default());
        source.reconcile(Ok(classic(false, Some(99))), 0, &hub);
        assert_eq!(source.deadline(), None);
        source.request(20, true);
        assert!(source.take_due(20));
        // PnP can still show the old state on the first scan.
        source.reconcile(Ok(classic(false, Some(99))), 20, &hub);
        assert_eq!(source.deadline(), Some(4_020));
        assert!(!source.take_due(4_019));
        assert!(source.take_due(4_020));
        assert!(matches!(
            &source.reconcile(Ok(classic(true, Some(75))), 4_020, &hub)[..],
            [HubInput::System(_)]
        ));
        assert_eq!(source.deadline(), Some(64_020));
        assert!(source.take_due(64_020));
        source.reconcile(
            Err(windows::core::Error::from_hresult(E_FAIL)),
            64_020,
            &hub,
        );
        assert_eq!(source.deadline(), Some(124_020));
        source.request(65_000, false);
        assert!(source.take_due(65_000));
        source.reconcile(Ok(classic(false, Some(75))), 65_000, &hub);
        assert_eq!(source.deadline(), None);
    }

    #[test]
    fn a_failed_scan_retries_while_scans_keep_failing() {
        let mut source = Source::default();
        let hub = Hub::new(Catalog::builtin(), Policy::default());
        let failed = || Err(windows::core::Error::from_hresult(E_FAIL));
        source.request(0, false);
        assert!(source.take_due(0));
        source.reconcile(failed(), 0, &hub);
        assert_eq!(source.deadline(), Some(60_000));
        assert!(!source.take_due(59_999));
        assert!(source.take_due(60_000));
        source.reconcile(failed(), 60_000, &hub);
        assert_eq!(source.deadline(), Some(120_000));
    }

    #[test]
    fn a_successful_idle_scan_ends_the_failure_retry() {
        let mut source = Source::default();
        let hub = Hub::new(Catalog::builtin(), Policy::default());
        source.reconcile(Err(windows::core::Error::from_hresult(E_FAIL)), 0, &hub);
        assert_eq!(source.deadline(), Some(60_000));
        assert!(source.take_due(60_000));
        source.reconcile(Ok(classic(false, Some(99))), 60_000, &hub);
        assert_eq!(source.deadline(), None, "idle discovery does not poll");
    }
}
