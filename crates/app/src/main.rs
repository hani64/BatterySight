#![windows_subsystem = "windows"]

mod alerts;
mod autostart;
mod diagnostics;
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
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    SetStdHandle,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::w;

/// How long a `--takeover` instance waits for the one it replaces to end.
const TAKEOVER_WAIT_MS: u32 = 10_000;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    if let Some(code) = run_cli(&args) {
        std::process::exit(code);
    }
    // `--takeover` (hidden): started by the widget watchdog just before it ends this
    // instance, so wait for the instance to go instead of leaving.
    let takeover = has("--takeover");
    let Some(instance) = SingleInstance::acquire(takeover) else {
        return;
    };
    // Before any window exists: these block this thread briefly (disk, registry).
    let store = Store::start();
    let mut settings = store.load_settings();
    settings.autostart = std::env::current_exe().is_ok_and(|exe| autostart::is_enabled(&exe));
    let samples = store.load_history(ui::unix_now_ms());
    let options = ui::Options {
        tray: ui::fallback::start_reason(
            has("--tray-only"),
            takeover,
            settings.display == DisplayMode::Tray,
        ),
        debug_stall: cfg!(debug_assertions) && has("--debug-stall-widget"),
        debug_no_anchor: cfg!(debug_assertions) && has("--debug-force-no-anchor"),
    };
    let code = ui::run(&options, store, settings, samples);
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

/// Runs `--list`, `--dump` or `--capture` and returns its exit code, or `None` to start the
/// app. Comes before the single-instance check and before the store, traces or audio start.
fn run_cli(args: &[OsString]) -> Option<i32> {
    if args.first().is_some_and(|arg| arg == "--list") {
        attach_parent_console();
        return Some(list::run());
    }
    let command = diagnostics::command(args)?;
    attach_parent_console();
    Some(diagnostics::run(command))
}

/// A GUI-subsystem process has no console: prints go to the console it was started from.
/// Output redirected to a file or pipe keeps its handle, which attaching would replace, so
/// a redirected stdout (a capture to a file) still leaves warnings on the console's stderr.
fn attach_parent_console() {
    let redirected = |kind| {
        // SAFETY: no pointers involved; `GetFileType` of an invalid handle is FILE_TYPE_UNKNOWN.
        unsafe { GetStdHandle(kind) }.ok().filter(|&h| {
            matches!(unsafe { GetFileType(h) }, FILE_TYPE_DISK | FILE_TYPE_PIPE)
        })
    };
    let (out, err) = (redirected(STD_OUTPUT_HANDLE), redirected(STD_ERROR_HANDLE));
    if out.is_some() && err.is_some() {
        return;
    }
    // SAFETY: no pointers involved. Failure (no parent console) just leaves no output.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok() {
        for (kind, handle) in [(STD_OUTPUT_HANDLE, out), (STD_ERROR_HANDLE, err)] {
            if let Some(handle) = handle {
                // SAFETY: restores a handle this process already had open.
                let _ = unsafe { SetStdHandle(kind, handle) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_run_while_another_instance_holds_the_mutex() {
        // Either this test or an already running BatterySight now holds the mutex.
        let held = SingleInstance::acquire(false);
        assert!(SingleInstance::acquire(false).is_none());
        for list in [&["--capture", "046D:C547", "0"][..], &["--dump", "extra"]] {
            let list: Vec<OsString> = list.iter().map(OsString::from).collect();
            assert_eq!(run_cli(&list), Some(2), "{list:?}");
        }
        drop(held);
    }
}
