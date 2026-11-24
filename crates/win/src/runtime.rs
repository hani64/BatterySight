//! Runs the hub on a device thread over the reactor.
//!
//! The device thread owns the [`Reactor`], the HID and power notification registrations and
//! the [`Hub`]; none of them leave it. Other threads see the devices through the
//! [`Snapshot`] and talk to the thread through a [`RuntimeHandle`].
//!
//! While audio plays to a physical device (its container has an active audio session and a
//! peak level of at least -60 dBFS), the device thread hands the hub an activity input for
//! each of that device's registered devices as soon as the scheduler would grant one (for
//! the PRO X Wireless, 10 minutes after its last read by default, 5 after a wake). Devices
//! the scheduler would refuse (asleep, or pushing their battery) are not metered. Without an
//! active session it sets no deadline for this, so a silent PC never wakes the thread.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use batterysight_core::catalog::Catalog;
use batterysight_core::engines;
use batterysight_core::hid::{CollectionId, GroupId, HidGroup};
use batterysight_core::hub::{Hub, HubInput, HubOutput};
use batterysight_core::model::{DeviceKey, DeviceState};
use batterysight_core::scheduler::Policy;
use batterysight_core::time::Ms;
use windows::Win32::Foundation::E_UNEXPECTED;
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::audio::{self, Mta};
use crate::hid_enum::{self, ContainerId, HidInterface};
use crate::notify::register_hid_notifications;
use crate::power::register_power;
use crate::reactor::{OsEvent, Poster, Reactor};
use crate::trace::{Direction, Trace};

/// How long [`RuntimeHandle::stop`] waits for the device thread to finish.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Every known device as of the last change, ordered by key.
pub struct Snapshot(pub Arc<Mutex<Vec<DeviceState>>>);

/// Controls the device thread started by [`start`]. Dropping it stops the thread without
/// waiting.
pub struct RuntimeHandle {
    pub snapshot: Snapshot,
    poster: Poster,
    commands: Sender<Command>,
    /// Disconnects once the device thread has released everything.
    finished: Receiver<Infallible>,
    thread: Option<JoinHandle<()>>,
}

enum Command {
    RefreshNow,
    SetIntervals(HashMap<DeviceKey, Ms>),
    Stop,
}

impl RuntimeHandle {
    /// Asks for a fresh reading of every device, as far as the scheduler allows.
    pub fn refresh_now(&self) {
        self.command(Command::RefreshNow);
    }

    /// Sets the user's gaps between battery reads while a device is in use (key → ms),
    /// replacing earlier ones; the device's own minimum still applies.
    pub fn set_intervals(&self, gaps: HashMap<DeviceKey, Ms>) {
        self.command(Command::SetIntervals(gaps));
    }

    /// Stops the device thread and waits up to 2 s for it to close the devices.
    pub fn stop(mut self) {
        self.command(Command::Stop);
        if self.finished.recv_timeout(STOP_TIMEOUT) == Err(RecvTimeoutError::Disconnected)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }

    fn command(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            self.poster.post(OsEvent::Wake);
        }
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        self.command(Command::Stop);
    }
}

/// Starts the device thread: opens every HID group an engine wants, feeds the hub and runs
/// its sends. `on_change` is called on the device thread after each snapshot change.
pub fn start(on_change: Box<dyn Fn() + Send>) -> windows::core::Result<RuntimeHandle> {
    let states = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&states);
    let (commands, received) = mpsc::channel();
    let (finished_tx, finished) = mpsc::channel::<Infallible>();
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("batterysight-device".into())
        .spawn(move || {
            device_thread(&ready_tx, &received, shared, on_change);
            drop(finished_tx);
        })?;
    let poster = ready
        .recv()
        .map_err(|_| windows::core::Error::from_hresult(E_UNEXPECTED))??;
    Ok(RuntimeHandle {
        snapshot: Snapshot(states),
        poster,
        commands,
        finished,
        thread: Some(thread),
    })
}

/// Enters the multithreaded apartment, sets up the reactor and the notifications, reports
/// the outcome on `ready`, starts watching audio activity, then runs until told to stop.
/// Everything is released before it returns.
fn device_thread(
    ready: &SyncSender<windows::core::Result<Poster>>,
    commands: &Receiver<Command>,
    states: Arc<Mutex<Vec<DeviceState>>>,
    on_change: Box<dyn Fn() + Send>,
) {
    // Left last, after every COM object of this thread (`audio::peak` creates them) is gone.
    let _mta = match Mta::enter() {
        Ok(mta) => mta,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let setup = Reactor::new().and_then(|reactor| {
        // Registered before the first enumeration so that no arrival is missed.
        let notify = register_hid_notifications(reactor.poster())?;
        let power = register_power(reactor.poster())?;
        Ok((reactor, notify, power))
    });
    let (reactor, _notify, _power) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(reactor.poster()));
    // Without it no device is read on audio activity; every other trigger still works.
    let _audio = audio::start(reactor.poster())
        .inspect_err(|_| {
            reactor
                .poster()
                .post(OsEvent::AudioUnavailable { stage: "thread" });
        })
        .ok();
    Device {
        hub: Hub::new(Catalog::builtin(), Policy::default()),
        reactor,
        groups: Vec::new(),
        products: HashMap::new(),
        guard: ReadFailureGuard::default(),
        release_at: None,
        held: BTreeMap::new(),
        audio: AudioActivity::default(),
        trace: Trace::from_env(),
        states,
        on_change,
    }
    .run(commands);
}

/// Monotonic milliseconds since boot.
fn now_ms() -> Ms {
    // SAFETY: no arguments, no preconditions.
    unsafe { GetTickCount64() }
}

/// The lowercased interface paths of one group: its identity across enumerations, which
/// re-mint every `CollectionId` and `GroupId`.
type PathSet = BTreeSet<String>;

/// A group opened in the reactor and handed to the hub.
struct Opened {
    id: GroupId,
    paths: PathSet,
    /// The collections opened, as the hub knows them.
    collections: Vec<CollectionId>,
    /// The physical device's container id, which its audio endpoints share.
    container: Option<ContainerId>,
}

/// The device thread's state.
struct Device {
    hub: Hub,
    reactor: Reactor,
    groups: Vec<Opened>,
    /// Product strings by lowercased path; each is read from the device until a read succeeds.
    products: HashMap<String, String>,
    /// Keeps a group closed after back-to-back read failures until the next arrival, resume
    /// or refresh, and holds back one reopened too often in the last minute.
    guard: ReadFailureGuard,
    /// When the first present group the reopen cap holds back may open; the device thread
    /// rescans once then.
    release_at: Option<Ms>,
    /// The present groups the reopen cap held back at the last scan, with their release.
    held: BTreeMap<PathSet, Ms>,
    audio: AudioActivity,
    trace: Option<Trace>,
    states: Arc<Mutex<Vec<DeviceState>>>,
    on_change: Box<dyn Fn() + Send>,
}

