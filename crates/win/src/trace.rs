//! Opt-in log of every report the runtime sends and reads.
//!
//! With `BATTERYSIGHT_TRACE=1`, each write the reactor started and each input report read
//! is appended to `%LOCALAPPDATA%\BatterySight\sends.log` as
//! `"<unix ms> <out|in> <collection id> <hex>"`, written on the device thread. Tracing only
//! records frames the hub already exchanges, so it adds no device traffic, and it never
//! fails the app: an unwritable file just means no trace.

use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use batterysight_core::hid::CollectionId;

/// The open trace file. Used only on the device thread.
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
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let mut text = line(unix_ms, direction, collection, bytes);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }
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
}
