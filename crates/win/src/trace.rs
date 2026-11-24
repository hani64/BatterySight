//! Opt-in log of every report the runtime sends and reads, of audio activity, of groups
//! held back or given up and of UI events.
//!
//! With `BATTERYSIGHT_TRACE=1`, each write the reactor started and each input report read
//! is appended to `%LOCALAPPDATA%\BatterySight\sends.log` as
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
//! The UI adds `"<unix ms> ui <text>"` lines ([`ui_line`]) and the settings and history
//! store `"<unix ms> store <text>"` lines when a file cannot be read or written or is set
//! aside. Tracing only records what the runtime already does, so it adds no device
//! traffic, and it never fails the app: an unwritable file just means no trace.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use batterysight_core::hid::{CollectionId, ReportKind};
use batterysight_core::hub::WriteRefusal;
use batterysight_core::model::DeviceKey;

use crate::hid_enum::{ContainerId, container_hex};

/// The open trace file: the device thread's, held for the runtime's life, or one opened
/// for a single [`ui_line`].
pub(crate) struct Trace(File);

impl Trace {
    /// The trace file when `BATTERYSIGHT_TRACE=1` and the file can be opened, else `None`.
    pub(crate) fn from_env() -> Option<Trace> {
        if env::var_os("BATTERYSIGHT_TRACE").as_deref() != Some(OsStr::new("1")) {
            return None;
        }
        let dir = PathBuf::from(env::var_os("LOCALAPPDATA")?).join("BatterySight");
        fs::create_dir_all(&dir).ok()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("sends.log"))
            .ok()
            .map(Trace)
    }

    /// Appends one frame; write errors are ignored.
    pub(crate) fn record(&mut self, direction: Direction, collection: CollectionId, bytes: &[u8]) {
        let mut text = line(unix_ms(), direction, collection, bytes);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
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
        let _ = self.0.write_all(text.as_bytes());
    }

    /// Appends an audio activity change of `container`; write errors are ignored.
    pub(crate) fn audio(&mut self, container: &ContainerId, active: bool) {
        let mut text = audio_text(unix_ms(), container, active);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }

    /// Appends that audio activity is not (fully) watched because `stage` failed; write
    /// errors are ignored.
    pub(crate) fn audio_unavailable(&mut self, stage: &str) {
        let mut text = audio_unavailable_text(unix_ms(), stage);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }

    /// Appends that the reopen cap holds back the group with interface `paths` for
    /// `release_in` ms more; write errors are ignored.
    pub(crate) fn held(&mut self, paths: &BTreeSet<String>, release_in: u64) {
        let now = unix_ms();
        let mut text = held_text(now, paths, now.saturating_add(release_in));
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }

    /// Appends that the group with interface `paths`, held back before, was opened; write
    /// errors are ignored.
    pub(crate) fn released(&mut self, paths: &BTreeSet<String>) {
        let mut text = released_text(unix_ms(), paths);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }

    /// Appends that none of the HID++ collections of the group with interface `paths`
    /// opened; write errors are ignored.
    pub(crate) fn open_failed(&mut self, paths: &BTreeSet<String>) {
        let mut text = open_failed_text(unix_ms(), paths);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }

    /// Appends that `group`'s discovery registered nothing and is retried `retry_in` ms
    /// from now, or never (`None`); write errors are ignored.
    pub(crate) fn discovery_failed(&mut self, group: &DeviceKey, retry_in: Option<u64>) {
        let now = unix_ms();
        let retry_at = retry_in.map(|retry_in| now.saturating_add(retry_in));
        let mut text = discovery_text(now, group, retry_at);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }

    /// A trace appending to `file`.
    #[cfg(test)]
    pub(crate) fn to_file(file: File) -> Trace {
        Trace(file)
    }
}

/// Appends `"<unix ms> ui <text>"` to the trace file when `BATTERYSIGHT_TRACE=1`; does
/// nothing otherwise, and ignores errors. It opens, appends to and closes the file on the
/// calling thread, the UI thread included: tracing is a debug switch, so that small append
/// is accepted there.
pub fn ui_line(text: &str) {
    if let Some(mut trace) = Trace::from_env() {
        let mut text = ui_text(unix_ms(), text);
        text.push('\n');
        let _ = trace.0.write_all(text.as_bytes());
    }
}

/// Appends `"<unix ms> store <text>"` like [`ui_line`]; called from the store's writer
/// thread, and at startup before any window exists.
pub(crate) fn store_line(text: &str) {
    if let Some(mut trace) = Trace::from_env() {
        let mut text = store_text(unix_ms(), text);
        text.push('\n');
        let _ = trace.0.write_all(text.as_bytes());
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
}

/// `"<unix ms> <out|in> <collection id> <hex>"`, the hex in uppercase pairs without
/// separators.
fn line(unix_ms: u64, direction: Direction, collection: CollectionId, bytes: &[u8]) -> String {
    let direction = match direction {
        Direction::Out => "out",
        Direction::In => "in",
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