/// What the device thread knows about audio playing to the devices.
#[derive(Default)]
struct AudioActivity {
    /// Containers whose render endpoints have an active audio session.
    active: BTreeSet<ContainerId>,
    /// Active containers found silent, and when to look at them again.
    silent: HashMap<ContainerId, Silence>,
}

/// An active container found silent at its last check.
struct Silence {
    /// When to look at it again.
    until: Ms,
    /// How long it waits after that check; doubles up to [`SILENT_RECHECK_MAX_MS`].
    wait: Ms,
}

impl AudioActivity {
    /// Records whether `container` has an active audio session.
    fn set(&mut self, container: ContainerId, active: bool) {
        if active {
            self.active.insert(container);
        } else {
            self.active.remove(&container);
            self.silent.remove(&container);
        }
    }

    /// `container` was silent at `now`: looked at again [`SILENT_RECHECK_MS`] later, twice
    /// as late after each further silent check, at most [`SILENT_RECHECK_MAX_MS`] later.
    fn silent_at(&mut self, container: ContainerId, now: Ms) {
        let wait = self
            .silent
            .get(&container)
            .map_or(SILENT_RECHECK_MS, |silence| {
                silence.wait.saturating_mul(2).min(SILENT_RECHECK_MAX_MS)
            });
        let until = now.saturating_add(wait);
        self.silent.insert(container, Silence { until, wait });
    }
}

