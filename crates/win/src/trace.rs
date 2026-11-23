//! Opt-in log of every report the runtime sends.
//!
//! With `BATTERYSIGHT_TRACE=1`, each send is appended to
//! `%LOCALAPPDATA%\BatterySight\sends.log` as `"<unix ms> <collection id> <hex>"`. Tracing
//! never fails the app: an unwritable file just means no trace.

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

    /// Appends one send; write errors are ignored.
    pub(crate) fn send(&mut self, collection: CollectionId, bytes: &[u8]) {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let mut text = line(unix_ms, collection, bytes);
        text.push('\n');
        let _ = self.0.write_all(text.as_bytes());
    }
}

/// `"<unix ms> <collection id> <hex>"`, the hex in uppercase pairs without separators.
fn line(unix_ms: u64, collection: CollectionId, bytes: &[u8]) -> String {
    let mut text = format!("{unix_ms} {} ", collection.0);
    for byte in bytes {
        let _ = write!(text, "{byte:02X}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_time_collection_and_hex() {
        assert_eq!(
            line(
                1_763_900_000_123,
                CollectionId(42),
                &[0x10, 0xFF, 0x81, 0x00, 0x0A]
            ),
            "1763900000123 42 10FF81000A"
        );
    }

    #[test]
    fn line_of_empty_send() {
        assert_eq!(line(5, CollectionId(1), &[]), "5 1 ");
    }
}
