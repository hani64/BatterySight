//! Local Windows device observations, independent of HID engine grants.

pub mod bluetooth;
pub mod xbox;

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
