//! Runs the hub on a device thread over the reactor.
//!
//! The device thread owns the [`Reactor`], the HID and power notification registrations and
//! the [`Hub`]; none of them leave it. Other threads see the devices through the
//! [`Snapshot`] and talk to the thread through a [`RuntimeHandle`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::Infallible;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use batterysight_core::catalog::Catalog;
use batterysight_core::engines;
use batterysight_core::hid::{CollectionId, GroupId, HidGroup};
use batterysight_core::hub::{Hub, HubInput, HubOutput};
use batterysight_core::model::DeviceState;
use batterysight_core::scheduler::Policy;
use batterysight_core::time::Ms;
use windows::Win32::Foundation::E_UNEXPECTED;
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::hid_enum::{self, HidInterface};
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
    Stop,
}

impl RuntimeHandle {
    /// Asks for a fresh reading of every device, as far as the scheduler allows.
    pub fn refresh_now(&self) {
        self.command(Command::RefreshNow);
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

/// Sets up the reactor and the notifications, reports the outcome on `ready`, then runs
/// until told to stop. Everything is released before it returns.
fn device_thread(
    ready: &SyncSender<windows::core::Result<Poster>>,
    commands: &Receiver<Command>,
    states: Arc<Mutex<Vec<DeviceState>>>,
    on_change: Box<dyn Fn() + Send>,
) {
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
    Device {
        hub: Hub::new(Catalog::builtin(), Policy::default()),
        reactor,
        groups: Vec::new(),
        products: HashMap::new(),
        guard: ReadFailureGuard::default(),
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
}

/// The device thread's state.
struct Device {
    hub: Hub,
    reactor: Reactor,
    groups: Vec<Opened>,
    /// Product strings by lowercased path; each is read from the device until a read succeeds.
    products: HashMap<String, String>,
    /// Keeps a group closed after back-to-back read failures until the next arrival,
    /// resume or refresh.
    guard: ReadFailureGuard,
    trace: Option<Trace>,
    states: Arc<Mutex<Vec<DeviceState>>>,
    on_change: Box<dyn Fn() + Send>,
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
            // May return nothing before the deadline; the loop then checks it again.
            let events = self.reactor.wait(self.hub.next_deadline(), now_ms());
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
                    self.guard.on_failure(&gone.paths) == AfterFailure::Reopen
                })
            }
            OsEvent::InterfaceRemoved { path } => {
                let path = path_key(&path);
                let removed = self
                    .groups
                    .iter()
                    .position(|group| group.paths.contains(&path));
                removed.is_some_and(|index| {
                    self.remove(index);
                    true
                })
            }
            OsEvent::InterfaceArrived => {
                self.guard.clear();
                true
            }
            OsEvent::Resume => {
                // Handles stay open: a device asleep across the suspend keeps its entry.
                self.guard.clear();
                self.rescan();
                self.feed(HubInput::Resume, now_ms());
                false
            }
            OsEvent::WriteFailed { .. } | OsEvent::Wake => false,
        }
    }

    /// Enumerates the HID groups engines want, closes the opened ones no longer present
    /// and opens the new ones, except those the read-failure guard keeps closed.
    fn rescan(&mut self) {
        self.apply_scan(hid_enum::enumerate());
    }

    /// [`Device::rescan`] on the interfaces `scanned`. `None` (the enumeration failed) says
    /// nothing about which devices are present, so every group stays as it is.
    fn apply_scan(&mut self, scanned: Option<Vec<HidInterface>>) {
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
            self.remove(index);
        }
        for (index, (group, members, paths)) in scanned.into_iter().enumerate() {
            if new.contains(&index) && !self.guard.is_closed(&paths) {
                self.arrive(group, members, paths);
            }
        }
    }

    /// Fills in the product strings of the collections an engine reads, opens those and
    /// hands them to the hub. The group's other collections (such as a headset's consumer
    /// control) are never opened, so their reports never reach the process.
    fn arrive(&mut self, mut group: HidGroup, members: Vec<HidInterface>, paths: PathSet) {
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
            return;
        }
        self.groups.push(Opened {
            id: group.id,
            paths,
            collections: group.collections.iter().map(|c| c.id).collect(),
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
                HubOutput::Send {
                    collection, bytes, ..
                } => {
                    if self.reactor.write(collection, &bytes)
                        && let Some(trace) = &mut self.trace
                    {
                        trace.record(Direction::Out, collection, &bytes);
                    }
                }
                HubOutput::StateChanged => changed = true,
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

/// What to do with a group whose read just failed.
#[derive(Debug, PartialEq, Eq)]
enum AfterFailure {
    /// Re-enumerate; the group is reopened if still present.
    Reopen,
    /// Leave the group closed until the guard is cleared.
    StayClosed,
}

/// Counts back-to-back read failures per group, keyed by path set.
///
/// A failure reopens the group unless the previous failure of the same group was not
/// followed by a report from it; then the group stays closed until [`clear`]. That way a
/// read that keeps failing does not reopen the device in a loop.
///
/// [`clear`]: ReadFailureGuard::clear
#[derive(Default)]
struct ReadFailureGuard {
    /// Groups that failed and reported nothing since.
    failed: HashSet<PathSet>,
    /// Groups that failed twice in a row.
    closed: HashSet<PathSet>,
}

impl ReadFailureGuard {
    /// The group read normally: its next failure counts as the first again.
    fn on_report(&mut self, paths: &PathSet) {
        self.failed.remove(paths);
    }

    fn on_failure(&mut self, paths: &PathSet) -> AfterFailure {
        if self.failed.insert(paths.clone()) {
            AfterFailure::Reopen
        } else {
            self.closed.insert(paths.clone());
            AfterFailure::StayClosed
        }
    }

    /// Forgets every failure: closed groups may be reopened.
    fn clear(&mut self) {
        self.failed.clear();
        self.closed.clear();
    }

    fn is_closed(&self, paths: &PathSet) -> bool {
        self.closed.contains(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::hid::Collection;
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

    #[test]
    fn failures_separated_by_a_report_reopen() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        assert_eq!(guard.on_failure(&group), AfterFailure::Reopen);
        guard.on_report(&group);
        assert_eq!(guard.on_failure(&group), AfterFailure::Reopen);
        assert!(!guard.is_closed(&group));
    }

    #[test]
    fn back_to_back_failures_stay_closed() {
        let group = set(&["a"]);
        let other = set(&["b"]);
        let mut guard = ReadFailureGuard::default();
        assert_eq!(guard.on_failure(&group), AfterFailure::Reopen);
        guard.on_report(&other);
        assert_eq!(guard.on_failure(&group), AfterFailure::StayClosed);
        assert!(guard.is_closed(&group));
        assert!(!guard.is_closed(&other));
    }

    #[test]
    fn clear_allows_reopening() {
        let group = set(&["a"]);
        let mut guard = ReadFailureGuard::default();
        guard.on_failure(&group);
        guard.on_failure(&group);
        guard.clear();
        assert!(!guard.is_closed(&group));
        assert_eq!(guard.on_failure(&group), AfterFailure::Reopen);
    }

    fn device() -> Device {
        Device {
            hub: Hub::new(Catalog::builtin(), Policy::default()),
            reactor: Reactor::new().unwrap(),
            groups: Vec::new(),
            products: HashMap::new(),
            guard: ReadFailureGuard::default(),
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

        device.apply_scan(Some(vec![consumer, hidpp]));

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
        });

        device.apply_scan(None);
        assert_eq!(device.groups.len(), 1);

        device.apply_scan(Some(Vec::new()));
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
}
