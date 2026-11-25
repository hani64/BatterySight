//! `batterysight --dump` and `--capture`: local contributor diagnostics. They run before
//! the single-instance check, so they work while BatterySight is running, and they start
//! no store, trace or audio watching.

use std::ffi::OsString;
use std::io::{self, Write as _};

use batterysight_win::diagnostics::{self, MAX_MINUTES};

const USAGE: &str = "usage: batterysight --dump\n       batterysight --capture <VID:PID> <minutes>\n\
VID:PID is four hex digits each, as --dump prints them; minutes is a whole number from 1 to 60.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Dump,
    Capture { vid: u16, pid: u16, minutes: u8 },
}

/// The diagnostics command the arguments ask for, or the usage error; `None` when the
/// first argument is neither `--dump` nor `--capture`.
pub fn command(args: &[OsString]) -> Option<Result<Command, String>> {
    let usage = |problem: &str| Err(format!("batterysight: {problem}\n{USAGE}"));
    let first = args.first()?;
    Some(if first == "--dump" {
        if args.len() == 1 {
            Ok(Command::Dump)
        } else {
            usage("--dump takes no arguments")
        }
    } else if first == "--capture" {
        match &args[1..] {
            [device, minutes] => {
                match (
                    device.to_str().and_then(diagnostics::parse_vid_pid),
                    minutes.to_str().and_then(diagnostics::parse_minutes),
                ) {
                    (None, _) => usage("invalid VID:PID"),
                    (_, None) => usage(&format!(
                        "invalid duration: whole minutes from 1 to {MAX_MINUTES}"
                    )),
                    (Some((vid, pid)), Some(minutes)) => Ok(Command::Capture { vid, pid, minutes }),
                }
            }
            _ => usage("--capture takes a VID:PID and a number of minutes"),
        }
    } else {
        return None;
    })
}

/// Runs the command and returns the process exit code: 0 on success, 1 when it failed,
/// 2 for a usage error.
pub fn run(command: Result<Command, String>) -> i32 {
    let mut console = io::stderr().lock();
    let result = match command {
        Err(usage) => {
            let _ = writeln!(console, "{usage}");
            return 2;
        }
        Ok(Command::Dump) => diagnostics::dump(&mut io::stdout().lock()).map_err(|e| e.to_string()),
        Ok(Command::Capture { vid, pid, minutes }) => {
            match diagnostics::capture(vid, pid, minutes, &mut io::stdout().lock(), &mut console) {
                Ok(reports) => {
                    let _ = writeln!(console, "batterysight: capture finished: {reports} reports");
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            }
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(console, "batterysight: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn other_arguments_are_not_diagnostics() {
        for list in [&[][..], &["--list"], &["--tray-only"], &["--takeover", "--dump"]] {
            assert!(command(&args(list)).is_none(), "{list:?}");
        }
    }

    #[test]
    fn dump_and_capture_parse() {
        assert_eq!(command(&args(&["--dump"])), Some(Ok(Command::Dump)));
        assert_eq!(
            command(&args(&["--capture", "046D:c547", "60"])),
            Some(Ok(Command::Capture {
                vid: 0x046D,
                pid: 0xC547,
                minutes: 60
            }))
        );
    }

    #[test]
    fn invalid_arguments_fail_with_usage() {
        for list in [
            &["--dump", "extra"][..],
            &["--capture"],
            &["--capture", "046D:C547"],
            &["--capture", "046D:C547", "0"],
            &["--capture", "046D:C547", "61"],
            &["--capture", "046D:C547", "1.5"],
            &["--capture", "046D:C547", "5", "extra"],
            &["--capture", "C547", "5"],
            &["--capture", "046D:XYZW", "5"],
        ] {
            let parsed = command(&args(list)).unwrap();
            let Err(message) = &parsed else {
                panic!("{list:?} parsed");
            };
            assert!(message.contains("usage"), "{message}");
            assert_eq!(run(parsed), 2, "{list:?}");
        }
    }
}
