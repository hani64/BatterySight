//! HID interfaces and local Bluetooth radio/interface notifications.
//!
//! Configuration Manager calls [`on_notification`] on a thread-pool thread; it only posts an
//! event to the reactor. Interfaces present before registration are not reported.

use std::ffi::c_void;
use std::mem::offset_of;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;
use std::slice;
use std::sync::{Mutex, PoisonError};

use windows::Win32::Devices::Bluetooth::{
    BLUETOOTH_FIND_RADIO_PARAMS, BTH_HCI_EVENT_INFO, BluetoothFindFirstRadio,
    BluetoothFindNextRadio, BluetoothFindRadioClose, GUID_BLUETOOTH_HCI_EVENT,
    GUID_BLUETOOTHLE_DEVICE_INTERFACE, GUID_BTHPORT_DEVICE_INTERFACE, HCI_CONNECTION_TYPE_ACL,
    HCI_CONNECTION_TYPE_LE,
};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICECUSTOMEVENT, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
    CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
    CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
    CM_NOTIFY_ACTION_DEVICEREMOVEPENDING, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_EVENT_DATA_0_0,
    CM_NOTIFY_EVENT_DATA_0_1, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CM_Unregister_Notification,
    HCMNOTIFICATION, PCM_NOTIFY_CALLBACK,
};
use windows::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::core::GUID;

use crate::hid_enum::cm_result;
use crate::reactor::{OsEvent, Poster};

/// Keeps the interface notifications registered; unregisters on drop.
pub struct NotifyGuard {
    _registration: Registration<Poster>,
}

struct Registration<T> {
    handle: HCMNOTIFICATION,
    /// Stable callback context; freed only after unregistering.
    _context: Box<T>,
}

