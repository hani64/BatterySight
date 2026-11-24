//! Audio activity of the render endpoints, by container id.
//!
//! [`start`] runs a thread in the multithreaded apartment that watches the audio sessions of
//! every active render endpoint and posts [`OsEvent::AudioActive`] when the endpoints of a
//! container (one physical device, whose HID interfaces share the container id) get their
//! first active session or lose their last one. [`peak`] reads the current peak level of a
//! container's endpoints, for the one check made when a read is due. Nothing here talks to
//! a HID device or plays audio.

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use windows::Win32::Devices::Properties::DEVPKEY_Device_ContainerId;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive,
    AudioSessionStateExpired, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
    IAudioSessionControl, IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDevice, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Variant::VT_CLSID;
use windows::core::{BOOL, GUID, PCWSTR, Ref, Result, implement};

use crate::hid_enum::{ContainerId, container_id};
use crate::reactor::{OsEvent, Poster};

/// The calling thread's membership in the multithreaded apartment, left on drop. Every COM
/// object the thread holds must be released first.
pub(crate) struct Mta(PhantomData<*const ()>);

impl Mta {
    pub(crate) fn enter() -> Result<Mta> {
        // SAFETY: no reserved argument; balanced by `CoUninitialize` on drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Mta(PhantomData))
    }
}

impl Drop for Mta {
    fn drop(&mut self) {
        // SAFETY: this thread entered the apartment in `enter`.
        unsafe { CoUninitialize() };
    }
}

/// Stops the audio thread when dropped, without waiting for it.
pub struct AudioGuard(Sender<Message>);

impl Drop for AudioGuard {
    fn drop(&mut self) {
        let _ = self.0.send(Message::Stop);
    }
}

/// Starts the audio thread, which posts [`OsEvent::AudioActive`] to `poster`.
///
/// Returns without waiting for the thread to set up: the audio service may be slow to answer
/// (for example right after logon) and must not hold up the caller. A thread that cannot set
/// up posts [`OsEvent::AudioUnavailable`] and ends; the devices are then read as without
/// audio.
pub fn start(poster: Poster) -> Result<AudioGuard> {
    let (sender, messages) = mpsc::channel();
    let own = sender.clone();
    thread::Builder::new()
        .name("batterysight-audio".into())
        .spawn(move || {
            let _mta = match Mta::enter() {
                Ok(mta) => mta,
                Err(_) => return unavailable(&poster, "com"),
            };
            // Declared after `_mta`, so a session still queued is released inside the
            // apartment.
            let messages = messages;
            match Activity::new(own, poster.clone()) {
                Ok(mut activity) => activity.run(&messages),
                Err(stage) => unavailable(&poster, stage),
            }
        })?;
    Ok(AudioGuard(sender))
}

/// Tells the device thread that audio activity is not (fully) watched, and why.
fn unavailable(poster: &Poster, stage: &'static str) {
    poster.post(OsEvent::AudioUnavailable { stage });
}

/// The highest current peak level (0.0 to 1.0) of the active render endpoints of
/// `container`, or `None` when it has none or none can be metered. Local calls into the
/// audio service only; the calling thread must be in the multithreaded apartment ([`Mta`]).
pub fn peak(container: &ContainerId) -> Option<f32> {
    // SAFETY: creates an in-process object; no pointer arguments.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }.ok()?;
    render_endpoints(&enumerator)
        .ok()?
        .into_iter()
        .filter(|device| endpoint_container(device).as_ref() == Some(container))
        .filter_map(|device| {
            // SAFETY: `device` is a live endpoint; no activation parameters.
            let meter: IAudioMeterInformation =
                unsafe { device.Activate(CLSCTX_ALL, None) }.ok()?;
            // SAFETY: `meter` is live.
            unsafe { meter.GetPeakValue() }.ok()
        })
        .reduce(f32::max)
}

/// The active render endpoints; an endpoint that cannot be fetched is left out.
fn render_endpoints(enumerator: &IMMDeviceEnumerator) -> Result<Vec<IMMDevice>> {
    // SAFETY: `enumerator` is live.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }?;
    // SAFETY: `devices` is live; `Item` is called with indices below its count.
    let count = unsafe { devices.GetCount() }?;
    Ok((0..count)
        .filter_map(|index| unsafe { devices.Item(index) }.ok())
        .collect())
}

