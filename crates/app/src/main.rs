#![windows_subsystem = "windows"]

mod alerts;
mod autostart;
mod list;
mod ui;

use std::ffi::OsString;

use batterysight_core::settings::DisplayMode;
use batterysight_win::store::Store;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::w;

/// How long a `--takeover` instance waits for the one it replaces to end.
const TAKEOVER_WAIT_MS: u32 = 10_000;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    if args.first().is_some_and(|arg| arg == "--list") {
        attach_parent_console();
        std::process::exit(list::run());
    }
    // `--takeover` (hidden): started by the widget watchdog just before it ends this
    // instance, so wait for the instance to go instead of leaving.
    let takeover = has("--takeover");
    let Some(instance) = SingleInstance::acquire(takeover) else {
        return;
    };
    // Before any window exists: both block this thread briefly (disk, registry).
    let store = Store::start();
    let mut settings = store.load_settings();
    settings.autostart = std::env::current_exe().is_ok_and(|exe| autostart::is_enabled(&exe));
    let options = ui::Options {
        tray: ui::fallback::start_reason(
            has("--tray-only"),
            takeover,
            settings.display == DisplayMode::Tray,
        ),
        debug_stall: cfg!(debug_assertions) && has("--debug-stall-widget"),
        debug_no_anchor: cfg!(debug_assertions) && has("--debug-force-no-anchor"),
    };
    let code = ui::run(&options, store, settings);
    drop(instance);
    std::process::exit(code);
}

/// Ownership of the named mutex `Local\BatterySight` (one instance per session). Owned by
/// the main thread, which runs the UI for the life of the process; a process that ends
/// without releasing it leaves it abandoned, which hands it to the next waiter.
struct SingleInstance(HANDLE);

impl SingleInstance {
    /// `None` when another instance holds the mutex (after waiting for it with `takeover`).
    fn acquire(takeover: bool) -> Option<SingleInstance> {
        // SAFETY: literal name; the handle is closed by `Drop` or below.
        unsafe {
            let mutex = CreateMutexW(None, true, w!(r"Local\BatterySight")).ok()?;
            if GetLastError() != ERROR_ALREADY_EXISTS {
                return Some(SingleInstance(mutex));
            }
            if takeover {
                let waited = WaitForSingleObject(mutex, TAKEOVER_WAIT_MS);
                if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
                    return Some(SingleInstance(mutex));
                }
            }
            let _ = CloseHandle(mutex);
            None
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: the mutex is owned by this thread and the handle is ours.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// A GUI-subsystem process has no console: prints go to the console it was started from.
/// Output redirected to a file or pipe keeps its handle, which attaching would replace.
fn attach_parent_console() {
    // SAFETY: no pointers involved; `GetFileType` of an invalid handle is FILE_TYPE_UNKNOWN.
    let redirected = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
        .is_ok_and(|h| matches!(unsafe { GetFileType(h) }, FILE_TYPE_DISK | FILE_TYPE_PIPE));
    if !redirected {
        // SAFETY: no pointers involved. Failure (no parent console) just leaves no output.
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}
