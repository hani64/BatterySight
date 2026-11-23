//! HID interface enumeration and grouping by physical device.
//!
//! Interfaces are opened with access 0 and shared read/write, only long enough to read
//! attributes, capabilities and the product string. No reports are read or written here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use batterysight_core::hid::{Collection, CollectionId, GroupId, HidGroup};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData,
    HidD_GetAttributes, HidD_GetPreparsedData, HidD_GetProductString, HidP_GetCaps,
    PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::PCWSTR;

static NEXT_COLLECTION_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_GROUP_ID: AtomicU32 = AtomicU32::new(1);

/// Attempts at fetching the interface list when it grows between the size query and the fetch.
const LIST_ATTEMPTS: usize = 4;

/// Product string buffer in UTF-16 units: USB allows 126 characters plus the NUL.
const PRODUCT_LEN: usize = 128;

/// One HID top-level collection found on the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidInterface {
    /// UTF-16 device interface path including the trailing NUL, ready for `CreateFileW`.
    pub path: Vec<u16>,
    pub collection: Collection,
}

/// Lists the present HID interfaces in the order Configuration Manager returns them.
///
/// Each interface gets a fresh `CollectionId`. Only attributes and capabilities are read; both
/// come from the HID class driver without a request to the device. An interface whose
/// attributes or capabilities cannot be read is skipped. `collection.product` is left empty:
/// see [`product_string`].
pub fn enumerate() -> Vec<HidInterface> {
    interface_list()
        .split(|&unit| unit == 0)
        .filter(|path| !path.is_empty())
        .filter_map(query)
        .collect()
}

/// Key shared by every collection of one physical device.
///
/// Lowercases `path`, then joins the hardware segment (between the 1st and 2nd `#`) without
/// its `&colNN` parts and the instance segment (between the 2nd and 3rd `#`) without its last
/// `&…` part. Returns an empty string when the path has fewer than three `#`.
pub fn physical_key(path: &str) -> String {
    let lower = path.to_lowercase();
    let mut segments = lower.splitn(4, '#');
    let (Some(_), Some(hardware), Some(instance), Some(_)) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return String::new();
    };

    let mut key = String::with_capacity(hardware.len() + instance.len() + 1);
    for (index, part) in hardware.split('&').enumerate() {
        if index > 0 {
            if is_collection_part(part) {
                continue;
            }
            key.push('&');
        }
        key.push_str(part);
    }
    key.push('#');
    key.push_str(instance.rsplit_once('&').map_or(instance, |(head, _)| head));
    key
}

/// Groups interfaces by `(vid, pid, physical_key(path))`, one group per physical device.
///
/// Groups keep first-seen order, and so do the interfaces within a group. Paths of an
/// unexpected shape have an empty physical key, so they are grouped by vid/pid alone. Every
/// call assigns fresh `GroupId`s.
pub fn group(interfaces: Vec<HidInterface>) -> Vec<(HidGroup, Vec<HidInterface>)> {
    let mut slots: HashMap<(u16, u16, String), usize> = HashMap::new();
    let mut groups: Vec<(HidGroup, Vec<HidInterface>)> = Vec::new();
    for interface in interfaces {
        let key = (
            interface.collection.vid,
            interface.collection.pid,
            physical_key(&path_string(&interface.path)),
        );
        let slot = *slots.entry(key).or_insert_with(|| {
            let id = GroupId(NEXT_GROUP_ID.fetch_add(1, Ordering::Relaxed));
            let collections = Vec::new();
            groups.push((HidGroup { id, collections }, Vec::new()));
            groups.len() - 1
        });
        let (hid_group, members) = &mut groups[slot];
        hid_group.collections.push(interface.collection.clone());
        members.push(interface);
    }
    groups
}

/// Decodes a UTF-16 path up to its first NUL.
fn path_string(path: &[u16]) -> String {
    let end = path
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(path.len());
    String::from_utf16_lossy(&path[..end])
}