/// The container id of an audio endpoint's device, or `None` when it cannot be read.
fn endpoint_container(device: &IMMDevice) -> Option<ContainerId> {
    // The endpoint property store's PKEY_Device_ContainerId is DEVPKEY_Device_ContainerId.
    let key = PROPERTYKEY {
        fmtid: DEVPKEY_Device_ContainerId.fmtid,
        pid: DEVPKEY_Device_ContainerId.pid,
    };
    // SAFETY: `device` is live.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    // SAFETY: `key` outlives the call.
    let mut value = unsafe { store.GetValue(&key) }.ok()?;
    // SAFETY: `vt` says which union member is set; a `VT_CLSID` value points at a GUID the
    // variant owns until `PropVariantClear`.
    let container = unsafe {
        let inner = &value.Anonymous.Anonymous;
        let guid = inner.Anonymous.puuid;
        (inner.vt == VT_CLSID && !guid.is_null())
            .then(|| container_id(&*guid))
            .flatten()
    };
    // SAFETY: `value` came from `GetValue` and is cleared once.
    let _ = unsafe { PropVariantClear(&mut value) };
    container
}

/// What the callbacks and the [`AudioGuard`] tell the audio thread. Callbacks run on the
/// audio service's threads and only send one of these: all work happens on the audio thread.
enum Message {
    /// A session was created on an endpoint of `container` watched since `generation`.
    Created {
        generation: u64,
        container: ContainerId,
        session: Session,
    },
    /// Session `id` changed state.
    State {
        id: u64,
        state: SessionState,
    },
    /// Render endpoints appeared, went away or changed state.
    Endpoints,
    Stop,
}

/// A session handed over from a callback thread.
struct Session(IAudioSessionControl);

// SAFETY: the callback threads and the audio thread are all in the multithreaded apartment,
// where an interface pointer may be used from any thread.
unsafe impl Send for Session {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Active,
    Inactive,
    /// Expired or disconnected: it will never be active again.
    Gone,
}

impl From<AudioSessionState> for SessionState {
    fn from(state: AudioSessionState) -> Self {
        if state == AudioSessionStateActive {
            SessionState::Active
        } else if state == AudioSessionStateExpired {
            SessionState::Gone
        } else {
            SessionState::Inactive
        }
    }
}

/// Asks for a rescan of the endpoints when one appears, goes away or changes state.
#[implement(IMMNotificationClient)]
struct EndpointWatch(Sender<Message>);

impl IMMNotificationClient_Impl for EndpointWatch_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        let _ = self.0.send(Message::Endpoints);
        Ok(())
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        let _ = self.0.send(Message::Endpoints);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        let _ = self.0.send(Message::Endpoints);
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> Result<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

/// Hands each session created on one endpoint to the audio thread.
#[implement(IAudioSessionNotification)]
struct NewSessions {
    generation: u64,
    container: ContainerId,
    sender: Sender<Message>,
}

impl IAudioSessionNotification_Impl for NewSessions_Impl {
    fn OnSessionCreated(&self, session: Ref<IAudioSessionControl>) -> Result<()> {
        if let Some(session) = session.cloned() {
            let _ = self.sender.send(Message::Created {
                generation: self.generation,
                container: self.container,
                session: Session(session),
            });
        }
        Ok(())
    }
}

/// Reports one session's state changes to the audio thread.
#[implement(IAudioSessionEvents)]
struct SessionWatch {
    id: u64,
    sender: Sender<Message>,
}

impl SessionWatch_Impl {
    fn send(&self, state: SessionState) {
        let _ = self.sender.send(Message::State { id: self.id, state });
    }
}

impl IAudioSessionEvents_Impl for SessionWatch_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, _: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, state: AudioSessionState) -> Result<()> {
        self.send(state.into());
        Ok(())
    }

    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> Result<()> {
        self.send(SessionState::Gone);
        Ok(())
    }
}

/// A session being watched.
struct Tracked {
    id: u64,
    container: ContainerId,
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
    active: bool,
}

/// One endpoint's session manager and the notification registered with it.
struct Endpoint {
    manager: IAudioSessionManager2,
    notification: IAudioSessionNotification,
}

