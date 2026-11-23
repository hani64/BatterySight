//! HID interface arrival and removal notifications.
//!
//! Configuration Manager calls [`on_notification`] on a thread-pool thread; it only posts an
//! event to the reactor. Interfaces present before registration are not reported.

use std::ffi::c_void;
use std::mem::offset_of;
use std::ptr;
use std::slice;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_MapCrToWin32Err, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
    CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_EVENT_DATA_0_0,
    CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification,
    CM_Unregister_Notification, CR_SUCCESS, HCMNOTIFICATION,
};
use windows::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;
use windows::Win32::Foundation::{ERROR_GEN_FAILURE, ERROR_SUCCESS, WIN32_ERROR};

use crate::reactor::{OsEvent, Poster};

/// Keeps the HID interface notifications registered; unregisters on drop.
pub struct NotifyGuard {
    handle: HCMNOTIFICATION,
    /// The callback context; freed only after unregistering.
    _poster: Box<Poster>,
}

impl Drop for NotifyGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CM_Register_Notification`. Unregistering waits for
        // callbacks in progress, so the poster outlives every callback.
        let _ = unsafe { CM_Unregister_Notification(self.handle) };
    }
}

/// Posts `InterfaceArrived` and `InterfaceRemoved` for HID interfaces until the guard drops.
pub fn register_hid_notifications(poster: Poster) -> windows::core::Result<NotifyGuard> {
    let poster = Box::new(poster);
    let mut filter = CM_NOTIFY_FILTER {
        cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
        FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
        ..Default::default()
    };
    filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_HID;
    let mut handle = HCMNOTIFICATION::default();
    // SAFETY: the filter is valid for the call; the context is the boxed poster, which the
    // guard frees only after unregistering.
    let result = unsafe {
        CM_Register_Notification(
            &filter,
            Some(ptr::from_ref::<Poster>(&poster).cast()),
            Some(on_notification),
            &mut handle,
        )
    };
    if result != CR_SUCCESS {
        // SAFETY: maps a return code; no pointers involved.
        let code = unsafe { CM_MapCrToWin32Err(result, ERROR_GEN_FAILURE.0) };
        WIN32_ERROR(code).ok()?;
    }
    Ok(NotifyGuard {
        handle,
        _poster: poster,
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
    let event = match action {
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL => OsEvent::InterfaceArrived,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL if !data.is_null() => OsEvent::InterfaceRemoved {
            // SAFETY: Configuration Manager passes `size` readable bytes of interface data.
            path: unsafe { symbolic_link(data, size) },
        },
        _ => return ERROR_SUCCESS.0,
    };
    poster.post(event);
    ERROR_SUCCESS.0
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
