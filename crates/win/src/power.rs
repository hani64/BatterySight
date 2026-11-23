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
    /// The callback context; freed only after unregistering.
    _poster: Box<Poster>,
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `PowerRegisterSuspendResumeNotification`.
        let _ = unsafe { PowerUnregisterSuspendResumeNotification(self.handle) };
    }
}

/// Posts `Resume` on every automatic resume from sleep until the guard drops.
pub fn register_power(poster: Poster) -> windows::core::Result<PowerGuard> {
    let poster = Box::new(poster);
    let params = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power),
        Context: ptr::from_ref::<Poster>(&poster).cast_mut().cast(),
    });
    let mut handle = ptr::null_mut();
    // SAFETY: with `DEVICE_NOTIFY_CALLBACK` the recipient is a pointer to the subscription
    // parameters; they and the poster they point to live as long as the guard.
    unsafe {
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
    .ok()?;
    Ok(PowerGuard {
        handle: HPOWERNOTIFY(handle as isize),
        _params: params,
        _poster: poster,
    })
}

/// `PDEVICE_NOTIFY_CALLBACK_ROUTINE`: posts `Resume` for `PBT_APMRESUMEAUTOMATIC` and ignores
/// every other event. `context` is the guard's `Poster`.
unsafe extern "system" fn on_power(
    context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    if kind == PBT_APMRESUMEAUTOMATIC {
        // SAFETY: the context is the poster boxed in the guard, alive while registered.
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
