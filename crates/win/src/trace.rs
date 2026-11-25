//! Troubleshooting log of every report the runtime sends and reads, of audio activity, of
//! groups held back or given up and of UI events.
//!
//! The app enables tracing at startup. Each write the reactor started and each input report
//! read is appended to `%LOCALAPPDATA%\BatterySight\sends.log` as
//! `"<unix ms> <out|in> <collection id> <hex>"`, written on the device thread, which also
//! adds `"<unix ms> audio <container hex> active|idle"` when a device's audio sessions start
//! or stop (`"<unix ms> audio unavailable <stage>"` when they cannot be watched),
//! `"<unix ms> held <paths> until <unix ms>"` when the reopen cap holds back the group with
//! those comma-separated interface paths, `"<unix ms> released <paths>"` when it opens
//! again and `"<unix ms> open failed <paths>"` when none of a group's HID++ collections
//! open, and `"<unix ms> discovery <group key> failed, retry at <unix ms>"` or
//! `"<unix ms> discovery <group key> gave up"` when a group's discovery was cut short.
//! A refused request is logged as `"<unix ms> refused write <kind> <collection id> <reason>"`;
//! this line contains no report bytes, paths, or device identity.
//! Feature records use `feature-set`, `feature-get` and `feature-reply` in place of `out`
//! and `in`. A feature failure records kind, collection and reason, not the report bytes.
//! The UI adds `"<unix ms> ui <text>"` lines ([`ui_line`]) and the settings and history
//! store `"<unix ms> store <text>"` lines when a file cannot be read or written or is set
//! aside. Tracing only records what the runtime already does, so it adds no device
//! traffic, and it never fails the app: an unwritable file just means no trace. When the log
//! exceeds 1 MiB at a size check, it becomes `sends.old.log`, replacing the previous log.

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use batterysight_core::hid::{CollectionId, ReportKind};
use batterysight_core::hub::WriteRefusal;
use batterysight_core::model::DeviceKey;
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::hid_enum::{ContainerId, container_hex};
use crate::reactor::FeatureFailure;

const CAP_BYTES: u64 = 1_048_576;
const RECHECK_BYTES: u64 = 65_536;
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Enables the troubleshooting log for this process. Only the app entry point calls this.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn rotate_if_needed(dir: &Path, cap: u64) {
    let current = dir.join("sends.log");
    let oversized = || fs::metadata(&current).is_ok_and(|meta| meta.len() > cap);
    if !oversized() {
        return;
    }

    // Never lock the log itself: other processes must remain free to append to it.
    let Ok(guard) = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("sends.log.lock"))
    else {
        return;
    };
    if guard.try_lock().is_err() {
        return;
    }

    // A different writer may have already rotated and opened a new, small log.
    if oversized() {
        let _ = fs::rename(&current, dir.join("sends.old.log"));
    }
}

fn open_log(dir: &Path) -> Option<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sends.log"))
        .ok()
}

fn open_log_for_oversized_record(dir: &Path) -> Option<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .open(dir.join("sends.log"))
        .ok()
}

/// The open trace file: the device thread's, held for the runtime's life, or one opened
/// for a single [`ui_line`] or [`store_line`].
pub(crate) struct Trace {
    file: File,
    dir: PathBuf,
    written: u64,
    #[cfg(test)]
    cap: u64,
    #[cfg(test)]
    recheck_bytes: u64,
}

impl Trace {
    /// The trace file if logging is enabled and the file can be opened.
    pub(crate) fn open() -> Option<Trace> {
        if !enabled() {
            return None;
        }
        let dir = PathBuf::from(env::var_os("LOCALAPPDATA")?).join("BatterySight");
        Self::open_in(&dir)
    }

    fn open_in(dir: &Path) -> Option<Trace> {
        Self::open_in_with_cap(dir, CAP_BYTES)
    }

    fn open_in_with_cap(dir: &Path, cap: u64) -> Option<Trace> {
        fs::create_dir_all(dir).ok()?;
        rotate_if_needed(dir, cap);
        Some(Trace {
            file: open_log(dir)?,
            dir: dir.to_path_buf(),
            written: 0,
            #[cfg(test)]
            cap,
            #[cfg(test)]
            recheck_bytes: RECHECK_BYTES,
        })
    }

