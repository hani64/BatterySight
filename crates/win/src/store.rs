//! The app's files and the background thread that writes them.
//!
//! `settings.toml` and `history.tsv` live in `%LOCALAPPDATA%\BatterySight`. Every write
//! runs on one writer thread: callers only send ([`Store::save_settings`],
//! [`Store::append_history`], [`Store::run`]), so the UI thread never waits on the disk.
//! The one blocking read, [`Store::load_settings`], happens at startup before any window
//! exists. A failed write is traced (`BATTERYSIGHT_TRACE`) and otherwise ignored: the app
//! keeps its in-memory settings, and the next save writes the whole file again. An
//! existing settings file is never replaced unless it was read and parsed: otherwise the
//! first save sets it aside as `settings.toml.bad`.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use batterysight_core::settings::Settings;

use crate::trace;

const SETTINGS: &str = "settings.toml";
/// A settings file that could not be used, kept for the user to look at.
const SETTINGS_BAD: &str = "settings.toml.bad";
/// A save in progress, renamed over [`SETTINGS`] once complete.
const SETTINGS_TMP: &str = "settings.toml.tmp";
const HISTORY: &str = "history.tsv";

/// How long [`Store::stop`] waits for queued writes.
const STOP_WAIT: Duration = Duration::from_secs(1);
/// Further reads of a settings file that exists but cannot be read (a scanner or backup
/// tool may hold it briefly at logon), and the pause before each.
const READ_RETRIES: u32 = 4;
const READ_RETRY_GAP: Duration = Duration::from_millis(100);

enum Job {
    Save(Settings),
    Append(String),
    Run(Box<dyn FnOnce() + Send>),
}

pub struct Store {
    /// Where the files live; `None` (no `%LOCALAPPDATA%`) keeps everything in memory.
    root: Option<PathBuf>,
    jobs: Sender<Job>,
    /// Closed by the writer once it has drained its queue and is about to exit.
    done: Receiver<()>,
    /// `None` if the thread could not be started; sends are then dropped.
    writer: Option<JoinHandle<()>>,
    /// Set when `settings.toml` may hold settings that were not loaded (unreadable, or
    /// rejected but not set aside): the next save renames it to `settings.toml.bad` first.
    keep_existing: Arc<AtomicBool>,
}

