//! The app's files and the background thread that writes them.
//!
//! `settings.toml` and `history.tsv` live in `%LOCALAPPDATA%\BatterySight`. Every write
//! runs on one writer thread: callers only send ([`Store::save_settings`],
//! [`Store::append_history`], [`Store::run`]), so the UI thread never waits on the disk.
//! The one blocking read, [`Store::load_settings`], happens at startup before any window
//! exists. Write errors are ignored: the app keeps its in-memory settings, and the next
//! save writes the whole file again.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use batterysight_core::settings::Settings;

const SETTINGS: &str = "settings.toml";
/// A settings file that did not parse, kept for the user to look at.
const SETTINGS_BAD: &str = "settings.toml.bad";
/// A save in progress, renamed over [`SETTINGS`] once complete.
const SETTINGS_TMP: &str = "settings.toml.tmp";
const HISTORY: &str = "history.tsv";

/// How long [`Store::stop`] waits for queued writes.
const STOP_WAIT: Duration = Duration::from_secs(1);

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
        let writer_root = root.clone();
        let writer = thread::Builder::new()
            .name("batterysight-store".into())
            .spawn(move || {
                write_loop(writer_root.as_deref(), &queue);
                drop(done_tx);
            })
            .ok();
        Store {
            root,
            jobs,
            done,
            writer,
        }
    }

    /// The saved settings, or defaults when there are none. A file that does not parse is
    /// renamed to `settings.toml.bad` (replacing an older one) and defaults are used.
    /// Blocks on the disk: call it once at startup, before any window exists.
    pub fn load_settings(&self) -> Settings {
        let Some(root) = &self.root else {
            return Settings::default();
        };
        let path = root.join(SETTINGS);
        // Missing or unreadable (e.g. locked): defaults; the next save writes it again.
        let Ok(bytes) = fs::read(&path) else {
            return Settings::default();
        };
        let parsed = std::str::from_utf8(&bytes)
            .ok()
            // Notepad may add a byte order mark.
            .map(|text| Settings::from_toml(text.trim_start_matches('\u{FEFF}')));
        match parsed {
            Some(Ok(settings)) => settings,
            _ => {
                let _ = fs::rename(&path, root.join(SETTINGS_BAD));
                Settings::default()
            }
        }
    }

    /// Queues writing `settings` to `settings.toml` (temp file, then rename); when several
    /// saves queue up, only the latest is written.
    pub fn save_settings(&self, settings: Settings) {
        let _ = self.jobs.send(Job::Save(settings));
    }

    /// Queues appending `line` and a line break to `history.tsv`. `line` holds no line break.
    pub fn append_history(&self, line: String) {
        let _ = self.jobs.send(Job::Append(line));
    }

    /// Queues `job` (other blocking work, such as a registry write) on the writer thread.
    /// Jobs run in the order they were queued.
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

/// Runs jobs until every [`Store`] sender is gone. Jobs queued while one batch is handled
/// are coalesced: the latest settings are written once, the lines appended in one write.
fn write_loop(root: Option<&Path>, queue: &Receiver<Job>) {
    while let Ok(first) = queue.recv() {
        let mut settings = None;
        let mut lines = String::new();
        for job in iter::once(first).chain(queue.try_iter()) {
            match job {
                Job::Save(s) => settings = Some(s),
                Job::Append(line) => {
                    lines.push_str(&line);
                    lines.push('\n');
                }
                Job::Run(job) => job(),
            }
        }
        let Some(root) = root else {
            continue;
        };
        if let Some(settings) = settings {
            let _ = save(root, &settings);
        }
        if !lines.is_empty() {
            let _ = append(root, &lines);
        }
    }
}

/// Writes `settings.toml.tmp`, flushes it to disk and renames it over `settings.toml`, so
/// the file is always either the old or the new settings.
fn save(root: &Path, settings: &Settings) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let tmp = root.join(SETTINGS_TMP);
    let written = File::create(&tmp).and_then(|mut file| {
        file.write_all(settings.to_toml().as_bytes())?;
        file.sync_all()
    });
    let result = written.and_then(|()| fs::rename(&tmp, root.join(SETTINGS)));
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
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use batterysight_core::model::DeviceKey;
    use batterysight_core::settings::{DisplayMode, Threshold};

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
        let store = open(&dir);
        let mut settings = Settings {
            display: DisplayMode::Tray,
            ..Settings::default()
        };
        store.save_settings(settings.clone());
        settings.set_threshold(&DeviceKey("logi:0ABA:255".into()), Threshold::Off);
        store.save_settings(settings.clone());
        store.stop();

        let text = fs::read_to_string(dir.path().join("settings.toml")).unwrap();
        assert_eq!(Settings::from_toml(&text), Ok(settings.clone()));
        assert!(!dir.path().join("settings.toml.tmp").exists());

        // A later start reads it back.
        let store = open(&dir);
        assert_eq!(store.load_settings(), settings);
        store.stop();
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