/// The audio thread's state: every watched endpoint and session, and the containers last
/// posted active. Its registrations are undone on drop.
struct Activity {
    sender: Sender<Message>,
    poster: Poster,
    enumerator: IMMDeviceEnumerator,
    endpoint_watch: IMMNotificationClient,
    /// Bumped on every rescan; sessions created on endpoints of an older one are ignored.
    generation: u64,
    endpoints: Vec<Endpoint>,
    sessions: Vec<Tracked>,
    next_id: u64,
    posted: BTreeSet<ContainerId>,
}

impl Activity {
    /// Sets up the endpoint notifications and watches the current endpoints; on failure,
    /// names the stage that failed.
    fn new(sender: Sender<Message>, poster: Poster) -> std::result::Result<Activity, &'static str> {
        // SAFETY: creates an in-process object; no pointer arguments.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| "enumerator")?;
        let endpoint_watch: IMMNotificationClient = EndpointWatch(sender.clone()).into();
        // SAFETY: both objects are live; the callback is unregistered on drop.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&endpoint_watch) }
            .map_err(|_| "notifications")?;
        let mut activity = Activity {
            sender,
            poster,
            enumerator,
            endpoint_watch,
            generation: 0,
            endpoints: Vec::new(),
            sessions: Vec::new(),
            next_id: 0,
            posted: BTreeSet::new(),
        };
        activity.rescan();
        Ok(activity)
    }

    fn run(&mut self, messages: &Receiver<Message>) {
        while let Ok(message) = messages.recv() {
            match message {
                Message::Created {
                    generation,
                    container,
                    session,
                } => {
                    if generation == self.generation {
                        self.track(container, session.0);
                        self.publish();
                    }
                }
                Message::State { id, state } => self.on_state(id, state),
                Message::Endpoints => self.rescan(),
                Message::Stop => return,
            }
        }
    }

    /// Watches the sessions of every active render endpoint afresh.
    fn rescan(&mut self) {
        self.release();
        self.generation += 1;
        let Ok(devices) = render_endpoints(&self.enumerator) else {
            unavailable(&self.poster, "endpoints");
            self.publish();
            return;
        };
        for device in devices {
            if let Some(container) = endpoint_container(&device)
                && self.watch(&device, container).is_err()
            {
                // Counts as silent until the next rescan.
                unavailable(&self.poster, "sessions");
            }
        }
        self.publish();
    }

    /// Registers for `device`'s new sessions and tracks its current ones. A session created
    /// meanwhile may be tracked twice; both copies follow the same session.
    fn watch(&mut self, device: &IMMDevice, container: ContainerId) -> Result<()> {
        // SAFETY: `device` is live; no activation parameters.
        let manager: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None) }?;
        let notification: IAudioSessionNotification = NewSessions {
            generation: self.generation,
            container,
            sender: self.sender.clone(),
        }
        .into();
        // SAFETY: both objects are live; the notification is unregistered in `release`.
        unsafe { manager.RegisterSessionNotification(&notification) }?;
        // Session notifications only start once the sessions have been enumerated.
        // SAFETY: `manager` is live.
        let sessions = unsafe { manager.GetSessionEnumerator() };
        self.endpoints.push(Endpoint {
            manager,
            notification,
        });
        let sessions = sessions?;
        // SAFETY: `sessions` is live; `GetSession` is called with indices below its count.
        for index in 0..unsafe { sessions.GetCount() }? {
            if let Ok(control) = unsafe { sessions.GetSession(index) } {
                self.track(container, control);
            }
        }
        Ok(())
    }

    fn track(&mut self, container: ContainerId, control: IAudioSessionControl) {
        let id = self.next_id;
        self.next_id += 1;
        let events: IAudioSessionEvents = SessionWatch {
            id,
            sender: self.sender.clone(),
        }
        .into();
        // SAFETY: both objects are live; the events are unregistered in `forget` or `release`.
        if unsafe { control.RegisterAudioSessionNotification(&events) }.is_err() {
            return;
        }
        // Read after registering, so no change between the two is missed.
        // SAFETY: `control` is live.
        let state = unsafe { control.GetState() }.map_or(SessionState::Inactive, Into::into);
        if state == SessionState::Gone {
            forget(&control, &events);
            return;
        }
        self.sessions.push(Tracked {
            id,
            container,
            control,
            events,
            active: state == SessionState::Active,
        });
    }

    fn on_state(&mut self, id: u64, state: SessionState) {
        let Some(index) = self.sessions.iter().position(|session| session.id == id) else {
            return;
        };
        match state {
            SessionState::Gone => {
                let gone = self.sessions.swap_remove(index);
                forget(&gone.control, &gone.events);
            }
            state => self.sessions[index].active = state == SessionState::Active,
        }
        self.publish();
    }

    /// Posts every container whose activity changed since the last post.
    fn publish(&mut self) {
        let active: BTreeSet<ContainerId> = self
            .sessions
            .iter()
            .filter(|session| session.active)
            .map(|session| session.container)
            .collect();
        for &container in active.difference(&self.posted) {
            self.poster.post(OsEvent::AudioActive {
                container,
                active: true,
            });
        }
        for &container in self.posted.difference(&active) {
            self.poster.post(OsEvent::AudioActive {
                container,
                active: false,
            });
        }
        self.posted = active;
    }

    /// Undoes every session and endpoint registration.
    fn release(&mut self) {
        for session in self.sessions.drain(..) {
            forget(&session.control, &session.events);
        }
        for endpoint in self.endpoints.drain(..) {
            // SAFETY: the notification was registered with this manager in `watch`.
            let _ = unsafe {
                endpoint
                    .manager
                    .UnregisterSessionNotification(&endpoint.notification)
            };
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.release();
        // SAFETY: the callback was registered with this enumerator in `new`.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.endpoint_watch)
        };
    }
}

