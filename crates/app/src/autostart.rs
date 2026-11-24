//! "Start with Windows": the `BatterySight` value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, holding the quoted exe path, and
//! the switch Task Manager and Settings ("Startup apps") keep for it under
//! `…\Explorer\StartupApproved\Run`.
//!
//! [`is_enabled`] is read once at startup, before any window exists; [`set`] runs on the
//! store's writer thread, never on the UI thread.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_BINARY, REG_ROUTINE_FLAGS, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_BINARY,
    RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{PCWSTR, w};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
/// Where Task Manager's and Settings' "Startup apps" switches keep their state.
const APPROVED_KEY: PCWSTR =
    w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
/// The value's name under both keys.
const VALUE: PCWSTR = w!("BatterySight");
/// An "enabled" `StartupApproved` value: 02, three zero bytes, then a zero FILETIME.
const ENABLED: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Whether Windows starts `exe` at sign-in: the Run value starts it and "Startup apps" has
/// not switched it off.
pub fn is_enabled(exe: &Path) -> bool {
    let starts_exe = read(RUN_KEY, RRF_RT_REG_SZ).is_some_and(|data| starts(&string(&data), exe));
    starts_exe && approved(read(APPROVED_KEY, RRF_RT_REG_BINARY).as_deref())
}

/// Turns starting `exe` at sign-in on (the Run value, and the "Startup apps" switch back on
/// if it was off) or off (both values removed; absent values are not an error).
pub fn set(exe: &Path, on: bool) -> windows::core::Result<()> {
    if on {
        let data: Vec<u8> = run_command(exe)
            .encode_wide()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        write(RUN_KEY, REG_SZ, &data)?;
        if !approved(read(APPROVED_KEY, RRF_RT_REG_BINARY).as_deref()) {
            write(APPROVED_KEY, REG_BINARY, &ENABLED)?;
        }
        Ok(())
    } else {
        delete(RUN_KEY)?;
        delete(APPROVED_KEY)
    }
}

/// `BatterySight` under `HKCU\<key>`, if it exists with a type `flags` accepts.
fn read(key: PCWSTR, flags: REG_ROUTINE_FLAGS) -> Option<Vec<u8>> {
    let mut size = 0u32;
    // SAFETY: literal names; `size` and then `buffer` outlive the calls, `size` in bytes.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            VALUE,
            flags,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        let mut buffer = vec![0u8; size as usize];
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            VALUE,
            flags,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        buffer.truncate(size as usize);
        Some(buffer)
    }
}

/// Writes `BatterySight` under `HKCU\<key>`.
fn write(key: PCWSTR, kind: REG_VALUE_TYPE, data: &[u8]) -> windows::core::Result<()> {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    // SAFETY: literal names; `data` outlives the call and `len` is its length in bytes.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key,
            VALUE,
            kind.0,
            Some(data.as_ptr().cast()),
            len,
        )
    }
    .ok()
}

/// Removes `BatterySight` under `HKCU\<key>`; one that is not there is not an error.
fn delete(key: PCWSTR) -> windows::core::Result<()> {
    // SAFETY: literal names.
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key, VALUE) } {
        ERROR_FILE_NOT_FOUND => Ok(()),
        status => status.ok(),
    }
}

/// A `REG_SZ` value's bytes as a string, up to its terminating NUL.
fn string(data: &[u8]) -> OsString {
    let wide: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&c| c != 0)
        .collect();
    OsString::from_wide(&wide)
}

/// The Run value for `exe`: its path in quotes.
fn run_command(exe: &Path) -> OsString {
    let mut command = OsString::from("\"");
    command.push(exe);
    command.push("\"");
    command
}

/// Whether the Run value `stored` starts `exe` (quoted or not; paths compare ignoring case).
fn starts(stored: &OsStr, exe: &Path) -> bool {
    let stored = stored.to_string_lossy();
    let stored = stored.trim();
    let path = stored
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(stored);
    path.to_lowercase() == exe.to_string_lossy().to_lowercase()
}

/// Whether a `StartupApproved\Run` value lets the Run entry start. The format is not
/// documented. No value means on; Task Manager writes 02 (on) or 03 (off), three zero
/// bytes and a FILETIME; Electron writes 03 and eleven zero bytes to turn an entry off and
/// reads 00… and 02… as on
/// (<https://github.com/electron/electron/blob/main/shell/browser/browser_win.cc>,
/// `SetLoginItemSettings` / `GetLoginItemSettings`). Parsers of the format read an odd
/// first byte as off, which this follows.
fn approved(data: Option<&[u8]>) -> bool {
    data.and_then(|d| d.first()).is_none_or(|b| b % 2 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Tools\Battery Sight\batterysight.exe";

    #[test]
    fn run_value_is_the_quoted_exe_path() {
        assert_eq!(run_command(Path::new(EXE)), format!("\"{EXE}\"").as_str());
    }

    #[test]
    fn reg_sz_bytes_read_up_to_the_nul() {
        let mut data: Vec<u8> = "\"C:\\ü.exe\""
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        data.extend([0, 0, 0x41, 0]);
        assert_eq!(string(&data), "\"C:\\ü.exe\"");
        assert_eq!(string(&[]), "");
    }

    #[test]
    fn stored_value_matches_the_running_exe() {
        let exe = Path::new(EXE);
        assert!(starts(run_command(exe).as_os_str(), exe));
        assert!(starts(OsStr::new(EXE), exe));
        assert!(starts(
            OsStr::new(r#"  "c:\tools\BATTERY SIGHT\batterysight.exe"  "#),
            exe
        ));
        assert!(!starts(OsStr::new(r#""C:\Old\batterysight.exe""#), exe));
        assert!(!starts(OsStr::new(""), exe));
        assert!(!starts(
            OsStr::new(r#""C:\Tools\Battery Sight\batterysight.exe" --x"#),
            exe
        ));
    }

    #[test]
    fn startup_approved_odd_first_byte_disables() {
        assert!(approved(None));
        assert!(approved(Some(&[])));
        // As Task Manager and Settings write them: 02 on, 03 off (then a FILETIME).
        assert!(approved(Some(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])));
        assert!(!approved(Some(&[3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8])));
        assert!(!approved(Some(&[7, 0, 0, 0])));
        assert!(approved(Some(&[6, 0, 0, 0])));
        assert!(approved(Some(&[0; 12])));
        // The marker the menu writes to turn it back on.
        assert!(approved(Some(&ENABLED)));
    }
}
