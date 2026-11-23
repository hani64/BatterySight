#![windows_subsystem = "windows"]

mod list;

use windows::Win32::Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
};

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--list")
    {
        attach_parent_console();
        std::process::exit(list::run());
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
