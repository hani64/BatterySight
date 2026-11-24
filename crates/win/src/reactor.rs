//! One-thread I/O completion port reactor for HID reads and writes and posted events.
//!
//! Every opened collection's handle is associated with one completion port under
//! [`KEY_IO`]; each overlapped operation is an [`Op`] identified by its `OVERLAPPED` pointer.
//! Other threads hand events to the reactor through a [`Poster`], which queues a packet with
//! [`KEY_POSTED`] whose `OVERLAPPED` slot carries a leaked `Box<OsEvent>`; the reactor takes
//! the box back when it dequeues the packet, or when it drains the port on drop.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem;
use std::sync::{Arc, PoisonError, RwLock};

use batterysight_core::hid::{CollectionId, GroupId};
use batterysight_core::time::Ms;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
    PostQueuedCompletionStatus,
};

use crate::hid_enum::{ContainerId, HidInterface, OwnedHandle};
use crate::hid_io::{self, Kind, Op};

/// Completion key of every HID handle; the packet's `OVERLAPPED` pointer is an [`Op`].
const KEY_IO: usize = 1;
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

/// One opened collection.
struct Open {
    group: GroupId,
    handle: OwnedHandle,
    /// Output report length; writes are refused when the handle is read-only.
    output_len: usize,
    writable: bool,
    /// Operations started and not yet dequeued.
    pending: usize,
    /// Set by `close_where`: completions are dropped and nothing is re-issued.
    closing: bool,
}

/// The completion port as shared with posters: the raw handle, or 0 once it is closed.
struct Port(RwLock<usize>);

/// Overlapped HID I/O and posted events, all completed through one completion port and
/// consumed on one thread.
pub struct Reactor {
    port: HANDLE,
    shared: Arc<Port>,
    opens: HashMap<CollectionId, Open>,
    /// Events produced outside `wait` (failed starts, completions dequeued while closing a
    /// group); returned first by the next `wait`.
    backlog: Vec<OsEvent>,
}

impl Reactor {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: creates a new port; no file handle is involved.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1) }?;
        Ok(Reactor {
            port,
            shared: Arc::new(Port(RwLock::new(port.0 as usize))),
            opens: HashMap::new(),
            backlog: Vec::new(),
        })
    }

    /// Opens `iface` as part of `group`, arming input only for a selected receive collection.
    ///
    /// Mouse and keyboard collections are skipped: Windows holds them exclusively. Opening a
    /// collection that is already open does nothing. A read that cannot be started is
    /// reported as `ReadFailed` by the next `wait`.
    pub fn open(
        &mut self,
        group: GroupId,
        iface: &HidInterface,
        read: bool,
    ) -> windows::core::Result<()> {
        let collection = &iface.collection;
        if hid_io::is_exclusive(collection.usage_page, collection.usage)
            || self.opens.contains_key(&collection.id)
        {
            return Ok(());
        }
        let (handle, writable) = hid_io::open(&iface.path)?;
        // SAFETY: both handles are open; the port outlives the file handle.
        unsafe { CreateIoCompletionPort(handle.0, Some(self.port), KEY_IO, 0) }?;
        let mut open = Open {
            group,
            handle,
            output_len: usize::from(collection.output_len),
            writable,
            pending: 0,
            closing: false,
        };
        let input_len = usize::from(collection.input_len);
        if read && input_len > 0 {
            if Op::new(collection.id, Kind::Read, vec![0; input_len]).start(open.handle.0) {
                open.pending += 1;
            } else {
                self.backlog.push(OsEvent::ReadFailed {
                    collection: collection.id,
                });
            }
        }
        self.opens.insert(collection.id, open);
        Ok(())
    }

    /// Cancels the I/O of every collection in `group`, waits until each cancelled operation
    /// has completed, and closes the handles.
    ///
    /// Completions of the group's own operations are dropped. Completions of other
    /// collections and posted events dequeued meanwhile are kept for the next `wait`.
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
        if !started {
            self.backlog.push(OsEvent::WriteFailed { collection });
        }
        started
    }

    pub fn poster(&self) -> Poster {
        Poster(Arc::clone(&self.shared))
    }

    /// Waits until something happens or `deadline` passes (no deadline: wait indefinitely)
    /// and returns every event gathered, oldest first.
    ///
    /// Returns at once when earlier events are pending. May return an empty list before the
    /// deadline when only internal completions (finished writes) arrived; callers loop.
    pub fn wait(&mut self, deadline: Option<Ms>, now: Ms) -> Vec<OsEvent> {
        let mut events = mem::take(&mut self.backlog);
        let timeout = if events.is_empty() {
            timeout_ms(deadline, now)
        } else {
            0
        };
        self.dequeue(timeout, &mut events);
        events
    }

    /// Closes every collection matching `closing`; see [`Reactor::close_group`].
    fn close_where(&mut self, closing: impl Fn(&Open) -> bool) {
        for open in self.opens.values_mut().filter(|open| closing(open)) {
            open.closing = true;
            if open.pending > 0 {
                // SAFETY: the handle is open. Failure means nothing was left to cancel.
                let _ = unsafe { CancelIoEx(open.handle.0, None) };
            }
        }
        // Buffers and OVERLAPPEDs may be freed only after their completion is dequeued, so
        // this waits without a timeout; cancelled HID reads complete promptly.
        let mut kept = mem::take(&mut self.backlog);
        while self
            .opens
            .values()
            .any(|open| open.closing && open.pending > 0)
        {
            if self.dequeue(INFINITE, &mut kept) == 0 {
                break;
            }
        }
        self.backlog = kept;
        self.opens.retain(|_, open| !open.closing);
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
                KEY_IO => {
                    // SAFETY: `KEY_IO` packets come from `Op::start`, each dequeued once.
                    let op = unsafe { Op::from_completion(entry.lpOverlapped) };
                    self.complete(op, out);
                }
                _ => {}
            }
        }
        count
    }

    /// Handles one finished operation: reports and re-issues reads, reports failures.
    fn complete(&mut self, op: Box<Op>, out: &mut Vec<OsEvent>) {
        let Some(open) = self.opens.get_mut(&op.collection) else {
            return;
        };
        open.pending -= 1;
        if open.closing {
            return;
        }
        let collection = op.collection;
        match (op.kind, op.result(open.handle.0)) {
            (Kind::Read, Ok(read)) => {
                out.push(OsEvent::Report {
                    collection,
                    bytes: op.buffer[..read].to_vec(),
                });
                if op.start(open.handle.0) {
                    open.pending += 1;
                } else {
                    out.push(OsEvent::ReadFailed { collection });
                }
            }
            // Includes a read aborted by anything but `close_where` (whose collections returned
            // above): nothing would read the collection again, so the runtime must reopen it.
            (Kind::Read, Err(_)) => out.push(OsEvent::ReadFailed { collection }),
            (Kind::Write, Ok(_)) => {}
            (Kind::Write, Err(_)) => out.push(OsEvent::WriteFailed { collection }),
        }
    }
}