/// Stops `events` from following `control`.
fn forget(control: &IAudioSessionControl, events: &IAudioSessionEvents) {
    // SAFETY: `events` was registered with `control` in `Activity::track`.
    let _ = unsafe { control.UnregisterAudioSessionNotification(events) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
    use windows::Win32::System::Variant::VT_LPWSTR;

    use crate::hid_enum::container_hex;

    /// The friendly name of an audio endpoint, or an empty string.
    fn endpoint_name(device: &IMMDevice) -> String {
        let key = PROPERTYKEY {
            fmtid: DEVPKEY_Device_FriendlyName.fmtid,
            pid: DEVPKEY_Device_FriendlyName.pid,
        };
        // SAFETY: `device` is live; the variant is read per its `vt` and cleared once.
        unsafe {
            let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
                return String::new();
            };
            let Ok(mut value) = store.GetValue(&key) else {
                return String::new();
            };
            let inner = &value.Anonymous.Anonymous;
            let name = if inner.vt == VT_LPWSTR {
                inner.Anonymous.pwszVal.to_string().unwrap_or_default()
            } else {
                String::new()
            };
            let _ = PropVariantClear(&mut value);
            name
        }
    }

    /// Manual check: `cargo test -p batterysight-win -- --ignored --nocapture print_render`.
    /// Read-only: lists the render endpoints, their containers and peak levels; plays nothing.
    #[test]
    #[ignore = "reads the audio endpoints of this machine"]
    fn print_render_endpoints() {
        let _mta = Mta::enter().unwrap();
        // SAFETY: creates an in-process object; no pointer arguments.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }.unwrap();
        for device in render_endpoints(&enumerator).unwrap() {
            let container = endpoint_container(&device);
            println!(
                "container {}  peak {:?}  {}",
                container.as_ref().map_or("-".into(), container_hex),
                container.as_ref().and_then(peak),
                endpoint_name(&device),
            );
        }
    }

    /// Manual check: `cargo test -p batterysight-win -- --ignored --nocapture print_audio`.
    /// Read-only: prints the audio thread's events for 20 s; start or stop playback meanwhile.
    #[test]
    #[ignore = "watches the audio sessions of this machine"]
    fn print_audio_activity() {
        use std::time::{Duration, Instant};

        use crate::reactor::Reactor;

        let mut reactor = Reactor::new().unwrap();
        let guard = start(reactor.poster()).unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            let left = end.saturating_duration_since(Instant::now()).as_millis() as u64;
            for event in reactor.wait(Some(left), 0) {
                match event {
                    OsEvent::AudioActive { container, active } => {
                        println!("{} {}", container_hex(&container), active);
                    }
                    OsEvent::AudioUnavailable { stage } => println!("unavailable {stage}"),
                    _ => {}
                }
            }
        }
        drop(guard);
    }
}