impl Device {
    fn run(mut self, commands: &Receiver<Command>) {
        self.rescan();
        loop {
            let now = now_ms();
            if self
                .hub
                .next_deadline()
                .is_some_and(|deadline| now >= deadline)
            {
                self.feed(HubInput::Tick, now);
            }
            if self.release_due(now) {
                self.rescan();
            }
            if self.activity_deadline().is_some_and(|at| now >= at) {
                self.check_activity(now, audio::peak);
            }
            // May return nothing before the deadline; the loop then checks it again.
            let events = self.reactor.wait(self.deadline(), now_ms());
            let mut rescan = false;
            for event in events {
                rescan |= self.on_event(event);
            }
            if rescan {
                self.rescan();
            }
            loop {
                match commands.try_recv() {
                    Ok(Command::RefreshNow) => {
                        // A refresh also reopens groups the read-failure guard closed.
                        self.guard.clear();
                        self.rescan();
                        self.feed(HubInput::RefreshNow, now_ms());
                    }
                    Ok(Command::SetIntervals(gaps)) => {
                        self.feed(HubInput::SetIntervals(gaps), now_ms());
                    }
                    Ok(Command::Stop) | Err(TryRecvError::Disconnected) => return,
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
    }

    /// Handles one event; returns whether the HID groups must be enumerated again.
    fn on_event(&mut self, event: OsEvent) -> bool {
        match event {
            OsEvent::Report { collection, bytes } => {
                if let Some(trace) = &mut self.trace {
                    trace.record(Direction::In, collection, &bytes);
                }
                if let Some(group) = self
                    .groups
                    .iter()
                    .find(|group| group.collections.contains(&collection))
                {
                    self.guard.on_report(&group.paths);
                }
                self.feed(HubInput::Report { collection, bytes }, now_ms());
                false
            }
            OsEvent::ReadFailed { collection } => {
                // A handle gone stale (for example across sleep) comes back as a new group.
                let failed = self
                    .groups
                    .iter()
                    .position(|group| group.collections.contains(&collection));
                failed.is_some_and(|index| {
                    let gone = self.remove(index);
                    self.guard.on_failure(gone.paths, now_ms()) == AfterFailure::Reopen
                })
            }
            OsEvent::InterfaceRemoved { path } => {
                let path = path_key(&path);
                let removed = self
                    .groups
                    .iter()
                    .position(|group| group.paths.contains(&path));
                removed.is_some_and(|index| {
                    let gone = self.remove(index);
                    self.guard.on_removed(gone.paths, now_ms());
                    true
                })
            }
            OsEvent::InterfaceArrived => {
                self.guard.on_arrival(now_ms());
                true
            }
            OsEvent::Resume => {
                // Handles stay open: a device asleep across the suspend keeps its entry.
                self.guard.clear();
                self.rescan();
                self.feed(HubInput::Resume, now_ms());
                false
            }
            OsEvent::AudioActive { container, active } => {
                if let Some(trace) = &mut self.trace {
                    trace.audio(&container, active);
                }
                self.audio.set(container, active);
                false
            }
            OsEvent::AudioUnavailable { stage } => {
                if let Some(trace) = &mut self.trace {
                    trace.audio_unavailable(stage);
                }
                false
            }
            OsEvent::WriteFailed { .. } | OsEvent::Wake => false,
        }
    }

    /// Enumerates the HID groups engines want, closes the opened ones no longer present
    /// and opens the new ones the read-failure guard admits.
    fn rescan(&mut self) {
        self.apply_scan(hid_enum::enumerate(), now_ms());
    }

    /// When the device thread must next wake: the hub's deadline, a held-back group's
    /// release or an activity check, whichever comes first.
    fn deadline(&self) -> Option<Ms> {
        [
            self.hub.next_deadline(),
            self.release_at,
            self.activity_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether a held-back group's release is due at `now`; if so, the release is spent.
    fn release_due(&mut self, now: Ms) -> bool {
        let due = self.release_at.is_some_and(|at| now >= at);
        if due {
            self.release_at = None;
        }
        due
    }

    /// Every registered device of an opened group whose container has an active audio
    /// session and that the scheduler could grant an activity read, with that container and
    /// when the scheduler could grant it ([`Hub::activity_ready_at`]).
    fn activity_candidates(&self) -> Vec<(ContainerId, DeviceKey, Ms)> {
        let mut candidates = Vec::new();
        if self.audio.active.is_empty() {
            return candidates;
        }
        for group in &self.groups {
            let Some(container) = group.container else {
                continue;
            };
            if !self.audio.active.contains(&container) {
                continue;
            }
            for key in self.hub.keys_of(group.id) {
                if let Some(ready) = self.hub.activity_ready_at(&key) {
                    candidates.push((container, key, ready));
                }
            }
        }
        candidates
    }

    /// When a device of `container` that the scheduler could grant at `ready` may next be
    /// checked: not while its container is known to be silent.
    fn activity_check_at(&self, container: &ContainerId, ready: Ms) -> Ms {
        let silent_until = self
            .audio
            .silent
            .get(container)
            .map(|silence| silence.until);
        ready.max(silent_until.unwrap_or(0))
    }

    /// When an activity check is next due. `None` while no device the scheduler could grant
    /// an activity read has an active audio session: silence, an asleep device or one that
    /// pushes its battery never wakes the device thread.
    fn activity_deadline(&self) -> Option<Ms> {
        self.activity_candidates()
            .iter()
            .map(|(container, _, ready)| self.activity_check_at(container, *ready))
            .min()
    }

    /// Reads the `peak` of each container with a device due at `now`, once, and hands
    /// [`HubInput::Activity`] to its due devices if it plays something. A silent container
    /// is looked at again later ([`AudioActivity::silent_at`]); an audible one starts over.
    fn check_activity(&mut self, now: Ms, peak: impl Fn(&ContainerId) -> Option<f32>) {
        let due: Vec<(ContainerId, DeviceKey)> = self
            .activity_candidates()
            .into_iter()
            .filter(|(container, _, ready)| now >= self.activity_check_at(container, *ready))
            .map(|(container, key, _)| (container, key))
            .collect();
        let mut containers: Vec<ContainerId> =
            due.iter().map(|(container, _)| *container).collect();
        containers.sort_unstable();
        containers.dedup();
        for container in containers {
            let level = peak(&container);
            if audible(level) {
                self.audio.silent.remove(&container);
            } else {
                self.audio.silent_at(container, now);
            }
            for (_, key) in due.iter().filter(|(of, _)| *of == container) {
                // An earlier input of this pass may have changed what the scheduler allows.
                let ready = self.hub.activity_ready_at(key);
                if activity_due(ready, now, self.audio.active.contains(&container), level) {
                    self.feed(HubInput::Activity(key.clone()), now);
                }
            }
        }
    }

    /// [`Device::rescan`] on the interfaces `scanned` at `now`. `None` (the enumeration
    /// failed) says nothing about which devices are present, so every group stays as it is.
    fn apply_scan(&mut self, scanned: Option<Vec<HidInterface>>, now: Ms) {
        let Some(scanned) = scanned else {
            return;
        };
        let scanned: Vec<(HidGroup, Vec<HidInterface>, PathSet)> = hid_enum::group(scanned)
            .into_iter()
            .filter(|(group, _)| engines::wants(group))
            .map(|(group, members)| {
                let paths = members.iter().map(|m| path_key(&m.path)).collect();
                (group, members, paths)
            })
            .collect();
        let known: Vec<&PathSet> = self.groups.iter().map(|group| &group.paths).collect();
        let found: Vec<&PathSet> = scanned.iter().map(|(_, _, paths)| paths).collect();
        let gone = unmatched(&known, &found);
        let new = unmatched(&found, &known);
        for &index in gone.iter().rev() {
            let gone = self.remove(index);
            self.guard.on_removed(gone.paths, now);
        }
        self.release_at = None;
        let mut held = BTreeMap::new();
        for (index, (group, members, paths)) in scanned.into_iter().enumerate() {
            if !new.contains(&index) {
                continue;
            }
            match self.guard.admission(&paths, now) {
                Admission::Open => self.arrive(group, members, paths, now),
                Admission::Closed => {}
                Admission::HeldUntil(at) => {
                    if self.held.get(&paths) != Some(&at)
                        && let Some(trace) = &mut self.trace
                    {
                        trace.held(&paths, at.saturating_sub(now));
                    }
                    self.release_at = Some(self.release_at.map_or(at, |release| release.min(at)));
                    held.insert(paths, at);
                }
            }
        }
        self.held = held;
    }

    /// Fills in the product strings of the collections an engine reads, opens those and
    /// hands them to the hub. The group's other collections (such as a headset's consumer
    /// control) are never opened, so their reports never reach the process. Traces an open
    /// that failed, and the release of a group the last scan held back.
    fn arrive(&mut self, mut group: HidGroup, members: Vec<HidInterface>, paths: PathSet, now: Ms) {
        let container = members.iter().find_map(|member| member.container);
        let mut opened = Vec::new();
        for mut member in members
            .into_iter()
            .filter(|member| engines::reads(&member.collection))
        {
            member.collection.product =
                cached_product(&mut self.products, path_key(&member.path), || {
                    hid_enum::product_string(&member.path)
                });
            if self.reactor.open(group.id, &member).is_ok() {
                opened.push(member.collection);
            }
        }
        group.collections = opened;
        if !engines::wants(&group) {
            // None of its HID++ collections opened; the next rescan tries again.
            self.reactor.close_group(group.id);
            if let Some(trace) = &mut self.trace {
                trace.open_failed(&paths);
            }
            return;
        }
        if self.held.contains_key(&paths)
            && let Some(trace) = &mut self.trace
        {
            trace.released(&paths);
        }
        self.guard.on_open(&paths, now);
        self.groups.push(Opened {
            id: group.id,
            paths,
            collections: group.collections.iter().map(|c| c.id).collect(),
            container,
        });
        self.feed(HubInput::GroupArrived(group), now_ms());
    }

    /// Closes the group at `index` and tells the hub.
    fn remove(&mut self, index: usize) -> Opened {
        let gone = self.groups.remove(index);
        self.reactor.close_group(gone.id);
        self.feed(HubInput::GroupRemoved(gone.id), now_ms());
        gone
    }

    /// Hands `input` to the hub, runs its sends and publishes a changed snapshot.
    fn feed(&mut self, input: HubInput, now: Ms) {
        let mut changed = false;
        for output in self.hub.handle(input, now) {
            match output {
                HubOutput::Send { collection, bytes } => {
                    if self.reactor.write(collection, &bytes)
                        && let Some(trace) = &mut self.trace
                    {
                        trace.record(Direction::Out, collection, &bytes);
                    }
                }
                HubOutput::StateChanged => changed = true,
                HubOutput::DiscoveryFailed { group, retry_at } => {
                    if let Some(trace) = &mut self.trace {
                        trace.discovery_failed(&group, retry_at.map(|at| at.saturating_sub(now)));
                    }
                }
            }
        }
        if changed {
            let snapshot = self.hub.snapshot();
            *self.states.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
            (self.on_change)();
        }
    }
}

/// `path` up to its first NUL, lowercased, so paths compare case-insensitively.
fn path_key(path: &[u16]) -> String {
    let end = path
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(path.len());
    String::from_utf16_lossy(&path[..end]).to_lowercase()
}

/// The product string cached under `key`, read with `read` when missing. An empty string
/// (the read failed) is not cached: HID++ receiver detection depends on the product
/// string, so the next arrival asks again.
fn cached_product(
    products: &mut HashMap<String, String>,
    key: String,
    read: impl FnOnce() -> String,
) -> String {
    if let Some(product) = products.get(&key) {
        return product.clone();
    }
    let product = read();
    if !product.is_empty() {
        products.insert(key, product.clone());
    }
    product
}

/// Indices of the sets in `sets` that `others` does not contain.
fn unmatched(sets: &[&PathSet], others: &[&PathSet]) -> Vec<usize> {
    sets.iter()
        .enumerate()
        .filter(|(_, set)| !others.contains(set))
        .map(|(index, _)| index)
        .collect()
}

/// How long an active container found silent first waits before it is looked at again: a
/// pause between two tracks must not cost a whole activity gap.
const SILENT_RECHECK_MS: Ms = 15_000;

/// The longest wait between two looks at an active but silent container, such as one whose
/// session a paused app or a voice chat keeps open: the PRO X Wireless activity gap.
const SILENT_RECHECK_MAX_MS: Ms = 300_000;

/// The lowest peak level (linear, full scale 1.0) that counts as audio playing: -60 dBFS.
/// A paused player can keep its stream open and feed it digital near-silence; in the owner's
/// hardware session a paused headset endpoint metered about 1.6e-6 (-115 dBFS), and `> 0.0`
/// counted that as playing, so the headset was read every 5 minutes while paused.
const AUDIBLE_PEAK: f32 = 0.001;

/// Whether a metered `peak` (`None` when it could not be metered) means audio is playing.
fn audible(peak: Option<f32>) -> bool {
    peak.is_some_and(|level| level >= AUDIBLE_PEAK)
}

/// Whether a device may be handed an activity input at `now`: its container has an active
/// audio session (`active`), its endpoints play something ([`audible`] `peak`) and the
/// scheduler would grant it (`ready_at` reached; `None` while it refuses any, the device
/// being asleep or pushing its battery).
fn activity_due(ready_at: Option<Ms>, now: Ms, active: bool, peak: Option<f32>) -> bool {
    active && audible(peak) && ready_at.is_some_and(|at| now >= at)
}

/// What to do with a group whose read just failed.
#[derive(Debug, PartialEq, Eq)]
enum AfterFailure {
    /// Re-enumerate; the group is reopened if still present and admitted.
    Reopen,
    /// Leave the group closed until the next arrival, resume or refresh; the reopen cap
    /// may hold it back longer.
    StayClosed,
}

/// Whether a rescan may open a group now.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Open,
    /// Closed for failing until the next arrival, resume or refresh.
    Closed,
    /// Held back by the reopen cap until this time, or until a resume or refresh.
    HeldUntil(Ms),
}

/// The most times [`ReadFailureGuard`] lets one group be reopened within [`REOPEN_WINDOW`].
const REOPEN_LIMIT: usize = 3;

/// The sliding window, in milliseconds, over which [`REOPEN_LIMIT`] applies.
const REOPEN_WINDOW: Ms = 60_000;

/// Whether `at` lies within the [`REOPEN_WINDOW`] ending at `now`.
fn in_window(at: Ms, now: Ms) -> bool {
    now.saturating_sub(at) < REOPEN_WINDOW
}

/// Decides per group, keyed by path set, whether a rescan may open a group that failed.
///
/// - A read failure reopens the group unless the previous failure of the same group was
///   not followed by a report from it, or the group was already reopened [`REOPEN_LIMIT`]
///   times within the last [`REOPEN_WINDOW`]; then the group stays closed until the next
///   arrival ([`on_arrival`]), resume or refresh ([`clear`]).
/// - Removing an open group counts as a failure for the window, though not towards the
///   back-to-back rule, so the cap does not depend on whether a device's removal or its
///   read failure is handled first.
/// - Every open of a group that failed or was removed within the window counts as a
///   reopen ([`on_open`]), whatever caused it. While the window holds [`REOPEN_LIMIT`]
///   reopens, the group is held back ([`admission`]) until the oldest of them leaves the
///   window, when the device thread rescans once, or until a resume or refresh. An arrival
///   does not lift this, so a device that re-enumerates after each failure is not reopened
///   in a loop, and one replugged more than [`REOPEN_LIMIT`] times a minute waits for the
///   window.
/// - A group with no failure or removal in the window is always opened.
///
/// [`on_arrival`]: ReadFailureGuard::on_arrival
/// [`clear`]: ReadFailureGuard::clear
/// [`on_open`]: ReadFailureGuard::on_open
/// [`admission`]: ReadFailureGuard::admission
#[derive(Default)]
struct ReadFailureGuard {
    groups: HashMap<PathSet, Failures>,
}

/// One group's failures and reopens since the guard was last cleared.
#[derive(Default)]
struct Failures {
    /// When its read last failed or it was last removed while open.
    failed_at: Ms,
    /// Its read failed and it reported nothing since.
    pending: bool,
    /// Kept closed until the next arrival, resume or refresh.
    closed: bool,
    /// When it was reopened, oldest first; those outside the window are pruned lazily.
    reopens: Vec<Ms>,
}

impl Failures {
    /// The reopens within the window ending at `now`.
    fn recent_reopens(&self, now: Ms) -> &[Ms] {
        let start = self
            .reopens
            .iter()
            .position(|&at| in_window(at, now))
            .unwrap_or(self.reopens.len());
        &self.reopens[start..]
    }
}

impl ReadFailureGuard {
    /// The group read normally: its next failure counts as the first again.
    fn on_report(&mut self, paths: &PathSet) {
        if let Some(failures) = self.groups.get_mut(paths) {
            failures.pending = false;
        }
    }

    fn on_failure(&mut self, paths: PathSet, now: Ms) -> AfterFailure {
        let failures = self.groups.entry(paths).or_default();
        failures.failed_at = now;
        let back_to_back = std::mem::replace(&mut failures.pending, true);
        if back_to_back || failures.recent_reopens(now).len() >= REOPEN_LIMIT {
            failures.closed = true;
            AfterFailure::StayClosed
        } else {
            AfterFailure::Reopen
        }
    }

    /// An open group was removed.
    fn on_removed(&mut self, paths: PathSet, now: Ms) {
        self.groups.entry(paths).or_default().failed_at = now;
    }

    fn admission(&self, paths: &PathSet, now: Ms) -> Admission {
        let Some(failures) = self.groups.get(paths) else {
            return Admission::Open;
        };
        if failures.closed {
            return Admission::Closed;
        }
        let recent = failures.recent_reopens(now);
        match recent.len().checked_sub(REOPEN_LIMIT) {
            Some(index) => Admission::HeldUntil(recent[index] + REOPEN_WINDOW),
            None => Admission::Open,
        }
    }

    /// The group was opened: a reopen if it failed or was removed within the window.
    fn on_open(&mut self, paths: &PathSet, now: Ms) {
        if let Some(failures) = self.groups.get_mut(paths)
            && in_window(failures.failed_at, now)
        {
            failures.reopens.retain(|&at| in_window(at, now));
            failures.reopens.push(now);
        }
    }

    /// An interface arrived: groups closed for failing may be opened again, except those
    /// the reopen cap holds back. Forgets groups with nothing left in the window.
    fn on_arrival(&mut self, now: Ms) {
        self.groups.retain(|_, failures| {
            failures.pending = false;
            failures.closed = false;
            in_window(failures.failed_at, now)
                || failures
                    .reopens
                    .last()
                    .is_some_and(|&at| in_window(at, now))
        });
    }

    /// Forgets every failure and reopen: every group may be opened.
    fn clear(&mut self) {
        self.groups.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use batterysight_core::engine::{Action, Engine};
    use batterysight_core::hid::Collection;
    use batterysight_core::model::{DeviceUpdate, Presence, Source};
    use batterysight_core::scheduler::{Capabilities, DEFAULT_INTERVAL_MS, Grant, Trigger};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;

    use crate::reactor::tests::Server;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn set(paths: &[&str]) -> PathSet {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn path_key_lowercases_and_stops_at_nul() {
        let path = r"\\?\HID#VID_046D&PID_C547&MI_02&Col01#8&2A2B&0&0000#{4D1E}";
        assert_eq!(
            path_key(&wide(path)),
            r"\\?\hid#vid_046d&pid_c547&mi_02&col01#8&2a2b&0&0000#{4d1e}"
        );
        let unterminated: Vec<u16> = "AbC".encode_utf16().collect();
        assert_eq!(path_key(&unterminated), "abc");
    }

    #[test]
    fn same_paths_in_any_order_match() {
        let known = set(&["a", "b"]);
        let scanned = set(&["b", "a"]);
        assert!(unmatched(&[&known], &[&scanned]).is_empty());
        assert!(unmatched(&[&scanned], &[&known]).is_empty());
    }

    #[test]
    fn changed_groups_are_gone_and_new() {
        let receiver = set(&["r1", "r2"]);
        let headset = set(&["h"]);
        let receiver_less = set(&["r1"]);
        let mouse = set(&["m"]);
        let known = [&receiver, &headset];
        let scanned = [&mouse, &headset, &receiver_less];
        assert_eq!(unmatched(&known, &scanned), [0]);
        assert_eq!(unmatched(&scanned, &known), [0, 2]);
    }

    #[test]
    fn nothing_known_means_all_new() {
        let a = set(&["a"]);
        assert_eq!(unmatched(&[&a], &[]), [0]);
        assert!(unmatched(&[], &[&a]).is_empty());
    }

    /// A rescan opens `group` at `at`.
    fn reopen(guard: &mut ReadFailureGuard, group: &PathSet, at: Ms) {
        assert_eq!(
            guard.admission(group, at),
            Admission::Open,
            "refused at {at}"
        );
        guard.on_open(group, at);
    }

    /// A read of `group` fails at `at`, the rescan reopens it and it reads normally again.
    fn fail_and_reopen(guard: &mut ReadFailureGuard, group: &PathSet, at: Ms) {
        assert_eq!(guard.on_failure(group.clone(), at), AfterFailure::Reopen);
        reopen(guard, group, at);
        guard.on_report(group);
    }

    #[test]
    fn failures_separated_by_a_report_reopen() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        fail_and_reopen(&mut guard, &group, 0);
        fail_and_reopen(&mut guard, &group, 1);
    }

    #[test]
    fn back_to_back_failures_stay_closed() {
        let group = set(&["a"]);
        let other = set(&["b"]);
        let mut guard = ReadFailureGuard::default();
        assert_eq!(guard.on_failure(group.clone(), 0), AfterFailure::Reopen);
        guard.on_report(&other);
        assert_eq!(guard.on_failure(group.clone(), 1), AfterFailure::StayClosed);
        assert_eq!(guard.admission(&group, 1), Admission::Closed);
        assert_eq!(guard.admission(&other, 1), Admission::Open);
    }

    #[test]
    fn clear_allows_reopening() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        guard.on_failure(group.clone(), 0);
        guard.on_failure(group.clone(), 1);
        guard.clear();
        fail_and_reopen(&mut guard, &group, 2);
    }

    #[test]
    fn reopens_capped_per_minute() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 2_000, 4_000] {
            fail_and_reopen(&mut guard, &group, at);
        }
        assert_eq!(
            guard.on_failure(group.clone(), 6_000),
            AfterFailure::StayClosed
        );
        assert_eq!(guard.admission(&group, 6_000), Admission::Closed);
        guard.clear();
        fail_and_reopen(&mut guard, &group, 61_000);
    }

    #[test]
    fn reopens_spread_over_more_than_a_minute_are_not_capped() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 20_000, 40_000, 60_000, 80_000] {
            fail_and_reopen(&mut guard, &group, at);
        }
    }