impl Store {
    /// Starts the writer for `%LOCALAPPDATA%\BatterySight`.
    pub fn start() -> Store {
        let root = env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("BatterySight"));
        Store::start_in(root)
    }

    fn start_in(root: Option<PathBuf>) -> Store {
        let (jobs, queue) = mpsc::channel();
        let (done_tx, done) = mpsc::channel::<()>();
        let keep_existing = Arc::new(AtomicBool::new(false));
        let writer_root = root.clone();
        let writer_keep = Arc::clone(&keep_existing);
        let writer = thread::Builder::new()
            .name("batterysight-store".into())
            .spawn(move || {
                write_loop(writer_root.as_deref(), &writer_keep, &queue);
                drop(done_tx);
            })
            .ok();
        Store {
            root,
            jobs,
            done,
            writer,
            keep_existing,
        }
    }

    /// The saved settings, or defaults when there are none or they cannot be used. A file
    /// that does not parse is renamed to `settings.toml.bad` (replacing an older one); one
    /// that cannot be read or renamed is set aside by the first save instead, so no save
    /// ever replaces settings that were not loaded. Blocks on the disk (up to about
    /// 0.5 s while the file is held by another program): call it once at startup, before
    /// any window exists.
    pub fn load_settings(&self) -> Settings {
        let Some(root) = &self.root else {
            return Settings::default();
        };
        let path = root.join(SETTINGS);
        let bytes = match read_retrying(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Settings::default(),
            Err(e) => {
                trace::store_line(&format!("settings unreadable: {e}"));
                self.keep_existing.store(true, Ordering::Relaxed);
                return Settings::default();
            }
        };
        let reason = match std::str::from_utf8(&bytes) {
            // Notepad may add a byte order mark.
            Ok(text) => match Settings::from_toml(text.trim_start_matches('\u{FEFF}')) {
                Ok(settings) => return settings,
                Err(e) => e.to_string(),
            },
            Err(_) => "invalid settings: not UTF-8".to_owned(),
        };
        trace::store_line(&format!("settings rejected: {reason}"));
        if let Err(e) = fs::rename(&path, root.join(SETTINGS_BAD)) {
            trace::store_line(&format!("settings not set aside: {e}"));
            self.keep_existing.store(true, Ordering::Relaxed);
        }
        Settings::default()
    }

    /// Queues writing `settings` to `settings.toml` (temp file, then rename); when several
    /// saves queue up between two [`Store::run`] jobs, only the latest is written.
    pub fn save_settings(&self, settings: Settings) {
        let _ = self.jobs.send(Job::Save(settings));
    }

    /// Queues appending `line` and a line break to `history.tsv`. `line` holds no line break.
    pub fn append_history(&self, line: String) {
        let _ = self.jobs.send(Job::Append(line));
    }

    /// Queues `job` (other blocking work, such as a registry write) on the writer thread.
    /// Everything queued before it, saves and appends included, is done before it runs. A
    /// job that panics is traced and the writer goes on (in a build with `panic = "abort"`
    /// the panic still ends the process).
    pub fn run(&self, job: Box<dyn FnOnce() + Send>) {
        let _ = self.jobs.send(Job::Run(job));
    }

    /// Finishes the queued work, waiting at most 1 s; past that the writer is left to
    /// finish on its own.
    pub fn stop(self) {
        let Store {
            jobs, done, writer, ..
        } = self;
        drop(jobs);
        // The writer closes `done` only after its last write.
        if done.recv_timeout(STOP_WAIT) == Err(mpsc::RecvTimeoutError::Disconnected)
            && let Some(writer) = writer
        {
            let _ = writer.join();
        }
    }
}

/// `fs::read`, retried [`READ_RETRIES`] times while the file exists but cannot be read.
fn read_retrying(path: &Path) -> io::Result<Vec<u8>> {
    let mut retries = 0;
    loop {
        match fs::read(path) {
            Err(e) if e.kind() != ErrorKind::NotFound && retries < READ_RETRIES => {
                retries += 1;
                thread::sleep(READ_RETRY_GAP);
            }
            result => return result,
        }
    }
}

/// Writes not yet done: the latest settings and the lines to append.
#[derive(Default)]
struct Pending {
    settings: Option<Settings>,
    lines: String,
}

/// Runs jobs until every [`Store`] sender is gone. Saves and appends are coalesced up to
/// the next run job or the end of what is queued: the latest settings are written once,
/// the lines appended in one write.
fn write_loop(root: Option<&Path>, keep_existing: &AtomicBool, queue: &Receiver<Job>) {
    let mut pending = Pending::default();
    while let Ok(first) = queue.recv() {
        for job in std::iter::once(first).chain(queue.try_iter()) {
            match job {
                Job::Save(settings) => pending.settings = Some(settings),
                Job::Append(line) => {
                    pending.lines.push_str(&line);
                    pending.lines.push('\n');
                }
                Job::Run(job) => {
                    flush(root, keep_existing, &mut pending);
                    if panic::catch_unwind(AssertUnwindSafe(job)).is_err() {
                        trace::store_line("job panicked");
                    }
                }
            }
        }
        flush(root, keep_existing, &mut pending);
    }
}

fn flush(root: Option<&Path>, keep_existing: &AtomicBool, pending: &mut Pending) {
    let Pending { settings, lines } = std::mem::take(pending);
    let Some(root) = root else {
        return;
    };
    if let Some(settings) = settings
        && let Err(e) = save(root, &settings, keep_existing)
    {
        trace::store_line(&format!("save failed: {e}"));
    }
    if !lines.is_empty()
        && let Err(e) = append(root, &lines)
    {
        trace::store_line(&format!("append failed: {e}"));
    }
}