impl Drop for Reactor {
    /// Cancels and drains all I/O, frees events still queued, then closes the port. Posters
    /// that outlive the reactor drop their events from then on.
    fn drop(&mut self) {
        self.close_where(|_| true);
        let shared = Arc::clone(&self.shared);
        let mut port = shared.0.write().unwrap_or_else(PoisonError::into_inner);
        let mut discarded = Vec::new();
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use batterysight_core::engines::hidpp::frame::fap;
    use batterysight_core::hid::Collection;
    use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX, PIPE_ACCESS_OUTBOUND, ReadFile, WriteFile,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
        SetNamedPipeHandleState,
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

        pub(crate) fn send(&self, bytes: &[u8]) {
            let mut written = 0;
            // SAFETY: synchronous write from a live buffer on an open handle.
            unsafe { WriteFile(self.0, Some(bytes), Some(&mut written), None) }.unwrap();
            assert_eq!(written as usize, bytes.len());
        }

        fn receive(&self) -> Vec<u8> {
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
            reactor.open(GroupId(1), &skipped, true).unwrap();
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
                .open(GroupId(1), &iface(missing, 0xFF00, 1), true)
                .is_err()
        );
    }

    #[test]
    fn reads_are_reissued_and_writes_padded() {
        let (server, pipe) = Server::new(PIPE_ACCESS_DUPLEX);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true).unwrap();

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
        reactor.open(GroupId(1), &pipe, true).unwrap();
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
        reactor.open(GroupId(3), &pipe, true).unwrap();
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
        reactor.open(GroupId(1), &pipe_a, true).unwrap();
        reactor.open(GroupId(2), &pipe_b, true).unwrap();
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
        reactor.open(GroupId(1), &pipe, true).unwrap();
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
        reactor.open(GroupId(1), &pipe, true).unwrap();
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
        reactor.open(GroupId(1), &pipe, true).unwrap();
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
        let error = reactor.open(GroupId(1), &pipe, true).unwrap_err();
        assert_eq!(error.code(), ERROR_INVALID_PARAMETER.to_hresult());
        assert!(reactor.opens.is_empty());
    }

    #[test]
    fn read_only_collection_still_reads() {
        // The client end of an outbound pipe refuses write access, as a HID collection may.
        let (server, pipe) = Server::new(PIPE_ACCESS_OUTBOUND);
        let id = pipe.collection.id;
        let mut reactor = Reactor::new().unwrap();
        reactor.open(GroupId(1), &pipe, true).unwrap();
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
                let opened = reactor.open(hid_group.id, member, true);
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
