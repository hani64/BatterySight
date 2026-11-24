//! One-thread I/O completion port reactor for HID reports and posted events.
//!
//! Each opened handle has a distinct completion key, and each overlapped operation is
//! an [`Op`] identified by its `OVERLAPPED` pointer. Closing a handle retires its key,
//! but the operation remains allocated until its packet arrives.
//! Other threads hand events to the reactor through a [`Poster`], which queues a packet with
//! [`KEY_POSTED`] whose `OVERLAPPED` slot carries a leaked `Box<OsEvent>`; the reactor takes
//! the box back when it dequeues the packet, or when it drains the port on drop.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::c_void;
use std::mem;
use std::ptr::NonNull;
use std::sync::{Arc, PoisonError, RwLock};

use batterysight_core::hid::{CollectionId, GroupId, ReportKind};
use batterysight_core::time::Ms;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
    PostQueuedCompletionStatus,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::core::HRESULT;

use crate::hid_enum::{ContainerId, HidInterface, OwnedHandle};
use crate::hid_io::{self, Kind, Op};

/// Completion keys at or above this value identify unique handle generations.
const FIRST_IO_KEY: usize = 3;
/// Completion key of posted packets; the `OVERLAPPED` slot is a leaked `Box<OsEvent>`.
const KEY_POSTED: usize = 2;
/// Packets taken from the port per `GetQueuedCompletionStatusEx` call.
const BATCH: usize = 64;
/// `INFINITE` from `WinBase.h`: wait without a timeout.
const INFINITE: u32 = u32::MAX;

