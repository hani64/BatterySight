//! Suspend and resume notifications.
//!
//! The power manager calls [`on_power`] on a system thread; it only posts an event to the
//! reactor.

use std::ffi::c_void;
use std::ptr;

use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, PowerRegisterSuspendResumeNotification,
    PowerUnregisterSuspendResumeNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC};

use crate::reactor::{OsEvent, Poster};

/// Keeps the suspend/resume notification registered; unregisters on drop.
pub struct PowerGuard {
    handle: HPOWERNOTIFY,
    /// The subscription passed at registration, kept alive with the registration.
    _params: Box<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>,
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `PowerRegisterSuspendResumeNotification`. The callback
        // context is leaked, so a callback still running during or after this call is sound.
        let _ = unsafe { PowerUnregisterSuspendResumeNotification(self.handle) };
    }
}

/// Posts `Resume` on every automatic resume from sleep until the guard drops.
///
/// Each successful registration leaks its `Poster`, the callback context, on purpose.
/// `PowerUnregisterSuspendResumeNotification` is not documented to wait for a running
/// callback, so the context must outlive the registration. Once the reactor is gone, the
/// leaked `Poster` only drops events.
pub fn register_power(poster: Poster) -> windows::core::Result<PowerGuard> {
    let context = Box::into_raw(Box::new(poster));
    let params = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power),
        Context: context.cast(),
    });
    let mut handle = ptr::null_mut();
    // SAFETY: with `DEVICE_NOTIFY_CALLBACK` the recipient is a pointer to the subscription
    // parameters, which live as long as the guard; the context they carry is never freed
    // once registered.
    let registered = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(
                ptr::from_ref::<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>(&params)
                    .cast_mut()
                    .cast(),
            ),
            &mut handle,
        )
    }
    .ok();
    if let Err(error) = registered {
        // SAFETY: registration failed, so no callback can see the context; it is still ours.
        drop(unsafe { Box::from_raw(context) });
        return Err(error);
    }
    Ok(PowerGuard {
        handle: HPOWERNOTIFY(handle as isize),
        _params: params,
    })
}

/// `PDEVICE_NOTIFY_CALLBACK_ROUTINE`: posts `Resume` for `PBT_APMRESUMEAUTOMATIC` and ignores
/// every other event. `context` is the leaked `Poster` of the registration.
unsafe extern "system" fn on_power(
    context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    if kind == PBT_APMRESUMEAUTOMATIC {
        // SAFETY: the context is the registration's leaked poster, never freed.
        unsafe { &*context.cast::<Poster>() }.post(OsEvent::Resume);
    }
    ERROR_SUCCESS.0
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows::Win32::UI::WindowsAndMessaging::{PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND};

    use crate::reactor::{OsEvent, Reactor};

    #[test]
    fn automatic_resume_is_posted() {
        let mut reactor = Reactor::new().unwrap();
        let poster = reactor.poster();
        let context = std::ptr::from_ref(&poster).cast();
        // SAFETY: the context is a live `Poster`, as `register_power` passes it.
        unsafe {
            on_power(context, PBT_APMSUSPEND, std::ptr::null());
            on_power(context, PBT_APMRESUMEAUTOMATIC, std::ptr::null());
        }
        assert_eq!(reactor.wait(Some(0), 0), vec![OsEvent::Resume]);
    }

    #[test]
    fn registers_and_unregisters() {
        let reactor = Reactor::new().unwrap();
        let guard = register_power(reactor.poster()).unwrap();
        drop(guard);
    }
}