impl<T> Drop for Registration<T> {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CM_Register_Notification`. Unregistering waits for
        // callbacks in progress, so the poster outlives every callback.
        let _ = unsafe { CM_Unregister_Notification(self.handle) };
    }
}

/// Posts `InterfaceArrived` and `InterfaceRemoved` for HID interfaces until the guard drops.
pub fn register_hid_notifications(poster: Poster) -> windows::core::Result<NotifyGuard> {
    register_interface(poster, GUID_DEVINTERFACE_HID)
}

pub(crate) fn register_bluetooth_interfaces(poster: &Poster) -> [Option<NotifyGuard>; 2] {
    [
        GUID_BTHPORT_DEVICE_INTERFACE,
        GUID_BLUETOOTHLE_DEVICE_INTERFACE,
    ]
    .map(|class| register_interface(poster.clone(), class).ok())
}

fn register_interface(poster: Poster, class: GUID) -> windows::core::Result<NotifyGuard> {
    let mut filter = CM_NOTIFY_FILTER {
        cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
        FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
        ..Default::default()
    };
    filter.u.DeviceInterface.ClassGuid = class;
    Ok(NotifyGuard {
        _registration: register(&filter, Box::new(poster), Some(on_notification))?,
    })
}

fn register<T>(
    filter: &CM_NOTIFY_FILTER,
    context: Box<T>,
    callback: PCM_NOTIFY_CALLBACK,
) -> windows::core::Result<Registration<T>> {
    let mut handle = HCMNOTIFICATION::default();
    // SAFETY: the filter is valid for the call; the boxed callback context is stable
    // until the registration drops, then gets freed only after unregistering.
    let result = unsafe {
        CM_Register_Notification(
            filter,
            Some(ptr::from_ref(context.as_ref()).cast()),
            callback,
            &mut handle,
        )
    };
    cm_result(result)?;
    Ok(Registration {
        handle,
        _context: context,
    })
}

/// `PCM_NOTIFY_CALLBACK` for the HID interface filter. `context` is the guard's `Poster`.
unsafe extern "system" fn on_notification(
    _notify: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    size: u32,
) -> u32 {
    // SAFETY: the context is the poster boxed in the guard, alive while registered.
    let poster = unsafe { &*context.cast::<Poster>() };
    let arrived = match action {
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL => true,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL => false,
        _ => return ERROR_SUCCESS.0,
    };
    let link_offset =
        offset_of!(CM_NOTIFY_EVENT_DATA, u) + offset_of!(CM_NOTIFY_EVENT_DATA_0_0, SymbolicLink);
    if data.is_null() || (size as usize) < link_offset {
        return ERROR_SUCCESS.0;
    }
    // SAFETY: the class GUID ends before the link offset, inside the reported size.
    let class = unsafe { (*data).u.DeviceInterface.ClassGuid };
    let event = match class {
        GUID_DEVINTERFACE_HID if arrived => OsEvent::InterfaceArrived,
        GUID_DEVINTERFACE_HID => OsEvent::InterfaceRemoved {
            // SAFETY: Configuration Manager passes `size` readable bytes of interface data.
            path: unsafe { symbolic_link(data, size) },
        },
        GUID_BTHPORT_DEVICE_INTERFACE | GUID_BLUETOOTHLE_DEVICE_INTERFACE => {
            OsEvent::BluetoothInterface {
                radio: class == GUID_BTHPORT_DEVICE_INTERFACE,
                arrived,
            }
        }
        _ => return ERROR_SUCCESS.0,
    };
    poster.post(event);
    ERROR_SUCCESS.0
}

struct RadioContext {
    poster: Poster,
    // A query-remove must close the handle before returning from the callback.
    // Closing a Win32 handle from another thread is supported; only one caller takes it.
    radio: Mutex<Option<OwnedHandle>>,
}

pub(crate) struct RadioNotify {
    _registration: Registration<RadioContext>,
}

/// Finds already-installed local radios, not remote devices. No inquiry is initiated.
pub(crate) fn watch_local_radios(poster: &Poster) -> Vec<RadioNotify> {
    let params = BLUETOOTH_FIND_RADIO_PARAMS {
        dwSize: size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
    };
    let mut handle = HANDLE::default();
    // SAFETY: `params` and the writable handle slot live through the call.
    let Ok(find) = (unsafe { BluetoothFindFirstRadio(&params, &mut handle) }) else {
        return Vec::new();
    };
    let mut radios = Vec::new();
    loop {
        let mut filter = CM_NOTIFY_FILTER {
            cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
            ..Default::default()
        };
        filter.u.DeviceHandle.hTarget = handle;
        let context = Box::new(RadioContext {
            poster: poster.clone(),
            // SAFETY: BluetoothFindFirst/NextRadio transfers this open handle to the caller.
            radio: Mutex::new(Some(unsafe { OwnedHandle::from_raw_handle(handle.0) })),
        });
        if let Ok(registration) = register(&filter, context, Some(on_radio_notification)) {
            radios.push(RadioNotify {
                _registration: registration,
            });
        }
        // SAFETY: `find` is live and `handle` is a writable out slot.
        if unsafe { BluetoothFindNextRadio(find, &mut handle) }.is_err() {
            break;
        }
    }
    // SAFETY: releases the iterator, not the individual radio handles.
    let _ = unsafe { BluetoothFindRadioClose(find) };
    radios
}

unsafe extern "system" fn on_radio_notification(
    _notify: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    size: u32,
) -> u32 {
    // SAFETY: the radio context stays boxed until unregistering waits for this callback.
    let context = unsafe { &*context.cast::<RadioContext>() };
    if matches!(
        action,
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE
            | CM_NOTIFY_ACTION_DEVICEREMOVEPENDING
            | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE
    ) {
        let radio = context
            .radio
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(radio);
    }
    match action {
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE => {
            // Re-enumerate/register on the device thread, never inside the callback.
            context.poster.post(OsEvent::BluetoothInterface {
                radio: true,
                arrived: false,
            });
        }
        CM_NOTIFY_ACTION_DEVICECUSTOMEVENT => {
            // SAFETY: Configuration Manager passes `size` readable bytes of event data.
            if let Some(connected) = unsafe { hci_link(data, size as usize) } {
                context.poster.post(OsEvent::BluetoothLink { connected });
            }
        }
        _ => {}
    }
    ERROR_SUCCESS.0
}

/// Reads only the connection type and state, never the remote address.
///
/// Safety: if non-null, `data` points to `size` readable bytes from CM.
unsafe fn hci_link(data: *const CM_NOTIFY_EVENT_DATA, size: usize) -> Option<bool> {
    const START: usize =
        offset_of!(CM_NOTIFY_EVENT_DATA, u) + offset_of!(CM_NOTIFY_EVENT_DATA_0_1, Data);
    const TYPE: usize = offset_of!(BTH_HCI_EVENT_INFO, connectionType);
    const CONNECTED: usize = offset_of!(BTH_HCI_EVENT_INFO, connected);
    if data.is_null() || size < START {
        return None;
    }
    // SAFETY: the filter, event GUID and data size are fully before START.
    let (filter, guid, len) = unsafe {
        (
            (*data).FilterType,
            (*data).u.DeviceHandle.EventGuid,
            (*data).u.DeviceHandle.DataSize as usize,
        )
    };
    if filter != CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE
        || guid != GUID_BLUETOOTH_HCI_EVENT
        || len <= CONNECTED
        || len > size - START
    {
        return None;
    }
    // SAFETY: both byte offsets are within the checked event payload.
    let (kind, connected) = unsafe {
        let payload = data.cast::<u8>().add(START);
        (*payload.add(TYPE), *payload.add(CONNECTED))
    };
    matches!(
        u32::from(kind),
        HCI_CONNECTION_TYPE_ACL | HCI_CONNECTION_TYPE_LE
    )
    .then_some(connected != 0)
}

/// The interface path in device interface event data, NUL-terminated like
/// `HidInterface::path`.
///
/// # Safety
/// `data` must point to `size` readable bytes of `CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE`
/// event data.
unsafe fn symbolic_link(data: *const CM_NOTIFY_EVENT_DATA, size: u32) -> Vec<u16> {
    let offset =
        offset_of!(CM_NOTIFY_EVENT_DATA, u) + offset_of!(CM_NOTIFY_EVENT_DATA_0_0, SymbolicLink);
    let units = (size as usize).saturating_sub(offset) / size_of::<u16>();
    // SAFETY: the link starts at `offset` and fills the rest of the `size` bytes; it is
    // 2-aligned because it follows a GUID in an 8-aligned structure.
    let link = unsafe { slice::from_raw_parts(data.cast::<u8>().add(offset).cast::<u16>(), units) };
    let end = link
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(link.len());
    let mut path = Vec::with_capacity(end + 1);
    path.extend_from_slice(&link[..end]);
    path.push(0);
    path
}

#[cfg(test)]
mod bluetooth_tests {
    use super::*;
    use crate::reactor::Reactor;

    #[test]
    fn only_acl_or_le_events_post_an_address_free_link_change() {
        let mut reactor = Reactor::new().unwrap();
        let context = RadioContext {
            poster: reactor.poster(),
            radio: std::sync::Mutex::new(None),
        };
        let offset =
            offset_of!(CM_NOTIFY_EVENT_DATA, u) + offset_of!(CM_NOTIFY_EVENT_DATA_0_1, Data);
        let mut buffer = [0u64; 8];
        let data = buffer.as_mut_ptr().cast::<CM_NOTIFY_EVENT_DATA>();
        let handle = unsafe { &raw mut (*data).u.DeviceHandle };
        let bytes = unsafe { data.cast::<u8>().add(offset) };
        let size = u32::try_from(offset + 10).unwrap();
        let mut deliver = |ty, connected, length| unsafe {
            (*data).FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE;
            (*handle).EventGuid = GUID_BLUETOOTH_HCI_EVENT;
            (*handle).DataSize = length;
            *bytes.add(8) = ty;
            *bytes.add(9) = connected;
            on_radio_notification(
                HCMNOTIFICATION::default(),
                ptr::from_ref(&context).cast(),
                CM_NOTIFY_ACTION_DEVICECUSTOMEVENT,
                data,
                size,
            );
            reactor.wait(Some(0), 0)
        };
        assert!(deliver(2, 1, 10).is_empty()); // SCO is not device presence.
        assert!(deliver(1, 1, 9).is_empty()); // Truncated before `connected`.
        assert!(deliver(1, 1, 11).is_empty()); // Claims bytes past the event.
        assert_eq!(
            deliver(1, 1, 10),
            [OsEvent::BluetoothLink { connected: true }]
        );
        assert_eq!(
            deliver(1, 0, 10),
            [OsEvent::BluetoothLink { connected: false }]
        );
        assert_eq!(
            deliver(3, 1, 10),
            [OsEvent::BluetoothLink { connected: true }]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL,
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, CM_NOTIFY_EVENT_DATA_0_0,
        CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
    };
    use windows::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;

    use crate::reactor::{OsEvent, Reactor};

    const LINK: &str = r"\\?\HID#VID_046D&PID_C547&MI_02&Col01#8&2a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";

    /// Calls the callback the way Configuration Manager does: event data with the symbolic
    /// link inline after the class GUID, sized to the link and its NUL.
    fn deliver(poster: &Poster, action: CM_NOTIFY_ACTION, link: &str) {
        let units: Vec<u16> = link.encode_utf16().chain([0]).collect();
        let link_offset = offset_of!(CM_NOTIFY_EVENT_DATA, u)
            + offset_of!(CM_NOTIFY_EVENT_DATA_0_0, SymbolicLink);
        let size = link_offset + units.len() * 2;
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let data = buffer.as_mut_ptr().cast::<CM_NOTIFY_EVENT_DATA>();
        // SAFETY: the buffer is 8-aligned and large enough for the header and the link.
        unsafe {
            (*data).FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
            (*data).u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_HID;
            let link = data.cast::<u8>().add(link_offset).cast::<u16>();
            std::ptr::copy_nonoverlapping(units.as_ptr(), link, units.len());
            on_notification(
                HCMNOTIFICATION::default(),
                std::ptr::from_ref(poster).cast(),
                action,
                data,
                size as u32,
            );
        }
    }

    #[test]
    fn arrival_and_removal_are_posted() {
        let mut reactor = Reactor::new().unwrap();
        let poster = reactor.poster();
        deliver(&poster, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, LINK);
        deliver(&poster, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, LINK);
        deliver(&poster, CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, LINK);
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![
                OsEvent::InterfaceArrived,
                OsEvent::InterfaceRemoved {
                    path: LINK.encode_utf16().chain([0]).collect()
                },
            ]
        );
    }

    #[test]
    fn registers_and_unregisters() {
        let reactor = Reactor::new().unwrap();
        let guard = register_hid_notifications(reactor.poster()).unwrap();
        drop(guard);
    }
}