/// Writes `settings.toml.tmp`, flushes it to disk and renames it over `settings.toml`, so
/// the file is always either the old or the new settings. A file flagged by
/// `keep_existing` is first renamed to `settings.toml.bad`; if that fails nothing is
/// written, and the next save tries again.
fn save(root: &Path, settings: &Settings, keep_existing: &AtomicBool) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join(SETTINGS);
    if keep_existing.load(Ordering::Relaxed) {
        match fs::rename(&path, root.join(SETTINGS_BAD)) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => keep_existing.store(false, Ordering::Relaxed),
        }
    }
    let tmp = root.join(SETTINGS_TMP);
    let written = File::create(&tmp).and_then(|mut file| {
        file.write_all(settings.to_toml().as_bytes())?;
        file.sync_all()
    });
    let result = written.and_then(|()| fs::rename(&tmp, &path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn append(root: &Path, text: &str) -> io::Result<()> {
    fs::create_dir_all(root)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(HISTORY))?
        .write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use batterysight_core::model::DeviceKey;
    use batterysight_core::settings::{DisplayMode, Threshold};
    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

    use super::*;

    /// A fresh empty directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "batterysight-store-test-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn open(dir: &TempDir) -> Store {
        Store::start_in(Some(dir.path().to_path_buf()))
    }

    #[test]
    fn missing_settings_are_defaults() {
        let dir = TempDir::new();
        let store = open(&dir);
        assert_eq!(store.load_settings(), Settings::default());
        store.stop();
        assert!(!dir.path().join("settings.toml.bad").exists());
    }

    #[test]
    fn corrupt_settings_fall_back_to_defaults() {
        let dir = TempDir::new();
        let file = dir.path().join("settings.toml");
        for garbage in [
            &b"order = [\x00 garbage"[..],
            b"[thresholds]\na = 0\n",
            b"\xFF\xFE",
        ] {
            fs::write(&file, garbage).unwrap();
            let store = open(&dir);

            assert_eq!(store.load_settings(), Settings::default());
            store.stop();
            assert!(!file.exists());
            assert_eq!(
                fs::read(dir.path().join("settings.toml.bad")).unwrap(),
                garbage
            );
        }
    }

    #[test]
    fn save_is_atomic() {
        let dir = TempDir::new();
        let mut settings = Settings {
            display: DisplayMode::Tray,
            ..Settings::default()
        };
        let store = open(&dir);
        store.save_settings(settings.clone());
        store.stop();
        assert!(dir.path().join("settings.toml").exists());

        // The second save replaces an existing file.
        let store = open(&dir);
        settings.set_threshold(&DeviceKey("logi:0ABA:255".into()), Threshold::Off);
        store.save_settings(settings.clone());
        store.stop();

        let text = fs::read_to_string(dir.path().join("settings.toml")).unwrap();
        assert_eq!(Settings::from_toml(&text), Ok(settings.clone()));
        assert!(!dir.path().join("settings.toml.tmp").exists());
        assert!(!dir.path().join("settings.toml.bad").exists());

        // A later start reads it back.
        let store = open(&dir);
        assert_eq!(store.load_settings(), settings);
        store.stop();
    }

    /// `path` opened for reading with only `share` access allowed to others.
    fn hold(path: &Path, share: u32) -> fs::File {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(share)
            .open(path)
            .unwrap()
    }

    #[test]
    fn unreadable_settings_are_kept_as_bad_on_save() {
        let dir = TempDir::new();
        let file = dir.path().join("settings.toml");
        let original = b"autostart = true\n";
        fs::write(&file, original).unwrap();
        let store = open(&dir);

        // Another program holds the file without sharing it: it cannot be read.
        let held = hold(&file, 0);
        assert_eq!(store.load_settings(), Settings::default());
        drop(held);
        let saved = Settings {
            display: DisplayMode::Tray,
            ..Settings::default()
        };
        store.save_settings(saved.clone());
        store.stop();

        assert_eq!(
            fs::read(dir.path().join("settings.toml.bad")).unwrap(),
            original
        );
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(Settings::from_toml(&text), Ok(saved));
    }

    #[test]
    fn corrupt_settings_that_cannot_be_renamed_are_kept_as_bad_on_save() {
        let dir = TempDir::new();
        let file = dir.path().join("settings.toml");
        let garbage = b"order = [";
        fs::write(&file, garbage).unwrap();
        let store = open(&dir);

        // Readable, but not renamable while held without delete sharing.
        let held = hold(&file, FILE_SHARE_READ.0);
        assert_eq!(store.load_settings(), Settings::default());
        drop(held);
        assert_eq!(fs::read(&file).unwrap(), garbage);
        store.save_settings(Settings::default());
        store.stop();

        assert_eq!(
            fs::read(dir.path().join("settings.toml.bad")).unwrap(),
            garbage
        );
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(Settings::from_toml(&text), Ok(Settings::default()));
    }

    #[test]
    fn writes_queued_before_a_job_are_done_before_it_runs() {
        let dir = TempDir::new();
        let store = open(&dir);
        // Holds the writer so the next jobs queue up behind it.
        let (release, wait) = mpsc::channel::<()>();
        store.run(Box::new(move || {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }));
        store.save_settings(Settings::default());
        store.append_history("1".into());
        let (seen_tx, seen) = mpsc::channel();
        let root = dir.path().to_path_buf();
        store.run(Box::new(move || {
            let _ = seen_tx.send((
                root.join("settings.toml").exists(),
                fs::read_to_string(root.join("history.tsv")).unwrap_or_default(),
            ));
        }));
        drop(release);
        store.stop();

        assert_eq!(seen.try_recv(), Ok((true, "1\n".to_owned())));
    }

    #[test]
    fn a_panicking_job_does_not_stop_the_writer() {
        let dir = TempDir::new();
        let store = open(&dir);
        store.append_history("1".into());
        store.run(Box::new(|| panic!("job failed")));
        store.append_history("2".into());
        store.stop();

        assert_eq!(
            fs::read_to_string(dir.path().join("history.tsv")).unwrap(),
            "1\n2\n"
        );
    }

    #[test]
    fn append_history_appends_lines() {
        let dir = TempDir::new();
        let store = open(&dir);
        store.append_history("1\tlogi:0ABA:255\t80".into());
        store.append_history("2\tlogi:0ABA:255\t79".into());
        store.stop();
        let store = open(&dir);
        store.append_history("3\tlogi:0ABA:255\t78".into());
        store.stop();

        assert_eq!(
            fs::read_to_string(dir.path().join("history.tsv")).unwrap(),
            "1\tlogi:0ABA:255\t80\n2\tlogi:0ABA:255\t79\n3\tlogi:0ABA:255\t78\n"
        );
    }

    #[test]
    fn creates_the_root_on_first_write() {
        let dir = TempDir::new();
        let root = dir.path().join("BatterySight");
        let store = Store::start_in(Some(root.clone()));
        store.save_settings(Settings::default());
        store.stop();
        assert!(root.join("settings.toml").exists());
    }

    #[test]
    fn jobs_run_on_the_writer_thread_in_order() {
        let dir = TempDir::new();
        let store = open(&dir);
        let caller = std::thread::current().id();
        let (tx, rx) = mpsc::channel();
        for i in 0..3 {
            let tx = tx.clone();
            store.run(Box::new(move || {
                tx.send((i, std::thread::current().id())).unwrap();
            }));
        }
        store.stop();

        let ran: Vec<_> = rx.try_iter().collect();
        assert_eq!(ran.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(ran.iter().all(|(_, thread)| *thread != caller));
    }

    #[test]
    fn stop_returns_within_a_second_when_a_job_hangs() {
        let dir = TempDir::new();
        let store = open(&dir);
        let (release, wait) = mpsc::channel::<()>();
        store.run(Box::new(move || {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }));
        let start = Instant::now();
        store.stop();
        assert!(
            start.elapsed() < Duration::from_millis(1_500),
            "{:?}",
            start.elapsed()
        );
        drop(release);
    }

    #[test]
    fn without_a_root_nothing_is_written() {
        let store = Store::start_in(None);
        assert_eq!(store.load_settings(), Settings::default());
        store.save_settings(Settings::default());
        store.append_history("1".into());
        store.stop();
    }
}
