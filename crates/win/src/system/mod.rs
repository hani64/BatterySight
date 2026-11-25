//! Local Windows device observations, independent of HID engine grants.

pub mod bluetooth;
pub mod xbox;

use std::collections::BTreeSet;

use crate::hid_enum::interface_list;

/// The bus a HID collection's devnode is enumerated on, from its interface path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HidBus {
    /// `HID#VID_…`: the USB HID class driver's naming.
    Usb,
    /// `HID#{00001124-…}` (Classic HID profile) or `HID#{00001812-…}` (HID over GATT).
    Bluetooth,
    Other,
}

/// Classifies `path` (UTF-16, NUL optional) without opening anything.
pub(crate) fn hid_bus(path: &[u16]) -> HidBus {
    let end = path
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(path.len());
    let path = String::from_utf16_lossy(&path[..end]).to_ascii_lowercase();
    let mut segments = path.split('#');
    let (Some(head), Some(hardware)) = (segments.next(), segments.next()) else {
        return HidBus::Other;
    };
    if !head.ends_with(r"\hid") {
        HidBus::Other
    } else if hardware.starts_with("vid_") {
        HidBus::Usb
    } else if hardware.starts_with("{00001124-0000-1000-8000-00805f9b34fb}")
        || hardware.starts_with("{00001812-0000-1000-8000-00805f9b34fb}")
    {
        HidBus::Bluetooth
    } else {
        HidBus::Other
    }
}

/// The hex digits after `tag` (`vid` or `pid`, then `_` or `&`) in a lowercase path segment.
fn hex_after<'a>(segment: &'a str, tag: &str) -> Option<&'a str> {
    let start = segment
        .match_indices(tag)
        .map(|(at, _)| at + tag.len())
        .find(|&at| matches!(segment.as_bytes().get(at), Some(b'_' | b'&')))?
        + 1;
    let rest = &segment[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// What a HID path's hardware segment says about its vendor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathVendor {
    /// A USB-IF VID and PID: `vid_045e&pid_0b12` (USB), `vid&0002045e_pid&02e0`
    /// (Classic) or `vid&02045e_pid&0b13` (LE).
    Product(u16, u16),
    /// A Bluetooth SIG company identifier (source `0001`/`01`).
    SigCompany(u16),
    /// No VID token, a zero or unknown source, or no readable PID.
    Unknown,
}

/// Parses a lowercase HID path's hardware segment (between its first and second `#`).
pub(crate) fn path_vendor(hardware: &str) -> PathVendor {
    let Some(vid) = hex_after(hardware, "vid") else {
        return PathVendor::Unknown;
    };
    let (source, id) = match vid.len() {
        4 => ("0002", vid),
        6 => vid.split_at(2),
        8 => vid.split_at(4),
        _ => return PathVendor::Unknown,
    };
    let Ok(id) = u16::from_str_radix(id, 16) else {
        return PathVendor::Unknown;
    };
    let pid = hex_after(hardware, "pid")
        .filter(|pid| pid.len() == 4)
        .and_then(|pid| u16::from_str_radix(pid, 16).ok());
    match (source, pid) {
        ("0002" | "02", Some(pid)) => PathVendor::Product(id, pid),
        ("0001" | "01", _) => PathVendor::SigCompany(id),
        _ => PathVendor::Unknown,
    }
}

/// The USB-IF VID:PIDs of the present HID interfaces, from Configuration Manager's path
/// text alone: nothing is opened. `None` when Configuration Manager cannot list them.
pub fn present_products() -> Option<BTreeSet<(u16, u16)>> {
    interface_list().map(|list| products_in(&list))
}

/// The USB-IF VID:PIDs in the hardware segments of `list` (NUL-separated paths); paths
/// without one (a Bluetooth SIG company, no VID, an unreadable PID) are skipped.
fn products_in(list: &[u16]) -> BTreeSet<(u16, u16)> {
    list.split(|&unit| unit == 0)
        .filter_map(|path| {
            let text = String::from_utf16_lossy(path).to_ascii_lowercase();
            match path_vendor(text.split('#').nth(1)?) {
                PathVendor::Product(vid, pid) => Some((vid, pid)),
                PathVendor::SigCompany(_) | PathVendor::Unknown => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(paths: &[&str]) -> Vec<u16> {
        paths
            .iter()
            .flat_map(|path| path.encode_utf16().chain([0]))
            .chain([0])
            .collect()
    }

    #[test]
    fn products_come_from_path_text() {
        let products = products_in(&list(&[
            r"\\?\HID#VID_1038&PID_12E0&MI_04#7&1a2b3c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}",
            r"\\?\HID#VID_046D&PID_C547&MI_02&Col01#7&2&0&0000#{4d1e55b2}",
            r"\\?\HID#VID_046D&PID_C547&MI_02&Col02#7&2&0&0001#{4d1e55b2}",
            r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0002045e_pid&02e0#1&2&0&0000#{4d1e55b2}",
            // No product: a SIG company, no VID token, an unreadable PID, a VID in the
            // instance segment only.
            r"\\?\HID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0001004c_PID&1234#1&2&0&0000#{4d1e55b2}",
            r"\\?\HID#ConvertedDevice&Col01#1&2&0&0000#{4d1e55b2}",
            r"\\?\HID#VID_1038&PID_XYZ#1&2&0&0000#{4d1e55b2}",
            r"\\?\HID#Virtual&Col01#vid_1038&pid_12e5#{4d1e55b2}",
        ]));
        assert_eq!(
            products,
            BTreeSet::from([(0x045E, 0x02E0), (0x046D, 0xC547), (0x1038, 0x12E0)])
        );
        assert!(products_in(&[0]).is_empty());
    }
}