/// Something that happened on the system, delivered by [`Reactor::wait`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsEvent {
    /// An input report, exactly as read: the report id is byte 0.
    Report {
        collection: CollectionId,
        bytes: Vec<u8>,
    },
    /// A read failed (for example, the device was unplugged, or something other than
    /// `close_group` aborted it); no further reads are issued.
    ReadFailed { collection: CollectionId },
    /// A write could not be started or failed, or the collection is not open for writing.
    WriteFailed { collection: CollectionId },
    /// A successful feature GET, including the requested report ID at byte zero.
    FeatureReply {
        collection: CollectionId,
        bytes: Vec<u8>,
        deadline: Ms,
    },
    /// A feature request was refused, failed or ran past its deadline.
    FeatureFailed {
        collection: CollectionId,
        kind: ReportKind,
        reason: FeatureFailure,
    },
    /// A HID interface appeared; re-enumerate to find it.
    InterfaceArrived,
    /// The HID interface at `path` (NUL-terminated, as in `HidInterface::path`) went away.
    InterfaceRemoved { path: Vec<u16> },
    /// The system resumed from sleep.
    Resume,
    /// The render endpoints of `container` got their first active audio session (`active`)
    /// or lost their last one.
    AudioActive {
        container: ContainerId,
        active: bool,
    },
    /// Audio activity is not (fully) watched: `stage` failed on the audio thread.
    AudioUnavailable { stage: &'static str },
    /// Posted by another thread to make [`Reactor::wait`] return.
    Wake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureFailure {
    Rejected,
    Start(HRESULT),
    Completion(HRESULT),
    InvalidReply,
    TimedOut,
}

/// One opened collection.
struct Open {
    group: GroupId,
    handle: OwnedHandle,
    key: usize,
    /// Descriptor widths; even GET is restricted to the admitted command handle.
    output_len: usize,
    feature_len: usize,
    writable: bool,
    /// Operations started and not yet dequeued.
    pending: usize,
}

struct PendingFeature {
    overlapped: NonNull<OVERLAPPED>,
    kind: ReportKind,
    /// Cleared when cancellation is requested; the slot stays busy until dequeue.
    deadline: Option<Ms>,
}

#[cfg(test)]
type Cancel = unsafe fn(HANDLE, Option<*const OVERLAPPED>) -> windows::core::Result<()>;

#[cfg(test)]
struct FeatureHooks {
    control: hid_io::FeatureControl,
    cancel: Cancel,
    clock: fn() -> Ms,
}

/// The completion port as shared with posters: the raw handle, or 0 once it is closed.
struct Port(RwLock<usize>);

/// Overlapped HID I/O and posted events, all completed through one completion port and
/// consumed on one thread.
pub struct Reactor {
    port: HANDLE,
    shared: Arc<Port>,
    opens: HashMap<CollectionId, Open>,
    features: HashMap<CollectionId, PendingFeature>,
    next_key: usize,
    /// All queued operations, including operations on already closed handles.
    pending: usize,
    /// Failed starts are returned first by the next `wait`.
    backlog: Vec<OsEvent>,
    #[cfg(test)]
    feature_hooks: FeatureHooks,
}

impl Reactor {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: creates a new port; no file handle is involved.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1) }?;
        Ok(Reactor {
            port,
            shared: Arc::new(Port(RwLock::new(port.0 as usize))),
            opens: HashMap::new(),
            features: HashMap::new(),
            next_key: FIRST_IO_KEY,
            pending: 0,
            backlog: Vec::new(),
            #[cfg(test)]
            feature_hooks: FeatureHooks {
                control: hid_io::FEATURE_CONTROL,
                cancel: CancelIoEx,
                clock: clock_ms,
            },
        })
    }

    /// Opens `iface` with the selected access, arming input only for a receive collection.
    ///
    /// Returns `false` for an exclusive mouse/keyboard or an already opened ID; neither is
    /// a new handle. A read that cannot be started is reported by the next `wait`.
    pub fn open(
        &mut self,
        group: GroupId,
        iface: &HidInterface,
        read: bool,
        write: bool,
    ) -> windows::core::Result<bool> {
        let collection = &iface.collection;
        if hid_io::is_exclusive(collection.usage_page, collection.usage)
            || self.opens.contains_key(&collection.id)
        {
            return Ok(false);
        }
        let handle = hid_io::open(&iface.path, read, write)?;
        let key = self.next_key;
        self.next_key = key.checked_add(1).expect("IOCP handle keys exhausted");
        // SAFETY: both handles are open; the port outlives the file handle.
        unsafe { CreateIoCompletionPort(handle.0, Some(self.port), key, 0) }?;
        let mut open = Open {
            group,
            handle,
            key,
            output_len: usize::from(collection.output_len),
            feature_len: usize::from(collection.feature_len),
            writable: write,
            pending: 0,
        };
        let input_len = usize::from(collection.input_len);
        if read && input_len > 0 {
            if Op::new(collection.id, Kind::Read, vec![0; input_len]).start(open.handle.0) {
                open.pending += 1;
                self.pending += 1;
            } else {
                self.backlog.push(OsEvent::ReadFailed {
                    collection: collection.id,
                });
            }
        }
        self.opens.insert(collection.id, open);
        Ok(true)
    }

    /// Cancels and closes the group's handles. Operations stay allocated until their
    /// completion packets arrive; packets from these handles cannot produce events.
    pub fn close_group(&mut self, group: GroupId) {
        self.close_where(|open| open.group == group);
    }

    /// Sends `bytes` (report id first) to `collection`, zero-padded to its output report
    /// length, and returns whether the write was started. Failures arrive as `WriteFailed`,
    /// including writes to a collection that is not open, is read-only, or whose output
    /// report is shorter than `bytes` (those are not started).
    pub fn write(&mut self, collection: CollectionId, bytes: &[u8]) -> bool {
        let started = self.opens.get_mut(&collection).is_some_and(|open| {
            let started = open.writable
                && hid_io::padded(bytes, open.output_len).is_some_and(|buffer| {
                    Op::new(collection, Kind::Write, buffer).start(open.handle.0)
                });
            open.pending += usize::from(started);
            started
        });
        self.pending += usize::from(started);
        if !started {
            self.backlog.push(OsEvent::WriteFailed { collection });
        }
        started
    }

    /// Starts one feature GET or SET on an admitted command handle. The first byte is
    /// the HID report ID (zero for an unnumbered report). The absolute deadline is in
    /// monotonic milliseconds since boot; a cancelled operation keeps its slot until
    /// its completion is dequeued.
    pub fn feature(
        &mut self,
        collection: CollectionId,
        kind: ReportKind,
        bytes: &[u8],
        deadline: Ms,
    ) -> bool {
        match self.start_feature(collection, kind, bytes, deadline) {
            Ok(()) => true,
            Err(reason) => {
                self.backlog.push(OsEvent::FeatureFailed {
                    collection,
                    kind,
                    reason,
                });
                false
            }
        }
    }

    fn start_feature(
        &mut self,
        collection: CollectionId,
        kind: ReportKind,
        bytes: &[u8],
        deadline: Ms,
    ) -> Result<(), FeatureFailure> {
        let rejected = FeatureFailure::Rejected;
        let report_id = *bytes.first().ok_or(rejected)?;
        let op_kind = match kind {
            ReportKind::Output => return Err(rejected),
            ReportKind::SetFeature => Kind::SetFeature,
            ReportKind::GetFeature => Kind::GetFeature { report_id },
        };
        if deadline <= self.feature_now() {
            return Err(rejected);
        }
        let Entry::Vacant(entry) = self.features.entry(collection) else {
            return Err(rejected);
        };
        let open = self
            .opens
            .get_mut(&collection)
            .filter(|open| open.writable)
            .ok_or(rejected)?;
        let buffer = hid_io::padded(bytes, open.feature_len).ok_or(rejected)?;
        let overlapped = Op::new(collection, op_kind, buffer)
            .start_feature(
                open.handle.0,
                #[cfg(test)]
                self.feature_hooks.control,
            )
            .map_err(|error| FeatureFailure::Start(error.code()))?;
        open.pending += 1;
        self.pending += 1;
        entry.insert(PendingFeature {
            overlapped,
            kind,
            deadline: Some(deadline),
        });
        Ok(())
    }

    pub fn poster(&self) -> Poster {
        Poster(Arc::clone(&self.shared))
    }

    /// Waits for an event or the first caller or feature deadline and returns the events
    /// gathered, oldest first. With no deadlines it waits indefinitely.
    ///
    /// Returns at once when earlier events are pending. May return an empty list before the
    /// deadline when only internal completions (finished writes) arrived; callers loop.
    pub fn wait(&mut self, deadline: Option<Ms>, now: Ms) -> Vec<OsEvent> {
        let mut events = mem::take(&mut self.backlog);
        let feature_timeout = self.expire_features(&mut events);
        let timeout = if events.is_empty() {
            timeout_ms(deadline, now).min(feature_timeout)
        } else {
            0
        };
        self.dequeue(timeout, &mut events);
        self.expire_features(&mut events);
        events
    }

    fn feature_now(&self) -> Ms {
        #[cfg(test)]
        let clock = self.feature_hooks.clock;
        #[cfg(not(test))]
        let clock = clock_ms;
        clock()
    }

    /// Cancels overdue operations without making their slots reusable, and returns
    /// the wait budget for the earliest remaining deadline.
    fn expire_features(&mut self, out: &mut Vec<OsEvent>) -> u32 {
        if self.features.is_empty() {
            return INFINITE;
        }
        let now = self.feature_now();
        let mut next = None;
        #[cfg(test)]
        let cancel = self.feature_hooks.cancel;
        #[cfg(not(test))]
        let cancel = CancelIoEx;
        for (&collection, feature) in &mut self.features {
            let Some(deadline) = feature.deadline else {
                continue;
            };
            if now < deadline {
                next = Some(next.map_or(deadline, |at: Ms| at.min(deadline)));
                continue;
            }
            feature.deadline = None;
            // SAFETY: the handle is open and this allocation stays alive until dequeue.
            // ERROR_NOT_FOUND is a normal race with completion; even a successful packet
            // for this operation is no longer a reply.
            let _ =
                unsafe { cancel(self.opens[&collection].handle.0, Some(feature.overlapped.as_ptr())) };
            out.push(OsEvent::FeatureFailed {
                collection,
                kind: feature.kind,
                reason: FeatureFailure::TimedOut,
            });
        }
        timeout_ms(next, now)
    }

    fn close_where(&mut self, closing: impl Fn(&Open) -> bool) {
        self.opens.retain(|collection, open| {
            if !closing(open) {
                return true;
            }
            let _feature = self.features.remove(collection);
            if open.pending > 0 {
                let cancel = CancelIoEx;
                #[cfg(test)]
                let cancel = if _feature.is_some() {
                    self.feature_hooks.cancel
                } else {
                    cancel
                };
                // SAFETY: the file handle is open and all operations outlive their packets.
                let _ = unsafe { cancel(open.handle.0, None) };
            }
            false
        });
    }

    /// Takes up to [`BATCH`] packets from the port, waiting up to `timeout` ms for the first,
    /// and appends their events to `out`. Returns the number of packets taken.
    fn dequeue(&mut self, timeout: u32, out: &mut Vec<OsEvent>) -> usize {
        let mut entries = [OVERLAPPED_ENTRY::default(); BATCH];
        let mut count = 0;
        // SAFETY: the port is open and `entries` is writable for its length.
        let dequeued = unsafe {
            GetQueuedCompletionStatusEx(self.port, &mut entries, &mut count, timeout, false)
        };
        if dequeued.is_err() {
            return 0;
        }
        let count = count as usize;
        for entry in &entries[..count] {
            match entry.lpCompletionKey {
                KEY_POSTED => {
                    // SAFETY: posted packets carry a leaked `Box<OsEvent>`; see `Poster::post`.
                    let event = unsafe { Box::from_raw(entry.lpOverlapped.cast::<OsEvent>()) };
                    out.push(*event);
                }
                key @ FIRST_IO_KEY.. => {
                    // SAFETY: file completion packets come from submitted Ops, each dequeued once.
                    let op = unsafe { Op::from_completion(entry.lpOverlapped) };
                    self.complete(key, entry.lpOverlapped, op, out);
                }
                _ => {}
            }
        }
        count
    }

    /// Handles one finished operation. A retired handle's packet only releases its buffer.
    fn complete(
        &mut self,
        key: usize,
        overlapped: *mut OVERLAPPED,
        mut op: Box<Op>,
        out: &mut Vec<OsEvent>,
    ) {
        self.pending -= 1;
        let Some(open) = self
            .opens
            .get_mut(&op.collection)
            .filter(|open| open.key == key)
        else {
            return;
        };
        open.pending -= 1;
        let collection = op.collection;
        match op.kind {
            Kind::GetFeature { .. } | Kind::SetFeature => {
                let handle = open.handle.0;
                self.complete_feature(handle, overlapped, &mut op, out);
            }
            Kind::Read => match op.result(open.handle.0) {
                Ok(read) => {
                    out.push(OsEvent::Report {
                        collection,
                        bytes: op.buffer[..read].to_vec(),
                    });
                    if op.start(open.handle.0) {
                        open.pending += 1;
                        self.pending += 1;
                    } else {
                        out.push(OsEvent::ReadFailed { collection });
                    }
                }
                // Nothing would read again; the runtime must reopen the collection.
                Err(_) => out.push(OsEvent::ReadFailed { collection }),
            },
            Kind::Write => {
                if op.result(open.handle.0).is_err() {
                    out.push(OsEvent::WriteFailed { collection });
                }
            }
        }
    }

    fn complete_feature(
        &mut self,
        handle: HANDLE,
        overlapped: *mut OVERLAPPED,
        op: &mut Op,
        out: &mut Vec<OsEvent>,
    ) {
        let collection = op.collection;
        let Entry::Occupied(entry) = self.features.entry(collection) else {
            return;
        };
        if entry.get().overlapped.as_ptr() != overlapped {
            return;
        }
        let feature = entry.remove();
        let Some(deadline) = feature.deadline else {
            return;
        };
        let result = if self.feature_now() >= deadline {
            Err(FeatureFailure::TimedOut)
        } else {
            match (op.kind, op.result(handle)) {
                (_, Err(error)) => Err(FeatureFailure::Completion(error.code())),
                (Kind::SetFeature, Ok(_)) => Ok(()),
                (Kind::GetFeature { report_id }, Ok(read)) => {
                    // For unnumbered reports the transfer count excludes Windows' ID byte.
                    let prefix = usize::from(report_id == 0);
                    if read == 0
                        || read > op.buffer.len().saturating_sub(prefix)
                        || op.buffer.first() != Some(&report_id)
                    {
                        Err(FeatureFailure::InvalidReply)
                    } else {
                        op.buffer.truncate(read + prefix);
                        out.push(OsEvent::FeatureReply {
                            collection,
                            bytes: mem::take(&mut op.buffer),
                            deadline,
                        });
                        Ok(())
                    }
                }
                _ => unreachable!("read and write completions are handled separately"),
            }
        };
        if let Err(reason) = result {
            out.push(OsEvent::FeatureFailed {
                collection,
                kind: feature.kind,
                reason,
            });
        }
    }
}