    fn append(&mut self, bytes: &[u8]) {
        #[cfg(not(test))]
        let (cap, recheck_bytes) = (CAP_BYTES, RECHECK_BYTES);
        #[cfg(test)]
        let (cap, recheck_bytes) = (self.cap, self.recheck_bytes);
        #[cfg(test)]
        if self.dir.as_os_str().is_empty() {
            let _ = self.file.write_all(bytes);
            return;
        }

        let len = bytes.len() as u64;
        if len > recheck_bytes {
            // A record larger than the budget cannot risk a rename between open and write.
            if let Some(mut file) = open_log_for_oversized_record(&self.dir) {
                let _ = file.write_all(bytes);
                drop(file);
                self.recheck(cap);
            }
            return;
        }
        if self.written + len > recheck_bytes {
            self.recheck(cap);
        }
        let _ = self.file.write_all(bytes);
        // Count attempted bytes even on error so a stale handle does not prevent a reopen.
        self.written += len;
        if self.written >= recheck_bytes {
            self.recheck(cap);
        }
    }

    fn recheck(&mut self, cap: u64) {
        self.written = 0;
        rotate_if_needed(&self.dir, cap);
        // Reopen even if this call didn't rotate: another writer may have done so.
        if let Some(file) = open_log(&self.dir) {
            self.file = file;
        }
    }

    /// Appends one frame; write errors are ignored.
    pub(crate) fn record(&mut self, direction: Direction, collection: CollectionId, bytes: &[u8]) {
        let mut text = line(unix_ms(), direction, collection, bytes);
        text.push('\n');
        self.append(text.as_bytes());
    }

    /// Appends only the typed refusal metadata, never the request data.
    pub(crate) fn write_refused(
        &mut self,
        kind: ReportKind,
        collection: CollectionId,
        reason: WriteRefusal,
    ) {
        let text = format!(
            "{} refused write {kind:?} {} {reason:?}\n",
            unix_ms(),
            collection.0,
        );
        self.append(text.as_bytes());
    }

    pub(crate) fn feature_failed(
        &mut self,
        collection: CollectionId,
        kind: ReportKind,
        reason: FeatureFailure,
    ) {
        let text = format!(
            "{} feature failed {kind:?} {} {reason:?}\n",
            unix_ms(),
            collection.0,
        );
        self.append(text.as_bytes());
    }

    /// Appends an audio activity change of `container`; write errors are ignored.
    pub(crate) fn audio(&mut self, container: &ContainerId, active: bool) {
        let mut text = audio_text(unix_ms(), container, active);
        text.push('\n');
        self.append(text.as_bytes());
    }

    /// Appends that audio activity is not (fully) watched because `stage` failed; write
    /// errors are ignored.
    pub(crate) fn audio_unavailable(&mut self, stage: &str) {
        let mut text = audio_unavailable_text(unix_ms(), stage);
        text.push('\n');
        self.append(text.as_bytes());
    }

    /// Appends that the reopen cap holds back the group with interface `paths` for
    /// `release_in` ms more; write errors are ignored.
    pub(crate) fn held(&mut self, paths: &BTreeSet<String>, release_in: u64) {
        let now = unix_ms();
        let mut text = held_text(now, paths, now.saturating_add(release_in));
        text.push('\n');
        self.append(text.as_bytes());
    }

    /// Appends that the group with interface `paths`, held back before, was opened; write
    /// errors are ignored.
    pub(crate) fn released(&mut self, paths: &BTreeSet<String>) {
        let mut text = released_text(unix_ms(), paths);
        text.push('\n');
        self.append(text.as_bytes());
    }

    /// Appends that none of the HID++ collections of the group with interface `paths`
    /// opened; write errors are ignored.
    pub(crate) fn open_failed(&mut self, paths: &BTreeSet<String>) {
        let mut text = open_failed_text(unix_ms(), paths);
        text.push('\n');
        self.append(text.as_bytes());
    }

    /// Appends that `group`'s discovery registered nothing and is retried `retry_in` ms
    /// from now, or never (`None`); write errors are ignored.
    pub(crate) fn discovery_failed(&mut self, group: &DeviceKey, retry_in: Option<u64>) {
        let now = unix_ms();
        let retry_at = retry_in.map(|retry_in| now.saturating_add(retry_in));
        let mut text = discovery_text(now, group, retry_at);
        text.push('\n');
        self.append(text.as_bytes());
    }

    #[cfg(test)]
    fn open_with_limits(dir: &Path, cap: u64, recheck_bytes: u64) -> Option<Trace> {
        let mut trace = Self::open_in_with_cap(dir, cap)?;
        trace.recheck_bytes = recheck_bytes;
        Some(trace)
    }