/// `col` followed by exactly two hex digits, the `&colNN` top-level collection suffix
/// (`Colb(2)`, 0x00-0xFF, in Microsoft's HIDClass hardware IDs).
fn is_collection_part(part: &str) -> bool {
    part.strip_prefix("col")
        .is_some_and(|digits| digits.len() == 2 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The present HID interface paths as a NUL-separated, double-NUL-terminated list.
fn interface_list() -> Vec<u16> {
    for _ in 0..LIST_ATTEMPTS {
        let mut len = 0u32;
        // SAFETY: `len` is a valid out pointer and the GUID outlives the call.
        let status = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &GUID_DEVINTERFACE_HID,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status != CR_SUCCESS {
            return Vec::new();
        }
        let mut buffer = vec![0u16; len as usize];
        // SAFETY: the buffer is writable for its full length, which the wrapper passes along.
        let status = unsafe {
            CM_Get_Device_Interface_ListW(
                &GUID_DEVINTERFACE_HID,
                PCWSTR::null(),
                &mut buffer,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status == CR_SUCCESS {
            return buffer;
        }
        if status != CR_BUFFER_SMALL {
            return Vec::new();
        }
    }
    Vec::new()
}

/// Opens one interface without access rights, shared read/write, for queries only.
///
/// `path` must be NUL-terminated.
fn open_for_query(path: &[u16]) -> Option<OwnedHandle> {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    Some(OwnedHandle(handle))
}

/// Reads one interface's identity from the HID class driver.
fn query(path: &[u16]) -> Option<HidInterface> {
    let mut wide = Vec::with_capacity(path.len() + 1);
    wide.extend_from_slice(path);
    wide.push(0);
    let handle = open_for_query(&wide)?;

    let mut attributes = HIDD_ATTRIBUTES {
        Size: size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    // SAFETY: the handle is open and `attributes` is a valid out pointer with `Size` set.
    if !unsafe { HidD_GetAttributes(handle.0, &mut attributes) } {
        return None;
    }

    let mut preparsed = PHIDP_PREPARSED_DATA::default();
    // SAFETY: the handle is open and `preparsed` is a valid out pointer.
    if !unsafe { HidD_GetPreparsedData(handle.0, &mut preparsed) } {
        return None;
    }
    let preparsed = PreparsedData(preparsed);
    let mut caps = HIDP_CAPS::default();
    // SAFETY: `preparsed` came from `HidD_GetPreparsedData` and is freed only on drop.
    if unsafe { HidP_GetCaps(preparsed.0, &mut caps) } != HIDP_STATUS_SUCCESS {
        return None;
    }

    Some(HidInterface {
        path: wide,
        collection: Collection {
            id: CollectionId(NEXT_COLLECTION_ID.fetch_add(1, Ordering::Relaxed)),
            vid: attributes.VendorID,
            pid: attributes.ProductID,
            usage_page: caps.UsagePage,
            usage: caps.Usage,
            input_len: caps.InputReportByteLength,
            output_len: caps.OutputReportByteLength,
            feature_len: caps.FeatureReportByteLength,
            product: String::new(),
        },
    })
}

/// The product string of the interface at `path`, or an empty string on any failure.
///
/// `HidD_GetProductString` is a request to the device and can block for seconds, for example
/// on a paired but disconnected Bluetooth device. Call it only for interfaces in groups an
/// engine accepts, once per path. `path` may include the trailing NUL.
pub fn product_string(path: &[u16]) -> String {
    let terminated;
    let path = if path.last() == Some(&0) {
        path
    } else {
        terminated = [path, &[0]].concat();
        &terminated
    };
    let Some(handle) = open_for_query(path) else {
        return String::new();
    };
    let mut buffer = [0u16; PRODUCT_LEN];
    // SAFETY: the handle is open and the buffer is writable for the byte length passed.
    let ok = unsafe {
        HidD_GetProductString(
            handle.0,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
        )
    };
    if ok {
        path_string(&buffer)
    } else {
        String::new()
    }
}

/// Closes the handle on drop.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `CreateFileW` and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Frees the preparsed data on drop.
struct PreparsedData(PHIDP_PREPARSED_DATA);

impl Drop for PreparsedData {
    fn drop(&mut self) {
        // SAFETY: the data came from a successful `HidD_GetPreparsedData` and is freed only here.
        let _ = unsafe { HidD_FreePreparsedData(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::hid::CollectionId;

    const C547_COL01: &str = r"\\?\hid#vid_046d&pid_c547&mi_02&col01#8&2a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const C547_COL02: &str = r"\\?\hid#vid_046d&pid_c547&mi_02&col02#8&2a2b3c4d&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const C547_OTHER: &str = r"\\?\hid#vid_046d&pid_c547&mi_02&col01#8&5e6f7a8b&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const ABA: &str =
        r"\\?\hid#vid_046d&pid_0aba&mi_03#7&1a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";

    fn iface(path: &str, vid: u16, pid: u16, usage: u16) -> HidInterface {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        HidInterface {
            path: wide,
            collection: Collection {
                id: CollectionId(u32::from(usage)),
                vid,
                pid,
                usage_page: 0xFF00,
                usage,
                input_len: 7,
                output_len: 7,
                feature_len: 0,
                product: String::new(),
            },
        }
    }

    fn usages(list: &[HidInterface]) -> Vec<u16> {
        list.iter().map(|i| i.collection.usage).collect()
    }

    #[test]
    fn physical_key_strips_col_and_instance_suffix() {
        let expected = "vid_046d&pid_c547&mi_02#8&2a2b3c4d&0";
        assert_eq!(physical_key(C547_COL01), expected);
        assert_eq!(physical_key(C547_COL02), expected);
    }

    #[test]
    fn physical_key_strips_hex_collection_suffix() {
        let col0a = C547_COL01.replace("&col01", "&col0a");
        assert_eq!(physical_key(&col0a), physical_key(C547_COL01));
    }

    #[test]
    fn physical_key_odd_shape_is_empty() {
        assert_eq!(physical_key("abc"), "");
        assert_eq!(
            physical_key(r"\\?\hid#vid_046d&pid_c547#8&2a2b3c4d&0&0000"),
            ""
        );
    }

    #[test]
    fn physical_key_lowercases() {
        let upper = r"\\?\HID#VID_046D&PID_C547&MI_02&Col01#8&2A2B3C4D&0&0000#{4D1E55B2-F16F-11CF-88CB-001111000030}";
        assert_eq!(physical_key(upper), "vid_046d&pid_c547&mi_02#8&2a2b3c4d&0");
    }

    #[test]
    fn physical_key_keeps_instance_without_ampersand() {
        assert_eq!(
            physical_key(r"\\?\hid#vid_1&pid_2#abc#{x}"),
            "vid_1&pid_2#abc"
        );
    }

    #[test]
    fn physical_key_does_not_strip_non_collection_parts() {
        assert_eq!(
            physical_key(r"\\?\hid#vid_1&pid_2&color#a&b#{x}"),
            "vid_1&pid_2&color#a"
        );
    }

    #[test]
    fn group_merges_collections_of_one_physical_device() {
        let groups = group(vec![
            iface(C547_COL01, 0x046D, 0xC547, 1),
            iface(ABA, 0x046D, 0x0ABA, 3),
            iface(C547_COL02, 0x046D, 0xC547, 2),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(usages(&groups[0].1), [1, 2]);
        assert_eq!(usages(&groups[1].1), [3]);
        for (hid_group, members) in &groups {
            let collections: Vec<Collection> =
                members.iter().map(|i| i.collection.clone()).collect();
            assert_eq!(hid_group.collections, collections);
        }
        assert_ne!(groups[0].0.id, groups[1].0.id);
    }

    #[test]
    fn group_splits_identical_receivers_by_physical_key() {
        let groups = group(vec![
            iface(C547_COL01, 0x046D, 0xC547, 1),
            iface(C547_OTHER, 0x046D, 0xC547, 2),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(usages(&groups[0].1), [1]);
        assert_eq!(usages(&groups[1].1), [2]);
    }

    #[test]
    fn group_splits_same_key_with_different_ids() {
        let groups = group(vec![
            iface(C547_COL01, 0x046D, 0xC547, 1),
            iface(C547_COL02, 0x046D, 0xC548, 2),
        ]);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn group_with_empty_key_groups_by_vid_pid() {
        let groups = group(vec![
            iface("odd-a", 0x1234, 0x0001, 1),
            iface("odd-b", 0x1234, 0x0002, 2),
            iface("odd-c", 0x1234, 0x0001, 3),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(usages(&groups[0].1), [1, 3]);
        assert_eq!(usages(&groups[1].1), [2]);
    }

    #[test]
    fn group_of_nothing_is_empty() {
        assert!(group(Vec::new()).is_empty());
    }

    /// Manual hardware check: `cargo test -p batterysight-win -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the HID devices of this machine"]
    fn print_enumeration() {
        for (hid_group, members) in group(enumerate()) {
            for member in &members {
                let c = &member.collection;
                println!(
                    "group {:>3}  {:04X}:{:04X}  {:04X}:{:04X}  in {:>3} out {:>3} feat {:>3}  {:?}  {}",
                    hid_group.id.0,
                    c.vid,
                    c.pid,
                    c.usage_page,
                    c.usage,
                    c.input_len,
                    c.output_len,
                    c.feature_len,
                    product_string(&member.path),
                    physical_key(&path_string(&member.path)),
                );
            }
        }
    }
}