impl Drop for Reactor {
    /// Cancels and drains all I/O, frees events still queued, then closes the port. Posters
    /// that outlive the reactor drop their events from then on.
    fn drop(&mut self) {
        self.close_where(|_| true);
        let mut discarded = Vec::new();
        while self.pending > 0 && self.dequeue(INFINITE, &mut discarded) > 0 {
            discarded.clear();
        }
        let shared = Arc::clone(&self.shared);
        let mut port = shared.0.write().unwrap_or_else(PoisonError::into_inner);
        while self.dequeue(0, &mut discarded) > 0 {
            discarded.clear();
        }
        // SAFETY: the port came from `CreateIoCompletionPort` and is closed only here; posters
        // see 0 under the lock and no longer use it.
        let _ = unsafe { CloseHandle(self.port) };
        *port = 0;
    }
}

/// Queues events for a [`Reactor`] from any thread.
#[derive(Clone)]
pub struct Poster(Arc<Port>);

impl Poster {
    /// Queues `ev` for the reactor's next `wait`. Dropped if the reactor is gone.
    pub fn post(&self, ev: OsEvent) {
        let port = self.0.0.read().unwrap_or_else(PoisonError::into_inner);
        if *port == 0 {
            return;
        }
        let event = Box::into_raw(Box::new(ev));
        // SAFETY: the port stays open while the read lock is held. The packet's OVERLAPPED
        // slot only carries the pointer; the reactor turns it back into the box.
        let posted = unsafe {
            PostQueuedCompletionStatus(
                HANDLE(*port as *mut c_void),
                0,
                KEY_POSTED,
                Some(event.cast_const().cast()),
            )
        };
        if posted.is_err() {
            // SAFETY: the packet was not queued, so the box is still ours.
            drop(unsafe { Box::from_raw(event) });
        }
    }
}