    /// A trace appending to `file`.
    #[cfg(test)]
    pub(crate) fn to_file(file: File) -> Trace {
        Trace {
            file,
            dir: PathBuf::new(),
            written: 0,
            cap: CAP_BYTES,
            recheck_bytes: RECHECK_BYTES,
        }
    }
}

/// Appends `"<unix ms> ui <text>"` to the trace file, ignoring errors. Only rare UI events
/// use this small append, which runs on the calling thread.
pub fn ui_line(text: &str) {
    if let Some(mut trace) = Trace::open() {
        let mut text = ui_text(unix_ms(), text);
        text.push('\n');
        trace.append(text.as_bytes());
    }
}

/// Appends `"<unix ms> store <text>"` like [`ui_line`]; called from the store's writer
/// thread, and at startup before any window exists.
pub(crate) fn store_line(text: &str) {
    if let Some(mut trace) = Trace::open() {
        let mut text = store_text(unix_ms(), text);
        text.push('\n');
        trace.append(text.as_bytes());
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Which way a traced frame went.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    /// A write the reactor started.
    Out,
    /// An input report as read.
    In,
    FeatureSet,
    FeatureGet,
    FeatureReply,
}

/// `"<unix ms> <out|in> <collection id> <hex>"`, the hex in uppercase pairs without
/// separators.
fn line(unix_ms: u64, direction: Direction, collection: CollectionId, bytes: &[u8]) -> String {
    let direction = match direction {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::FeatureSet => "feature-set",
        Direction::FeatureGet => "feature-get",
        Direction::FeatureReply => "feature-reply",
    };
    let mut text = format!("{unix_ms} {direction} {} ", collection.0);
    for byte in bytes {
        let _ = write!(text, "{byte:02X}");
    }
    text
}

/// `"<unix ms> ui <text>"`.
fn ui_text(unix_ms: u64, text: &str) -> String {
    format!("{unix_ms} ui {text}")
}

/// `"<unix ms> store <text>"`.
fn store_text(unix_ms: u64, text: &str) -> String {
    format!("{unix_ms} store {text}")
}

/// `"<unix ms> audio <container hex> active|idle"`.
fn audio_text(unix_ms: u64, container: &ContainerId, active: bool) -> String {
    let state = if active { "active" } else { "idle" };
    format!("{unix_ms} audio {} {state}", container_hex(container))
}

/// `"<unix ms> audio unavailable <stage>"`.
fn audio_unavailable_text(unix_ms: u64, stage: &str) -> String {
    format!("{unix_ms} audio unavailable {stage}")
}

/// `"<unix ms> held <paths> until <unix ms>"`, the paths separated by commas.
fn held_text(unix_ms: u64, paths: &BTreeSet<String>, until: u64) -> String {
    format!("{unix_ms} held {} until {until}", joined(paths))
}

/// `"<unix ms> released <paths>"`, the paths separated by commas.
fn released_text(unix_ms: u64, paths: &BTreeSet<String>) -> String {
    format!("{unix_ms} released {}", joined(paths))
}

/// `"<unix ms> open failed <paths>"`, the paths separated by commas.
fn open_failed_text(unix_ms: u64, paths: &BTreeSet<String>) -> String {
    format!("{unix_ms} open failed {}", joined(paths))
}

/// `"<unix ms> discovery <group key> failed, retry at <unix ms>"`, or
/// `"<unix ms> discovery <group key> gave up"` without a retry.
fn discovery_text(unix_ms: u64, group: &DeviceKey, retry_at: Option<u64>) -> String {
    match retry_at {
        Some(at) => format!("{unix_ms} discovery {} failed, retry at {at}", group.0),
        None => format!("{unix_ms} discovery {} gave up", group.0),
    }
}

fn joined(paths: &BTreeSet<String>) -> String {
    paths
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!("bs-trace-{}-{name}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn disabled_by_default_writes_nothing() {
        // No test calls enable(), so the default is independent of test order.
        assert!(!enabled());
        assert!(Trace::open().is_none());
    }

    #[test]
    fn rotate_if_needed_renames_only_over_cap() {
        let dir = TempDir::new("cap");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        fs::write(&current, b"1234").unwrap();
        rotate_if_needed(&dir.0, 4);
        assert_eq!(fs::read(&current).unwrap(), b"1234");
        assert!(!old.exists());

        fs::write(&current, b"12345").unwrap();
        rotate_if_needed(&dir.0, 4);
        assert!(!current.exists());
        assert_eq!(fs::read(&old).unwrap(), b"12345");
    }

    #[test]
    fn rotation_replaces_existing_old_file() {
        let dir = TempDir::new("replace");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        fs::write(&old, b"old").unwrap();
        fs::write(&current, b"newer").unwrap();
        rotate_if_needed(&dir.0, 4);
        assert!(!current.exists());
        assert_eq!(fs::read(&old).unwrap(), b"newer");
    }

    #[test]
    fn second_rotation_preserves_archive_when_current_log_is_small() {
        let dir = TempDir::new("second-rotation");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        fs::write(&current, b"oversized").unwrap();
        rotate_if_needed(&dir.0, 4);

        fs::write(&current, b"new").unwrap();
        rotate_if_needed(&dir.0, 4);
        assert_eq!(fs::read(&current).unwrap(), b"new");
        assert_eq!(fs::read(&old).unwrap(), b"oversized");
    }

    #[test]
    fn rotation_skips_while_guard_is_held() {
        let dir = TempDir::new("rotation-contended");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        fs::write(&current, b"oversized").unwrap();
        fs::write(&old, b"archive").unwrap();
        let guard = File::create(dir.0.join("sends.log.lock")).unwrap();
        guard.try_lock().unwrap();

        rotate_if_needed(&dir.0, 4);
        assert_eq!(fs::read(&current).unwrap(), b"oversized");
        assert_eq!(fs::read(&old).unwrap(), b"archive");

        drop(guard);
        rotate_if_needed(&dir.0, 4);
        assert!(!current.exists());
        assert_eq!(fs::read(&old).unwrap(), b"oversized");
    }

    #[test]
    fn long_lived_trace_moves_to_new_file_after_external_rotation() {
        const EARLY: &[u8] = b"early line..............\n"; // 25 bytes
        const LATER: &[u8] = b"later line..............\n";
        let dir = TempDir::new("long-lived");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        let external = dir.0.join("external.log");
        let mut trace = Trace::open_with_limits(&dir.0, 100, 50).unwrap();

        for _ in 0..4 {
            trace.append(EARLY);
        }
        assert_eq!(fs::read(&current).unwrap(), EARLY.repeat(4));
        assert!(!old.exists());
        for _ in 0..2 {
            trace.append(EARLY);
        }
        trace.append(LATER);
        assert_eq!(fs::read(&old).unwrap(), EARLY.repeat(6));
        assert_eq!(fs::read(&current).unwrap(), LATER);

        fs::rename(&current, &external).unwrap();
        trace.append(LATER);
        trace.append(b"after external rotation\n");
        assert_eq!(fs::read(&external).unwrap(), LATER.repeat(2));
        assert_eq!(fs::read(&current).unwrap(), b"after external rotation\n");
    }

    #[test]
    fn external_rotation_reopens_before_a_record_would_cross_the_budget() {
        let first: &[u8; 30] = b"first record.................\n";
        let next: &[u8; 30] = b"next record..................\n";
        let dir = TempDir::new("non-divisible");
        let current = dir.0.join("sends.log");
        let external = dir.0.join("external.log");
        let mut trace = Trace::open_with_limits(&dir.0, 100, 50).unwrap();
        fs::rename(&current, &external).unwrap();

        trace.append(first);
        trace.append(next);

        assert_eq!(fs::read(&external).unwrap(), first);
        assert_eq!(fs::read(&current).unwrap(), next);
    }

    #[test]
    fn oversized_record_goes_to_the_current_log() {
        let dir = TempDir::new("oversized-current");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        let mut trace = Trace::open_with_limits(&dir.0, 100, 50).unwrap();
        trace.append(b"earlier\n");
        fs::rename(&current, &old).unwrap();
        let record = b"this oversized record is longer than the budget...\n";

        trace.append(record);

        assert_eq!(fs::read(&current).unwrap(), record);
        assert_eq!(fs::read(&old).unwrap(), b"earlier\n");
    }

    #[test]
    fn oversized_record_handle_refuses_rename_until_closed() {
        let dir = TempDir::new("oversized-share-mode");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        let mut file = open_log_for_oversized_record(&dir.0).unwrap();
        assert!(fs::rename(&current, &old).is_err());
        file.write_all(b"record\n").unwrap();
        assert!(!old.exists());

        drop(file);
        fs::rename(&current, &old).unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"record\n");
    }

    #[test]
    fn oversized_record_is_dropped_when_current_log_cannot_open() {
        let dir = TempDir::new("oversized-locked");
        let current = dir.0.join("sends.log");
        let old = dir.0.join("sends.old.log");
        let mut trace = Trace::open_with_limits(&dir.0, 100, 50).unwrap();
        trace.append(b"earlier\n");
        fs::rename(&current, &old).unwrap();
        fs::write(&current, b"current\n").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&current)
            .unwrap();
        let record = b"this oversized record is longer than the budget...\n";

        trace.append(record);

        assert_eq!(fs::read(&old).unwrap(), b"earlier\n");
        drop(lock);
        assert_eq!(fs::read(&current).unwrap(), b"current\n");
    }

    #[test]
    fn open_in_missing_dir_returns_none() {
        let dir = TempDir::new("not-a-directory");
        let file = dir.0.join("file");
        fs::write(&file, b"file").unwrap();
        assert!(Trace::open_in(&file.join("child")).is_none());
    }

    #[test]
    fn failed_rotation_and_reopen_leave_trace_writable() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = TempDir::new("locked");
        let current = dir.0.join("sends.log");
        let external = dir.0.join("external.log");
        let mut trace = Trace::open_with_limits(&dir.0, 100, 50).unwrap();
        fs::rename(&current, &external).unwrap();
        fs::write(&current, [b'x'; 101]).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&current)
            .unwrap();

        let line = b"still writing...........\n"; // 25 bytes
        trace.append(line);
        trace.append(line);
        trace.append(line);
        assert_eq!(fs::read(&external).unwrap(), line.repeat(3));

        drop(lock);
        assert_eq!(fs::read(&current).unwrap(), [b'x'; 101]);
        assert!(!dir.0.join("sends.old.log").exists());
    }

    #[test]
    fn out_line_has_time_direction_collection_and_hex() {
        assert_eq!(
            line(
                1_763_900_000_123,
                Direction::Out,
                CollectionId(42),
                &[0x10, 0xFF, 0x81, 0x00, 0x0A]
            ),
            "1763900000123 out 42 10FF81000A"
        );
    }

    #[test]
    fn in_line() {
        assert_eq!(
            line(7, Direction::In, CollectionId(8), &[0x11, 0xFF, 0x06, 0x0B]),
            "7 in 8 11FF060B"
        );
    }

    #[test]
    fn line_of_empty_frame() {
        assert_eq!(line(5, Direction::Out, CollectionId(1), &[]), "5 out 1 ");
    }

    #[test]
    fn ui_line_has_time_and_text() {
        assert_eq!(
            ui_text(1_763_900_000_123, "fallback NoAnchor"),
            "1763900000123 ui fallback NoAnchor"
        );
    }

    #[test]
    fn store_line_has_time_and_text() {
        assert_eq!(
            store_text(12, "save failed: denied"),
            "12 store save failed: denied"
        );
    }

    #[test]
    fn audio_line_has_time_container_and_state() {
        let mut container = [0u8; 16];
        container[0] = 0xAB;
        container[15] = 0x01;
        assert_eq!(
            audio_text(9, &container, true),
            "9 audio AB000000000000000000000000000001 active"
        );
        assert_eq!(
            audio_text(10, &container, false),
            "10 audio AB000000000000000000000000000001 idle"
        );
    }

    #[test]
    fn audio_unavailable_line_names_the_stage() {
        assert_eq!(
            audio_unavailable_text(11, "endpoints"),
            "11 audio unavailable endpoints"
        );
    }

    #[test]
    fn held_and_released_lines_name_the_paths() {
        let paths: BTreeSet<String> = ["b".to_owned(), "a".to_owned()].into();
        assert_eq!(held_text(1, &paths, 61), "1 held a,b until 61");
        assert_eq!(released_text(62, &paths), "62 released a,b");
        assert_eq!(open_failed_text(63, &paths), "63 open failed a,b");
    }

    #[test]
    fn discovery_lines_tell_retry_from_giving_up() {
        let group = DeviceKey("logi-group:0ABA:8".into());
        assert_eq!(
            discovery_text(1, &group, Some(2_001)),
            "1 discovery logi-group:0ABA:8 failed, retry at 2001"
        );
        assert_eq!(
            discovery_text(3, &group, None),
            "3 discovery logi-group:0ABA:8 gave up"
        );
    }
}
