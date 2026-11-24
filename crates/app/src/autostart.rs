//! "Start with Windows": the `BatterySight` value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, holding the quoted exe path.
//!
//! [`is_enabled`] is read once at startup, before any window exists; [`set`] runs on the
//! store's writer thread, never on the UI thread.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{PCWSTR, w};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: PCWSTR = w!("BatterySight");

/// Whether the Run value starts `exe`.
pub fn is_enabled(exe: &Path) -> bool {
    read_value().is_some_and(|stored| starts(&stored, exe))
}

/// Writes the Run value for `exe`, or removes it (an absent value is not an error).
pub fn set(exe: &Path, on: bool) -> windows::core::Result<()> {
    // SAFETY: literal names; `data` outlives the call and its length is in bytes.
    unsafe {
        if on {
            let data: Vec<u16> = run_command(exe).encode_wide().chain(Some(0)).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                u32::try_from(data.len() * 2).unwrap_or(u32::MAX),
            )
            .ok()
        } else {
            match RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) {
                ERROR_FILE_NOT_FOUND => Ok(()),
                status => status.ok(),
            }
        }
    }
}

/// The Run value, if there is one (a string).
fn read_value() -> Option<OsString> {
    let mut size = 0u32;
    // SAFETY: literal names; `size` and then `buffer` outlive the calls, `size` in bytes.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Some(OsString::from_wide(&buffer[..len]))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Tools\Battery Sight\batterysight.exe";

    #[test]
    fn run_value_is_the_quoted_exe_path() {
        assert_eq!(run_command(Path::new(EXE)), format!("\"{EXE}\"").as_str());
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
}