/// Milliseconds from `now` until `deadline`: 0 once it has passed, [`INFINITE`] without one,
/// otherwise capped just below `INFINITE`.
fn timeout_ms(deadline: Option<Ms>, now: Ms) -> u32 {
    match deadline {
        None => INFINITE,
        Some(deadline) => u32::try_from(deadline.saturating_sub(now))
            .unwrap_or(INFINITE)
            .min(INFINITE - 1),
    }
}

/// Monotonic milliseconds since boot, shared with the device thread's scheduler.
pub(crate) fn clock_ms() -> Ms {
    // SAFETY: no arguments, no preconditions.
    unsafe { GetTickCount64() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use batterysight_core::engines::hidpp::frame::fap;
    use batterysight_core::hid::Collection;
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
        ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, ERROR_PIPE_LISTENING, HANDLE, NTSTATUS,
        STATUS_ACCESS_DENIED, STATUS_CANCELLED, STATUS_DEVICE_BUSY, STATUS_PENDING,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX, PIPE_ACCESS_OUTBOUND, ReadFile, WriteFile,
    };
    use windows::Win32::System::IO::{IO_STATUS_BLOCK, PIO_APC_ROUTINE};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_NOWAIT, PIPE_READMODE_MESSAGE,
        PIPE_TYPE_MESSAGE, PIPE_WAIT, SetNamedPipeHandleState,
    };
    use windows::core::PCWSTR;

    use crate::hid_enum::{enumerate, group};

    static NEXT_TEST_ID: AtomicU32 = AtomicU32::new(1_000_000);

    fn fresh_id() -> u32 {
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn iface(path: Vec<u16>, usage_page: u16, usage: u16) -> HidInterface {
        HidInterface {
            path,
            collection: Collection {
                id: CollectionId(fresh_id()),
                vid: 0,
                pid: 0,
                usage_page,
                usage,
                interface: None,
                input_len: 7,
                output_len: 7,
                feature_len: 0,
                product: String::new(),
            },
            container: None,
            node: None,
        }
    }

    /// The server end of a message-mode named pipe; the reactor opens the client end as if it
    /// were a HID collection with 7-byte input and output reports.
    pub(crate) struct Server(HANDLE);

    impl Server {
        pub(crate) fn new(mode: FILE_FLAGS_AND_ATTRIBUTES) -> (Server, HidInterface) {
            let name = wide(&format!(
                r"\\.\pipe\batterysight-test-{}-{}",
                std::process::id(),
                fresh_id()
            ));
            // SAFETY: `name` is NUL-terminated and outlives the call.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    mode,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    1,
                    512,
                    512,
                    0,
                    None,
                )
            };
            assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
            (Server(handle), iface(name, 0xFF00, 0x0001))
        }

        pub(crate) fn listen_again(&self) {
            let nonblocking = PIPE_READMODE_MESSAGE | PIPE_NOWAIT;
            let blocking = PIPE_READMODE_MESSAGE | PIPE_WAIT;
            // SAFETY: the server handle is open, with no operation using a stack pointer.
            unsafe {
                DisconnectNamedPipe(self.0).unwrap();
                SetNamedPipeHandleState(self.0, Some(&nonblocking), None, None).unwrap();
                let listening = ConnectNamedPipe(self.0, None);
                // The first nonblocking connect may return success while becoming listenable.
                if let Err(error) = listening {
                    assert_eq!(error.code(), ERROR_PIPE_LISTENING.to_hresult());
                }
                SetNamedPipeHandleState(self.0, Some(&blocking), None, None).unwrap();
            }
        }

        pub(crate) fn send(&self, bytes: &[u8]) {
            let mut written = 0;
            // SAFETY: synchronous write from a live buffer on an open handle.
            unsafe { WriteFile(self.0, Some(bytes), Some(&mut written), None) }.unwrap();
            assert_eq!(written as usize, bytes.len());
        }

        pub(crate) fn receive(&self) -> Vec<u8> {
            let mut buffer = [0u8; 64];
            let mut read = 0;
            // SAFETY: synchronous read into a live buffer on an open handle.
            unsafe { ReadFile(self.0, Some(&mut buffer), Some(&mut read), None) }.unwrap();
            buffer[..read as usize].to_vec()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            // SAFETY: the handle came from `CreateNamedPipeW` and is closed only here.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn report(collection: CollectionId, bytes: &[u8]) -> OsEvent {
        OsEvent::Report {
            collection,
            bytes: bytes.to_vec(),
        }
    }

    #[derive(Clone, Copy)]
    struct FeatureCall {
        code: u32,
        input: Option<*const c_void>,
        input_len: u32,
        output: Option<*mut c_void>,
        output_len: u32,
        overlapped: *mut OVERLAPPED,
    }

    thread_local! {
        static FEATURE_CALL: Cell<Option<FeatureCall>> = const { Cell::new(None) };
        static FEATURE_CANCELS: RefCell<Vec<(HANDLE, Option<*const OVERLAPPED>)>> =
            const { RefCell::new(Vec::new()) };
        static FEATURE_NOW: Cell<Option<Ms>> = const { Cell::new(None) };
        static FEATURE_STATUS: Cell<NTSTATUS> = const { Cell::new(STATUS_PENDING) };
    }

    // Substitute only the Windows calls that would need a HID driver. The operation,
    // buffer, packet dequeue and GetOverlappedResult are the production path.
    #[allow(clippy::too_many_arguments)] // Mirrors NtDeviceIoControlFile.
    unsafe fn feature_control(
        _: HANDLE,
        event: Option<HANDLE>,
        apc: PIO_APC_ROUTINE,
        context: Option<*const c_void>,
        iosb: *mut IO_STATUS_BLOCK,
        code: u32,
        input: Option<*const c_void>,
        input_len: u32,
        output: Option<*mut c_void>,
        output_len: u32,
    ) -> NTSTATUS {
        assert!(event.is_none() && apc.is_none());
        assert_eq!(context, Some(iosb.cast_const().cast()));
        let overlapped = iosb.cast::<OVERLAPPED>();
        // SAFETY: submission still owns this initialized OVERLAPPED.
        assert_eq!(unsafe { (*overlapped).Internal }, STATUS_PENDING.0 as usize);
        let status = FEATURE_STATUS.get();
        if status == STATUS_ACCESS_DENIED {
            return status;
        }
        assert!(
            FEATURE_CALL
                .replace(Some(FeatureCall {
                    code,
                    input,
                    input_len,
                    output,
                    output_len,
                    overlapped,
                }))
                .is_none()
        );
        status
    }

    unsafe fn feature_cancel(
        handle: HANDLE,
        overlapped: Option<*const OVERLAPPED>,
    ) -> windows::core::Result<()> {
        FEATURE_CANCELS.with_borrow_mut(|calls| calls.push((handle, overlapped)));
        // Deliberately do not complete here: cancellation can race a successful completion.
        Err(ERROR_NOT_FOUND.into())
    }

    fn feature_clock() -> Ms {
        FEATURE_NOW.get().unwrap_or_else(clock_ms)
    }

    struct PendingCall {
        port: HANDLE,
        key: usize,
        call: Option<FeatureCall>,
    }

    impl PendingCall {
        fn take(reactor: &Reactor, id: CollectionId) -> Self {
            Self {
                port: reactor.port,
                key: reactor.opens[&id].key,
                call: Some(FEATURE_CALL.take().expect("no overlapped feature submission")),
            }
        }

        fn call(&self) -> FeatureCall {
            self.call.unwrap()
        }

        fn buffer(&self) -> &[u8] {
            let call = self.call();
            let (ptr, len) = call.output.map_or_else(
                || (call.input.unwrap(), call.input_len),
                |out| (out.cast_const(), call.output_len),
            );
            // SAFETY: the fake kernel still owns this pending operation and its buffer.
            unsafe { std::slice::from_raw_parts(ptr.cast(), len as usize) }
        }

        fn reply(mut self, bytes: &[u8], transferred: u32) {
            let call = self.call();
            assert!(bytes.len() <= call.output_len as usize);
            // SAFETY: emulate a driver writing its output before queueing the completion.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    call.output.unwrap().cast(),
                    bytes.len(),
                );
            }
            self.complete(0, transferred);
        }

        fn complete(&mut self, status: usize, transferred: u32) {
            let Some(call) = self.call.take() else {
                return;
            };
            // SAFETY: the fake kernel writes the status before releasing ownership to IOCP.
            unsafe {
                (*call.overlapped).Internal = status;
                (*call.overlapped).InternalHigh = transferred as usize;
                PostQueuedCompletionStatus(self.port, transferred, self.key, Some(call.overlapped))
                    .unwrap();
            }
        }
    }

    impl Drop for PendingCall {
        fn drop(&mut self) {
            self.complete(STATUS_CANCELLED.0 as usize, 0);
        }
    }

    fn feature_pipe() -> (Server, HidInterface) {
        let (server, mut pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        pipe.collection.feature_len = 4; // Distinct from the seven-byte output width.
        (server, pipe)
    }

    fn feature_reactor(now: Option<Ms>) -> Reactor {
        FEATURE_NOW.set(now);
        FEATURE_STATUS.set(STATUS_PENDING);
        FEATURE_CALL.set(None);
        FEATURE_CANCELS.with_borrow_mut(Vec::clear);
        let mut reactor = Reactor::new().unwrap();
        reactor.feature_hooks = FeatureHooks {
            control: feature_control,
            cancel: feature_cancel,
            clock: feature_clock,
        };
        reactor
    }

    fn feature_reply(collection: CollectionId, bytes: &[u8], deadline: Ms) -> OsEvent {
        OsEvent::FeatureReply {
            collection,
            bytes: bytes.to_vec(),
            deadline,
        }
    }

    fn feature_failed(
        collection: CollectionId,
        kind: ReportKind,
        reason: FeatureFailure,
    ) -> OsEvent {
        OsEvent::FeatureFailed {
            collection,
            kind,
            reason,
        }
    }

    #[test]
    fn feature_control_uses_hid_directions_and_feature_width() {
        let (_server, pipe) = feature_pipe();
        let id = pipe.collection.id;
        let mut reactor = feature_reactor(Some(10));
        reactor.open(GroupId(1), &pipe, false, true).unwrap();

        for (request, expected) in [
            (&[0x05, 1, 2, 3][..], [0x05, 1, 2, 3]),
            (&[0x00, 1][..], [0x00, 1, 0, 0]),
        ] {
            assert!(reactor.feature(id, ReportKind::SetFeature, request, 50));
            let mut pending = PendingCall::take(&reactor, id);
            let call = pending.call();
            assert_eq!((call.code, call.input_len, call.output_len), (0xb0191, 4, 0));
            assert!(call.input.is_some() && call.output.is_none());
            assert_eq!(pending.buffer(), expected);
            pending.complete(0, 0); // A successful SET transfers no output.
            assert!(reactor.wait(Some(0), 0).is_empty());
        }

        for (report_id, expected) in [
            (0x07, &[0x07, 0xAB, 0xCD][..]),
            (0x00, &[0x00, 0xAB, 0xCD, 0xEF][..]),
        ] {
            assert!(reactor.feature(id, ReportKind::GetFeature, &[report_id], 50));
            let pending = PendingCall::take(&reactor, id);
            let call = pending.call();
            assert_eq!((call.code, call.input_len, call.output_len), (0xb0192, 0, 4));
            assert!(call.input.is_none() && call.output.is_some());
            assert_eq!(pending.buffer(), [report_id, 0, 0, 0]);
            pending.reply(&[report_id, 0xAB, 0xCD, 0xEF], 3);
            assert_eq!(
                reactor.wait(Some(0), 0),
                [feature_reply(id, expected, 50)]
            );
        }
    }

    #[test]
    fn bad_feature_requests_and_completions_are_not_replies() {
        let (_server, pipe) = feature_pipe();
        let (_rx_server, rx) = feature_pipe();
        let id = pipe.collection.id;
        let rx_id = rx.collection.id;
        let unknown = CollectionId(fresh_id());
        let mut reactor = feature_reactor(Some(10));
        reactor.open(GroupId(1), &pipe, false, true).unwrap();
        reactor.open(GroupId(2), &rx, true, false).unwrap();

        for (collection, kind, bytes, deadline) in [
            (unknown, ReportKind::GetFeature, &[3][..], 50),
            (rx_id, ReportKind::SetFeature, &[3][..], 50),
            (rx_id, ReportKind::GetFeature, &[3][..], 50),
            (id, ReportKind::Output, &[3][..], 50),
            (id, ReportKind::GetFeature, &[][..], 50),
            (id, ReportKind::GetFeature, &[3, 1, 2, 3, 4][..], 50),
            (id, ReportKind::GetFeature, &[3][..], 10),
        ] {
            assert!(!reactor.feature(collection, kind, bytes, deadline));
            assert!(FEATURE_CALL.get().is_none());
            assert_eq!(
                reactor.wait(Some(0), 0),
                [feature_failed(collection, kind, FeatureFailure::Rejected)]
            );
        }

        FEATURE_STATUS.set(STATUS_ACCESS_DENIED);
        assert!(!reactor.feature(id, ReportKind::SetFeature, &[3], 50));
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_failed(
                id,
                ReportKind::SetFeature,
                FeatureFailure::Start(ERROR_ACCESS_DENIED.to_hresult())
            )]
        );
        FEATURE_STATUS.set(STATUS_PENDING);

        for (requested_id, reply_id, transferred) in [
            (3, 4, 4),
            (3, 3, 0),
            (3, 3, 5),
            (0, 0, 0),
            (0, 0, 4),
            (0, 1, 3),
        ] {
            assert!(reactor.feature(id, ReportKind::GetFeature, &[requested_id], 50));
            PendingCall::take(&reactor, id).reply(&[reply_id], transferred);
            assert_eq!(
                reactor.wait(Some(0), 0),
                [feature_failed(id, ReportKind::GetFeature, FeatureFailure::InvalidReply)]
            );
        }

        assert!(reactor.feature(id, ReportKind::SetFeature, &[3], 50));
        PendingCall::take(&reactor, id).complete(STATUS_CANCELLED.0 as usize, 0);
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_failed(
                id,
                ReportKind::SetFeature,
                FeatureFailure::Completion(ERROR_OPERATION_ABORTED.to_hresult())
            )]
        );
    }

    #[test]
    fn feature_submission_warning_is_retained_until_dequeue() {
        let (_server, pipe) = feature_pipe();
        let id = pipe.collection.id;
        let mut reactor = feature_reactor(Some(10));
        reactor.open(GroupId(1), &pipe, false, true).unwrap();
        FEATURE_STATUS.set(STATUS_DEVICE_BUSY);

        // Do not post or touch the captured pointer until the reactor confirms it
        // retained the operation. The pre-fix false result means the allocation is gone.
        assert!(reactor.feature(id, ReportKind::GetFeature, &[3], 50));
        let mut pending = PendingCall::take(&reactor, id);
        pending.complete(STATUS_DEVICE_BUSY.0 as usize, 0);
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_failed(
                id,
                ReportKind::GetFeature,
                FeatureFailure::Completion(ERROR_BUSY.to_hresult())
            )]
        );
        assert_eq!(reactor.pending, 0);
    }

    #[test]
    fn feature_submission_on_invalid_handle_fails_immediately() {
        let error = Op::new(CollectionId(fresh_id()), Kind::SetFeature, vec![3])
            .start_feature(HANDLE::default(), hid_io::FEATURE_CONTROL)
            .unwrap_err();
        assert_eq!(error.code(), ERROR_INVALID_HANDLE.to_hresult());
    }

    #[test]
    fn feature_timeout_cancels_only_its_operation_and_waits_for_dequeue_before_reuse() {
        let (server, pipe) = feature_pipe();
        let (_other_server, other) = feature_pipe();
        let id = pipe.collection.id;
        let other_id = other.collection.id;
        let mut reactor = feature_reactor(Some(10));
        reactor.open(GroupId(1), &pipe, true, true).unwrap();
        reactor.open(GroupId(2), &other, false, true).unwrap();
        assert!(reactor.feature(id, ReportKind::GetFeature, &[3], 50));
        let old = PendingCall::take(&reactor, id);
        assert!(reactor.feature(other_id, ReportKind::GetFeature, &[5], 100));
        let unrelated = PendingCall::take(&reactor, other_id);

        FEATURE_NOW.set(Some(50));
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_failed(id, ReportKind::GetFeature, FeatureFailure::TimedOut)]
        );
        let handle = reactor.opens[&id].handle.0;
        FEATURE_CANCELS.with_borrow(|calls| {
            assert_eq!(
                calls.as_slice(),
                &[(handle, Some(old.call().overlapped.cast_const()))]
            );
        });
        assert_eq!(old.buffer(), [3, 0, 0, 0]);
        assert!(!reactor.feature(id, ReportKind::GetFeature, &[3], 100));
        assert!(FEATURE_CALL.get().is_none());
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_failed(id, ReportKind::GetFeature, FeatureFailure::Rejected)]
        );

        server.send(&[0x11, 7]);
        assert_eq!(reactor.wait(Some(1000), 0), [report(id, &[0x11, 7])]);
        unrelated.reply(&[5, 9], 2);
        old.reply(&[3, 8], 2); // It raced with CancelIoEx and succeeded, but is too late.
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_reply(other_id, &[5, 9], 100)]
        );
        assert!(reactor.feature(id, ReportKind::GetFeature, &[3], 100));
        PendingCall::take(&reactor, id).reply(&[3, 1], 2);
        assert_eq!(reactor.wait(Some(0), 0), [feature_reply(id, &[3, 1], 100)]);
    }

    #[test]
    fn feature_deadline_bounds_the_iocp_wait_without_a_hub_wakeup() {
        let (_server, pipe) = feature_pipe();
        let id = pipe.collection.id;
        let mut reactor = feature_reactor(None);
        reactor.open(GroupId(1), &pipe, false, true).unwrap();
        let start = Instant::now();
        let at = clock_ms() + 80;
        assert!(reactor.feature(id, ReportKind::GetFeature, &[3], at));
        let _pending = PendingCall::take(&reactor, id);
        // Keep a distant caller deadline as a backstop; it must not be the one that wakes us.
        let events = loop {
            let left = 1500u64.saturating_sub(start.elapsed().as_millis() as u64);
            let events = reactor.wait(Some(left), 0);
            if !events.is_empty() || left == 0 {
                break events;
            }
        };
        assert!(start.elapsed() < Duration::from_millis(750));
        assert_eq!(
            events,
            [feature_failed(id, ReportKind::GetFeature, FeatureFailure::TimedOut)]
        );
    }

    #[test]
    fn old_feature_completion_after_disconnect_cannot_release_or_answer_successor() {
        let (_server, pipe) = feature_pipe();
        let (_successor_server, mut successor) = feature_pipe();
        let id = pipe.collection.id;
        successor.collection.id = id;
        let mut reactor = feature_reactor(Some(10));
        reactor.open(GroupId(1), &pipe, false, true).unwrap();
        assert!(reactor.feature(id, ReportKind::GetFeature, &[3], 50));
        let old = PendingCall::take(&reactor, id);
        let old_handle = reactor.opens[&id].handle.0;
        reactor.close_group(GroupId(1));
        FEATURE_CANCELS.with_borrow(|calls| assert_eq!(calls.as_slice(), &[(old_handle, None)]));
        assert_eq!(old.buffer(), [3, 0, 0, 0]);

        reactor.open(GroupId(2), &successor, false, true).unwrap();
        assert!(reactor.feature(id, ReportKind::GetFeature, &[3], 50));
        let new = PendingCall::take(&reactor, id);
        old.reply(&[3, 0xAA], 2);
        assert!(reactor.wait(Some(0), 0).is_empty());
        assert!(!reactor.feature(id, ReportKind::GetFeature, &[3], 50));
        assert_eq!(
            reactor.wait(Some(0), 0),
            [feature_failed(id, ReportKind::GetFeature, FeatureFailure::Rejected)]
        );
        new.reply(&[3, 0xBB], 2);
        assert_eq!(reactor.wait(Some(0), 0), [feature_reply(id, &[3, 0xBB], 50)]);
    }

    #[test]
    fn poster_wakes_wait() {
        let mut reactor = Reactor::new().unwrap();
        let poster = reactor.poster();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            poster.post(OsEvent::Wake);
        });
        let start = Instant::now();
        let events = reactor.wait(None, 0);
        assert_eq!(events, vec![OsEvent::Wake]);
        assert!(start.elapsed() < Duration::from_secs(1));
        sender.join().unwrap();
    }

    #[test]
    fn deadline_returns_empty() {
        let mut reactor = Reactor::new().unwrap();
        let start = Instant::now();
        assert_eq!(reactor.wait(Some(50), 0), Vec::new());
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(30), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[test]
    fn timeout_is_the_remaining_time() {
        assert_eq!(timeout_ms(None, 5), INFINITE);
        assert_eq!(timeout_ms(Some(70), 20), 50);
        assert_eq!(timeout_ms(Some(10), 20), 0);
        assert_eq!(timeout_ms(Some(u64::MAX), 0), INFINITE - 1);
    }

    #[test]
    fn posted_events_keep_their_order() {
        let mut reactor = Reactor::new().unwrap();
        let poster = reactor.poster();
        poster.post(OsEvent::Resume);
        poster.post(OsEvent::InterfaceArrived);
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![OsEvent::Resume, OsEvent::InterfaceArrived]
        );
    }

    #[test]
    fn poster_outliving_reactor_drops_events() {
        let reactor = Reactor::new().unwrap();
        let poster = reactor.poster();
        poster.post(OsEvent::Wake);
        drop(reactor);
        poster.post(OsEvent::Resume);
    }

    #[test]
    fn write_to_unknown_collection_fails() {
        let mut reactor = Reactor::new().unwrap();
        assert!(!reactor.write(CollectionId(7), &[0x10]));
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![OsEvent::WriteFailed {
                collection: CollectionId(7)
            }]
        );
    }

    #[test]
    fn mouse_and_keyboard_collections_are_skipped() {
        let mut reactor = Reactor::new().unwrap();
        let missing = wide(r"\\.\pipe\batterysight-test-missing");
        for usage in [0x02, 0x06] {
            let skipped = iface(missing.clone(), 0x01, usage);
            assert!(!reactor.open(GroupId(1), &skipped, true, false).unwrap());
            reactor.write(skipped.collection.id, &[0x10]);
            assert_eq!(
                reactor.wait(Some(0), 0),
                vec![OsEvent::WriteFailed {
                    collection: skipped.collection.id
                }]
            );
        }
        assert!(
            reactor
                .open(GroupId(1), &iface(missing, 0xFF00, 1), true, false)
                .is_err()
        );
    }

    #[test]
    fn reads_are_reissued_and_writes_padded() {
        let (server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        assert!(reactor.open(GroupId(1), &pipe, true, true).unwrap());
        assert!(!reactor.open(GroupId(1), &pipe, true, true).unwrap());

        server.send(&[0x11, 1, 2, 3, 4, 5, 6]);
        assert_eq!(
            reactor.wait(Some(1000), 0),
            vec![report(id, &[0x11, 1, 2, 3, 4, 5, 6])]
        );
        server.send(&[0x10, 9, 8]);
        assert_eq!(reactor.wait(Some(1000), 0), vec![report(id, &[0x10, 9, 8])]);

        assert!(reactor.write(id, &[0x10, 0x01, 0x02]));
        assert_eq!(server.receive(), [0x10, 0x01, 0x02, 0, 0, 0, 0]);
        assert_eq!(reactor.wait(Some(50), 0), Vec::new());
    }

    #[test]
    fn oversized_write_fails() {
        let (_server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true, true).unwrap();
        assert!(!reactor.write(id, &[0; 8]));
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![OsEvent::WriteFailed { collection: id }]
        );
    }

    #[test]
    fn close_group_cancels_pending_read() {
        let (_server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(3), &pipe, true, false).unwrap();
        reactor.close_group(GroupId(3));
        reactor.write(id, &[0x10]);
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![OsEvent::WriteFailed { collection: id }]
        );
    }

    #[test]
    fn close_group_keeps_other_groups_events() {
        let (_server_a, pipe_a) = Server::new(PIPE_ACCESS_DUPLEX);
        let (server_b, pipe_b) = Server::new(PIPE_ACCESS_DUPLEX);
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe_a, true, false).unwrap();
        reactor.open(GroupId(2), &pipe_b, true, false).unwrap();
        let poster = reactor.poster();

        server_b.send(&[0x11, 7]);
        poster.post(OsEvent::Wake);
        reactor.close_group(GroupId(1));
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![report(pipe_b.collection.id, &[0x11, 7]), OsEvent::Wake]
        );
    }

    #[test]
    fn disconnect_fails_read_without_reissue() {
        let (server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true, false).unwrap();
        drop(server);
        assert_eq!(
            reactor.wait(Some(1000), 0),
            vec![OsEvent::ReadFailed { collection: id }]
        );
        assert_eq!(reactor.wait(Some(50), 0), Vec::new());
    }

    #[test]
    fn read_aborted_from_outside_fails_without_reissue() {
        let (_server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true, false).unwrap();
        // Something other than `close_group` aborts the pending read.
        // SAFETY: the reactor's handle is open.
        unsafe { CancelIoEx(reactor.opens[&id].handle.0, None) }.unwrap();
        assert_eq!(
            reactor.wait(Some(1000), 0),
            vec![OsEvent::ReadFailed { collection: id }]
        );
        assert_eq!(reactor.wait(Some(50), 0), Vec::new());
    }

    #[test]
    fn oversized_report_fails_read_once() {
        let (server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true, true).unwrap();
        // Message read mode makes a short read fail with ERROR_MORE_DATA, as a HID read into
        // a too-small buffer does (the client end of a pipe starts in byte mode).
        let mode = PIPE_READMODE_MESSAGE;
        // SAFETY: the reactor's client handle is open; `mode` outlives the call.
        unsafe { SetNamedPipeHandleState(reactor.opens[&id].handle.0, Some(&mode), None, None) }
            .unwrap();
        // The first message completes the pending read; the second is already queued in the
        // pipe when the read is re-issued, so that 7-byte read fails synchronously with
        // ERROR_MORE_DATA and still queues a completion packet.
        server.send(&[0x10, 1, 2, 3, 4, 5, 6]);
        server.send(&[0x11, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            reactor.wait(Some(1000), 0),
            vec![report(id, &[0x10, 1, 2, 3, 4, 5, 6])]
        );
        assert_eq!(
            reactor.wait(Some(1000), 0),
            vec![OsEvent::ReadFailed { collection: id }]
        );
        assert_eq!(reactor.wait(Some(50), 0), Vec::new());
        drop(reactor);
    }

    #[test]
    fn unterminated_path_is_refused() {
        let (_server, mut pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        pipe.path.pop();
        let mut reactor = Reactor::new().unwrap();
        let error = reactor.open(GroupId(1), &pipe, true, false).unwrap_err();
        assert_eq!(error.code(), ERROR_INVALID_PARAMETER.to_hresult());
        assert!(reactor.opens.is_empty());
    }

    #[test]
    fn read_only_collection_still_reads() {
        // The client end of an outbound pipe refuses write access, as a HID collection may.
        let (server, pipe) = Server::new(PIPE_ACCESS_OUTBOUND);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true, false).unwrap();
        assert!(!reactor.opens[&id].writable);
        server.send(&[0x10, 1]);
        assert_eq!(reactor.wait(Some(1000), 0), vec![report(id, &[0x10, 1])]);
        assert!(!reactor.write(id, &[0x10]));
        assert_eq!(
            reactor.wait(Some(0), 0),
            vec![OsEvent::WriteFailed { collection: id }]
        );
    }

    /// Manual hardware check, run with
    /// `cargo test -p batterysight-win reads_logitech_receiver -- --ignored --nocapture`.
    ///
    /// Pings receiver slot 1 through the C547's `FF00:0001` collection and prints every
    /// report of the receiver's groups for about 2 s.
    #[test]
    #[ignore = "talks to the Logitech receiver on this machine"]
    fn reads_logitech_receiver() {
        let mut reactor = Reactor::new().unwrap();
        let ping = fap(0x10, 0x01, 0x00, 1, &[0, 0, 0x5A]);
        let mut pinged = 0;
        for (hid_group, members) in group(enumerate().expect("enumeration failed")) {
            let short = members.iter().find(|m| {
                let c = &m.collection;
                c.vid == 0x046D && c.pid == 0xC547 && c.usage_page == 0xFF00 && c.usage == 0x0001
            });
            let Some(short) = short else { continue };
            for member in &members {
                let c = &member.collection;
                let opened = reactor.open(hid_group.id, member, true, c.id == short.collection.id);
                println!(
                    "open group {} collection {} {:04X}:{:04X} in {} out {} -> {:?}",
                    hid_group.id.0,
                    c.id.0,
                    c.usage_page,
                    c.usage,
                    c.input_len,
                    c.output_len,
                    opened
                );
            }
            println!(
                "write {:02X?} to collection {}",
                ping, short.collection.id.0
            );
            reactor.write(short.collection.id, &ping);
            pinged += 1;
        }
        assert!(pinged > 0, "no C547 receiver with an FF00:0001 collection");

        let start = Instant::now();
        let end = 2000;
        loop {
            let now = start.elapsed().as_millis() as Ms;
            if now >= end {
                break;
            }
            for event in reactor.wait(Some(end), now) {
                match event {
                    OsEvent::Report { collection, bytes } => {
                        println!("{now:>5} ms  collection {}  {:02X?}", collection.0, bytes)
                    }
                    other => println!("{now:>5} ms  {other:?}"),
                }
            }
        }
    }
}