    #[test]
    fn clear_resets_the_window() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 2_000, 4_000] {
            fail_and_reopen(&mut guard, &group, at);
        }
        guard.clear();
        for at in [7_000, 8_000, 9_000] {
            fail_and_reopen(&mut guard, &group, at);
        }
        assert_eq!(
            guard.on_failure(group.clone(), 10_000),
            AfterFailure::StayClosed
        );
    }

    #[test]
    fn cap_is_per_path_set() {
        let capped = set(&["a"]);
        let other = set(&["b"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 1_000, 2_000] {
            fail_and_reopen(&mut guard, &capped, at);
        }
        assert_eq!(
            guard.on_failure(capped.clone(), 3_000),
            AfterFailure::StayClosed
        );
        for at in [4_000, 5_000, 6_000] {
            fail_and_reopen(&mut guard, &other, at);
        }
    }

    #[test]
    fn checking_spends_no_reopen() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        assert_eq!(guard.on_failure(group.clone(), 0), AfterFailure::Reopen);
        for at in 0..5 {
            assert_eq!(guard.admission(&group, at), Admission::Open);
        }
        guard.on_report(&group);
        for at in [1_000, 2_000, 3_000] {
            fail_and_reopen(&mut guard, &group, at);
        }
    }

    /// A device that re-enumerates after each failure: the failure's rescan finds it gone,
    /// then its arrival a second later opens it if admitted.
    fn fail_and_rearrive(guard: &mut ReadFailureGuard, group: &PathSet, at: Ms) -> Admission {
        guard.on_failure(group.clone(), at);
        guard.on_arrival(at + 1_000);
        let admission = guard.admission(group, at + 1_000);
        if admission == Admission::Open {
            guard.on_open(group, at + 1_000);
        }
        admission
    }

    #[test]
    fn reopens_by_rearrival_are_capped() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 2_000, 4_000] {
            let admission = fail_and_rearrive(&mut guard, &group, at);
            assert_eq!(admission, Admission::Open, "refused at {at}");
        }
        // Held back until the reopen at 1 s leaves the window.
        let held = Admission::HeldUntil(61_000);
        assert_eq!(fail_and_rearrive(&mut guard, &group, 6_000), held);
        guard.on_arrival(59_000);
        assert_eq!(guard.admission(&group, 59_000), held);
        assert_eq!(guard.admission(&group, 61_000), Admission::Open);
    }

    #[test]
    fn resume_lifts_the_rearrival_cap_at_once() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 2_000, 4_000] {
            assert_eq!(fail_and_rearrive(&mut guard, &group, at), Admission::Open);
        }
        assert_ne!(
            fail_and_rearrive(&mut guard, &group, 6_000),
            Admission::Open
        );
        guard.clear();
        assert_eq!(guard.admission(&group, 8_000), Admission::Open);
    }

    #[test]
    fn removal_then_arrival_is_capped() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 2_000, 4_000] {
            guard.on_removed(group.clone(), at);
            guard.on_arrival(at + 1_000);
            reopen(&mut guard, &group, at + 1_000);
        }
        guard.on_removed(group.clone(), 6_000);
        guard.on_arrival(7_000);
        assert_eq!(guard.admission(&group, 7_000), Admission::HeldUntil(61_000));
    }

    #[test]
    fn arrivals_without_failures_are_never_capped() {
        let fresh = set(&["fresh"]);
        let capped = set(&["capped"]);
        let mut guard = ReadFailureGuard::default();
        for at in [0, 2_000, 4_000] {
            assert_eq!(fail_and_rearrive(&mut guard, &capped, at), Admission::Open);
        }
        for at in 0..10 {
            guard.on_arrival(at);
            reopen(&mut guard, &fresh, at);
        }
    }

    #[test]
    fn arrivals_after_an_old_failure_are_not_counted() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        fail_and_reopen(&mut guard, &group, 0);
        for at in [60_000, 61_000, 62_000, 63_000] {
            guard.on_arrival(at);
            reopen(&mut guard, &group, at);
        }
    }

    #[test]
    fn arrival_forgets_groups_outside_the_window() {
        let mut guard = ReadFailureGuard::default();
        guard.on_removed(set(&["a"]), 0);
        guard.on_arrival(59_999);
        assert_eq!(guard.groups.len(), 1);
        guard.on_arrival(60_000);
        assert!(guard.groups.is_empty());
    }

    /// A device whose PRO X Wireless dongle collection is served by a fresh pipe, with its
    /// product string cached so that nothing but the open touches the single-instance pipe.
    fn dongle_device() -> (Device, Server, HidInterface, PathSet) {
        let (server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let hidpp = dongle_member(pipe, 0xFF43, 0x0202, 20);
        let paths: PathSet = [path_key(&hidpp.path)].into();
        let mut device = device();
        let product = "PRO X Wireless Gaming Headset".to_owned();
        device.products.insert(path_key(&hidpp.path), product);
        (device, server, hidpp, paths)
    }

    /// Caps `paths` with reopens at `base` + 1, 3 and 5 s and a failure at `base` + 6 s.
    fn cap(guard: &mut ReadFailureGuard, paths: &PathSet, base: Ms) {
        for at in [0, 2_000, 4_000] {
            assert_eq!(fail_and_rearrive(guard, paths, base + at), Admission::Open);
        }
        guard.on_failure(paths.clone(), base + 6_000);
    }

    #[test]
    fn arrival_keeps_a_capped_group_closed() {
        let (mut device, _server, hidpp, paths) = dongle_device();
        let base = now_ms();
        cap(&mut device.guard, &paths, base);

        assert!(device.on_event(OsEvent::InterfaceArrived));
        device.apply_scan(Some(vec![hidpp]), base + 7_000);
        assert!(device.groups.is_empty());
    }

    #[test]
    fn held_back_group_opens_when_its_window_frees() {
        let (mut device, _server, hidpp, paths) = dongle_device();
        let base = now_ms();
        cap(&mut device.guard, &paths, base);
        device.guard.on_arrival(base + 7_000);
        let release = base + 61_000;

        device.apply_scan(Some(vec![hidpp.clone()]), base + 7_000);
        assert!(device.groups.is_empty());
        assert_eq!(device.deadline(), Some(release));
        assert!(!device.release_due(release - 1));
        assert!(device.release_due(release));
        assert_eq!(device.deadline(), None, "release is one-shot");

        device.apply_scan(Some(vec![hidpp]), release);
        assert_eq!(device.groups.len(), 1);
    }

    /// A trace appending to a fresh file of its own, and that file's path.
    fn temp_trace(name: &str) -> (Trace, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "batterysight-test-{name}-{}.log",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        (Trace::to_file(file), path)
    }

    #[test]
    fn held_back_group_is_traced_until_released() {
        let (mut device, _server, hidpp, paths) = dongle_device();
        let (trace, log) = temp_trace("held");
        device.trace = Some(trace);
        let path = paths.first().unwrap().clone();
        let base = now_ms();
        cap(&mut device.guard, &paths, base);
        device.guard.on_arrival(base + 7_000);

        device.apply_scan(Some(vec![hidpp.clone()]), base + 7_000);
        // Still held at the next scan: no second line.
        device.apply_scan(Some(vec![hidpp.clone()]), base + 8_000);
        let release = base + 61_000;
        assert!(device.release_due(release));
        device.apply_scan(Some(vec![hidpp]), release);
        assert_eq!(device.groups.len(), 1);

        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        // The reopened group's arrival ping is traced too: only the cap's lines matter here.
        let words: Vec<Vec<&str>> = text
            .lines()
            .map(|line| line.split(' ').skip(1).collect::<Vec<_>>())
            .filter(|words| matches!(words.first(), Some(&("held" | "released"))))
            .collect();
        assert_eq!(words.len(), 2, "{text}");
        assert_eq!(words[0][..3], ["held", path.as_str(), "until"], "{text}");
        let held_at: u64 = text.split(' ').next().unwrap().parse().unwrap();
        let until: u64 = words[0][3].parse().unwrap();
        assert_eq!(until - held_at, 54_000, "{text}");
        assert_eq!(words[1], ["released", path.as_str()], "{text}");
    }

    #[test]
    fn held_back_group_that_fails_to_open_is_traced_but_not_released() {
        let (mut device, server, hidpp, paths) = dongle_device();
        let (trace, log) = temp_trace("open-failed");
        device.trace = Some(trace);
        let path = paths.first().unwrap().clone();
        let base = now_ms();
        cap(&mut device.guard, &paths, base);
        device.guard.on_arrival(base + 7_000);
        device.apply_scan(Some(vec![hidpp.clone()]), base + 7_000);

        drop(server);
        device.apply_scan(Some(vec![hidpp]), base + 61_000);
        assert!(device.groups.is_empty());

        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        let words: Vec<Vec<&str>> = text
            .lines()
            .map(|line| line.split(' ').skip(1).collect())
            .collect();
        assert_eq!(words.len(), 2, "{text}");
        assert_eq!(words[0][..2], ["held", path.as_str()], "{text}");
        assert_eq!(words[1], ["open", "failed", path.as_str()], "{text}");
    }

    #[test]
    fn failed_discovery_is_traced() {
        let mut device = device();
        let (trace, log) = temp_trace("discovery");
        device.trace = Some(trace);
        let group = HidGroup {
            id: GroupId(9),
            collections: vec![Collection {
                id: CollectionId(u32::MAX),
                vid: 0x046D,
                pid: 0x0ABA,
                usage_page: 0xFF43,
                usage: 0x0202,
                input_len: 20,
                output_len: 20,
                feature_len: 0,
                product: "PRO X Wireless Gaming Headset".into(),
            }],
        };

        // The ping (to a collection that is not open) goes unanswered.
        device.feed(HubInput::GroupArrived(group), 0);
        device.feed(HubInput::Tick, 4_000);

        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        let words: Vec<&str> = text.trim_end().split(' ').collect();
        assert_eq!(
            words[1..5],
            ["discovery", "logi-group:0ABA:9", "failed,", "retry"],
            "{text}"
        );
        let at: u64 = words[0].parse().unwrap();
        let retry_at: u64 = words[6].parse().unwrap();
        assert_eq!(retry_at - at, 2_000, "{text}");
    }

    #[test]
    fn absent_held_back_group_sets_no_deadline() {
        let (mut device, _server, hidpp, paths) = dongle_device();
        let base = now_ms();
        cap(&mut device.guard, &paths, base);
        device.guard.on_arrival(base + 7_000);
        device.apply_scan(Some(vec![hidpp]), base + 7_000);
        assert!(device.deadline().is_some());

        device.apply_scan(Some(Vec::new()), base + 8_000);
        assert_eq!(device.deadline(), None);
    }

    #[test]
    fn failed_open_spends_no_reopen() {
        let (mut device, server, hidpp, paths) = dongle_device();
        drop(server);
        let base = now_ms();
        assert_eq!(
            device.guard.on_failure(paths.clone(), base),
            AfterFailure::Reopen
        );
        for at in 1..=4 {
            device.apply_scan(Some(vec![hidpp.clone()]), base + at);
            assert!(device.groups.is_empty());
        }
        assert_eq!(device.guard.admission(&paths, base + 5), Admission::Open);
    }

    #[test]
    fn removing_an_opened_group_records_a_failure() {
        let mut device = device();
        for path in ["a", "b"] {
            device.groups.push(Opened {
                id: GroupId(u32::MAX),
                paths: set(&[path]),
                collections: Vec::new(),
                container: None,
            });
        }

        assert!(device.on_event(OsEvent::InterfaceRemoved { path: wide("A") }));
        device.apply_scan(Some(Vec::new()), now_ms());

        assert!(device.guard.groups.contains_key(&set(&["a"])));
        assert!(device.guard.groups.contains_key(&set(&["b"])));
    }

    fn device() -> Device {
        Device {
            hub: Hub::new(Catalog::builtin(), Policy::default()),
            reactor: Reactor::new().unwrap(),
            groups: Vec::new(),
            products: HashMap::new(),
            guard: ReadFailureGuard::default(),
            release_at: None,
            held: BTreeMap::new(),
            audio: AudioActivity::default(),
            trace: None,
            states: Arc::new(Mutex::new(Vec::new())),
            on_change: Box::new(|| {}),
        }
    }

    /// A collection of the PRO X Wireless dongle (046D:0ABA) served by a named pipe.
    fn dongle_member(pipe: HidInterface, usage_page: u16, usage: u16, len: u16) -> HidInterface {
        HidInterface {
            path: pipe.path,
            collection: Collection {
                id: pipe.collection.id,
                vid: 0x046D,
                pid: 0x0ABA,
                usage_page,
                usage,
                input_len: len,
                output_len: len,
                feature_len: 0,
                product: String::new(),
            },
            container: None,
        }
    }

    #[test]
    fn only_collections_an_engine_reads_are_opened() {
        let (_hidpp_server, hidpp) = Server::new(PIPE_ACCESS_DUPLEX);
        let (_consumer_server, consumer) = Server::new(PIPE_ACCESS_DUPLEX);
        let hidpp = dongle_member(hidpp, 0xFF43, 0x0202, 20);
        let consumer = dongle_member(consumer, 0x000C, 0x0001, 3);
        let (hidpp_id, consumer_id) = (hidpp.collection.id, consumer.collection.id);
        let mut device = device();
        // A single-instance pipe takes one open only: keep `product_string` off the pipes.
        for member in [&hidpp, &consumer] {
            let product = "PRO X Wireless Gaming Headset".to_owned();
            device.products.insert(path_key(&member.path), product);
        }

        device.apply_scan(Some(vec![consumer, hidpp]), now_ms());

        assert_eq!(device.groups.len(), 1);
        assert_eq!(device.groups[0].collections, [hidpp_id]);
        assert!(
            !device.reactor.write(consumer_id, &[0x03]),
            "consumer control opened"
        );
    }

    #[test]
    fn failed_enumeration_keeps_every_group() {
        let mut device = device();
        device.groups.push(Opened {
            id: GroupId(u32::MAX),
            paths: set(&["a"]),
            collections: Vec::new(),
            container: None,
        });

        device.apply_scan(None, now_ms());
        assert_eq!(device.groups.len(), 1);

        device.apply_scan(Some(Vec::new()), now_ms());
        assert!(device.groups.is_empty());
    }

    #[test]
    fn empty_product_string_is_not_cached() {
        let mut products = HashMap::new();
        let key = || "path".to_owned();
        assert_eq!(cached_product(&mut products, key(), String::new), "");
        assert_eq!(
            cached_product(&mut products, key(), || "USB Receiver".into()),
            "USB Receiver"
        );
        assert_eq!(
            cached_product(&mut products, key(), || panic!("read again")),
            "USB Receiver"
        );
    }

    #[test]
    fn activity_due_needs_audible_audio_and_a_ready_device() {
        assert!(!activity_due(Some(0), 0, false, Some(0.2)), "inactive");
        assert!(!activity_due(Some(0), 0, true, Some(0.0)), "silent");
        // Owner session: a paused player left the headset endpoint at ~1.6e-6 (-115 dBFS).
        assert!(
            !activity_due(Some(0), 0, true, Some(1.6e-6)),
            "near-silence"
        );
        assert!(activity_due(Some(0), 0, true, Some(0.001)), "-60 dBFS");
        assert!(!activity_due(Some(0), 0, true, None), "no meter");
        assert!(
            !activity_due(None, 0, true, Some(0.2)),
            "refused: asleep or pushes"
        );
        assert!(activity_due(Some(0), 0, true, Some(0.2)));
        assert!(!activity_due(Some(300_000), 299_999, true, Some(0.2)));
        assert!(activity_due(Some(300_000), 300_000, true, Some(0.2)));
    }

    /// The activity gap the fake headset registers with.
    const GAP: Ms = 300_000;

    /// Registers `key` with `caps` and `presence` on its arrival grant and records every
    /// grant's trigger.
    struct Registering {
        key: DeviceKey,
        caps: Capabilities,
        presence: Option<Presence>,
        grants: Rc<RefCell<Vec<Trigger>>>,
    }

    impl Engine for Registering {
        fn group_key(&self) -> DeviceKey {
            DeviceKey(format!("group:{}", self.key.0))
        }

        fn on_grant(&mut self, grant: &Grant, _now: Ms) -> Vec<Action> {
            self.grants.borrow_mut().push(grant.trigger);
            if grant.trigger != Trigger::Arrival {
                return Vec::new();
            }
            vec![Action::Register {
                key: self.key.clone(),
                caps: self.caps,
                initial: DeviceUpdate {
                    key: self.key.clone(),
                    name: None,
                    kind: None,
                    via: None,
                    level: None,
                    millivolts: None,
                    charge: None,
                    presence: self.presence,
                    source: Source::Query,
                },
                wpid: None,
            }]
        }

        fn on_report(&mut self, _: CollectionId, _: &[u8], _: Ms) -> Vec<Action> {
            Vec::new()
        }

        fn on_tick(&mut self, _: Ms) -> Vec<Action> {
            Vec::new()
        }

        fn next_deadline(&self) -> Option<Ms> {
            None
        }
    }

    const HEADSET: ContainerId = [1; 16];
    const MOUSE: ContainerId = [2; 16];

    type Grants = Rc<RefCell<Vec<Trigger>>>;

    /// A device with a headset (group 1, read on activity, registered with
    /// `headset_presence`) and a mouse that pushes its battery (group 2), each in its own
    /// container, both registered at 0 with audio playing to both; and their grant logs.
    fn audio_device(headset_presence: Option<Presence>) -> (Device, Grants, Grants) {
        let headset_grants: Grants = Rc::default();
        let mouse_grants: Grants = Rc::default();
        let (h, m) = (Rc::clone(&headset_grants), Rc::clone(&mouse_grants));
        let mut device = device();
        device.hub = Hub::new(Catalog::builtin(), Policy::default()).with_engine_factory(Box::new(
            move |group, _| {
                let headset = group.id == GroupId(1);
                let caps = Capabilities {
                    pushes_battery: !headset,
                    query_wakes: false,
                    min_query_gap_ms: if headset { GAP } else { 0 },
                };
                Some(Box::new(Registering {
                    key: DeviceKey(if headset { "headset" } else { "mouse" }.into()),
                    caps,
                    presence: if headset { headset_presence } else { None },
                    grants: Rc::clone(if headset { &h } else { &m }),
                }) as Box<dyn Engine>)
            },
        ));
        for (id, container) in [(1, HEADSET), (2, MOUSE)] {
            device.groups.push(Opened {
                id: GroupId(id),
                paths: set(&[&id.to_string()]),
                collections: Vec::new(),
                container: Some(container),
            });
            let group = HidGroup {
                id: GroupId(id),
                collections: Vec::new(),
            };
            device.feed(HubInput::GroupArrived(group), 0);
        }
        assert_eq!(device.deadline(), None, "silence sets no deadline");
        for container in [HEADSET, MOUSE] {
            let active = OsEvent::AudioActive {
                container,
                active: true,
            };
            assert!(!device.on_event(active));
        }
        (device, headset_grants, mouse_grants)
    }

    /// A peak meter for the headset's container only: the mouse is never a candidate.
    fn headset_peak(level: f32) -> impl Fn(&ContainerId) -> Option<f32> {
        move |container| {
            assert_eq!(
                *container, HEADSET,
                "metered a container no read can follow"
            );
            Some(level)
        }
    }

    #[test]
    fn audio_activity_reads_audible_devices_that_do_not_push() {
        let (mut device, headset_grants, mouse_grants) = audio_device(None);
        // Registration read the battery: audio already playing waits the gap.
        assert_eq!(device.deadline(), Some(GAP));
        device.check_activity(GAP - 1, |_| panic!("checked before the gap"));

        // Silent: looked at again after 15 s, doubling up to 5 min.
        let mut at = GAP;
        for wait in [15_000, 30_000, 60_000, 120_000, 240_000, 300_000, 300_000] {
            device.check_activity(at, headset_peak(0.0));
            at += wait;
            assert_eq!(device.deadline(), Some(at), "after a silent check");
        }
        assert_eq!(*headset_grants.borrow(), [Trigger::Arrival]);

        device.check_activity(at, headset_peak(0.4));
        assert_eq!(
            *headset_grants.borrow(),
            [Trigger::Arrival, Trigger::Activity]
        );
        assert_eq!(*mouse_grants.borrow(), [Trigger::Arrival]);
        // The first read after registration came at the minimum; the default gap follows.
        assert_eq!(device.deadline(), Some(at + DEFAULT_INTERVAL_MS));

        // An audible check reset the back-off.
        let next = at + DEFAULT_INTERVAL_MS;
        device.check_activity(next, headset_peak(0.0));
        assert_eq!(device.deadline(), Some(next + 15_000));

        // Audio stops: nothing wakes the device thread any more.
        for container in [HEADSET, MOUSE] {
            let idle = OsEvent::AudioActive {
                container,
                active: false,
            };
            assert!(!device.on_event(idle));
        }
        assert_eq!(device.deadline(), None);
    }

    #[test]
    fn going_idle_resets_the_silence_back_off() {
        let (mut device, _, _) = audio_device(None);
        device.check_activity(GAP, headset_peak(0.0));
        device.check_activity(GAP + 15_000, headset_peak(0.0));
        assert_eq!(device.deadline(), Some(GAP + 45_000));
        for active in [false, true] {
            let event = OsEvent::AudioActive {
                container: HEADSET,
                active,
            };
            device.on_event(event);
        }
        device.check_activity(GAP + 50_000, headset_peak(0.0));
        assert_eq!(device.deadline(), Some(GAP + 65_000));
    }

    #[test]
    fn near_silence_backs_off_and_reads_nothing() {
        let (mut device, headset_grants, _) = audio_device(None);
        // Owner session: a paused player kept a stream open at ~1.6e-6 (-115 dBFS).
        device.check_activity(GAP, headset_peak(1.6e-6));
        device.check_activity(GAP + 15_000, headset_peak(1.6e-6));
        assert_eq!(device.deadline(), Some(GAP + 45_000), "silent back-off");
        assert_eq!(*headset_grants.borrow(), [Trigger::Arrival]);

        device.check_activity(GAP + 45_000, headset_peak(0.001));
        assert_eq!(
            *headset_grants.borrow(),
            [Trigger::Arrival, Trigger::Activity]
        );
    }

    #[test]
    fn asleep_device_sets_no_activity_deadline() {
        let (mut device, headset_grants, _) = audio_device(Some(Presence::Asleep));
        assert_eq!(device.deadline(), None);
        device.check_activity(10 * GAP, |_| panic!("metered for an asleep device"));
        assert_eq!(*headset_grants.borrow(), [Trigger::Arrival]);
    }
}
