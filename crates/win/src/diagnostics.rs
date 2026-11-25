//! Local contributor diagnostics: `--dump` and `--capture`.
//!
//! Both only read. `--dump` prints cached HID and Bluetooth metadata, never a device path,
//! serial number, container id or report. `--capture` opens the requested device's
//! vendor-defined collections shared and read-only and prints the input reports they
//! deliver; it never writes, never asks for a feature report and never opens a keyboard,
//! mouse, gamepad or other system collection. Reports are written exactly as read.

use std::fmt::{self, Write as _};
use std::io::{self, Write};

use batterysight_core::hid::CollectionId;
use batterysight_core::time::Ms;

use crate::hid_enum::{self, HidInterface};
use crate::hid_io;
use crate::reactor::{self, OsEvent, Reactor};
use crate::system::bluetooth::{self, Metadata};

/// First line of every capture; bump it when the line format changes.
pub const CAPTURE_FORMAT: &str = "batterysight-capture-v1";
/// First line of every dump.
const DUMP_FORMAT: &str = "batterysight-dump-v1";

/// Longest capture, in minutes.
pub const MAX_MINUTES: u8 = 60;

/// `VVVV:PPPP`: exactly four hex digits each, as `--dump` prints them.
pub fn parse_vid_pid(text: &str) -> Option<(u16, u16)> {
    let (vid, pid) = text.split_once(':')?;
    Some((hex4(vid)?, hex4(pid)?))
}

fn hex4(text: &str) -> Option<u16> {
    if text.len() != 4 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(text, 16).ok()
}

/// Whole minutes from 1 to [`MAX_MINUTES`], decimal digits only.
pub fn parse_minutes(text: &str) -> Option<u8> {
    if text.is_empty() || text.len() > 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let minutes = text.parse().ok()?;
    (1..=MAX_MINUTES).contains(&minutes).then_some(minutes)
}

/// `--dump` could not read the system's metadata.
#[derive(Debug)]
pub enum DumpError {
    Hid,
    Bluetooth(windows::core::Error),
    Output(io::Error),
}

impl fmt::Display for DumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DumpError::Hid => f.write_str(
                "cannot list HID devices: Windows Configuration Manager failed; try again",
            ),
            DumpError::Bluetooth(error) => {
                write!(f, "cannot read cached Bluetooth devices: {error}; try again")
            }
            DumpError::Output(error) => write!(f, "cannot write the dump: {error}"),
        }
    }
}

/// Prints every HID collection and Bluetooth container from cached metadata. Opens
/// collections only without access rights for their attributes; sends nothing.
pub fn dump(out: &mut dyn Write) -> Result<(), DumpError> {
    dump_from(
        out,
        hid_enum::enumerate(),
        bluetooth::scan_metadata(),
        hid_enum::cached_product_name,
    )
}

/// Writes the dump only once both sources were read, so a failure prints nothing.
fn dump_from(
    out: &mut dyn Write,
    hid: Option<Vec<HidInterface>>,
    bluetooth: windows::core::Result<Vec<Metadata>>,
    name: impl Fn(&HidInterface) -> Option<String>,
) -> Result<(), DumpError> {
    let hid = hid.ok_or(DumpError::Hid)?;
    let bluetooth = bluetooth.map_err(DumpError::Bluetooth)?;
    write_dump(out, &hid, name, &bluetooth).map_err(DumpError::Output)
}

/// The dump: a header, then one `hid` line per collection (sorted) and one `bluetooth` line
/// per container. Paths, containers and nodes are never printed; `name` must not send.
fn write_dump(
    out: &mut dyn Write,
    hid: &[HidInterface],
    name: impl Fn(&HidInterface) -> Option<String>,
    bluetooth: &[Metadata],
) -> io::Result<()> {
    writeln!(out, "{DUMP_FORMAT}")?;
    writeln!(
        out,
        "# Metadata only: no device paths, serial numbers or container ids, and nothing was \
         sent to any device."
    )?;
    let mut sorted: Vec<&HidInterface> = hid.iter().collect();
    sorted.sort_by_key(|iface| {
        let c = &iface.collection;
        (c.vid, c.pid, c.interface, c.usage_page, c.usage)
    });
    for iface in sorted {
        let c = &iface.collection;
        let mut line = format!("hid {:04X}:{:04X} mi=", c.vid, c.pid);
        match c.interface {
            Some(number) => {
                let _ = write!(line, "{number:02X}");
            }
            None => line.push('-'),
        }
        let _ = write!(
            line,
            " usage={:04X}:{:04X} input={} output={} feature={} name=",
            c.usage_page, c.usage, c.input_len, c.output_len, c.feature_len
        );
        match name(iface) {
            Some(name) => push_quoted(&mut line, &name),
            None => line.push('-'),
        }
        writeln!(out, "{line}")?;
    }
    for device in bluetooth {
        let mut line = format!(
            "bluetooth battery-property={} name=",
            if device.has_battery_property { "yes" } else { "no" }
        );
        push_quoted(&mut line, &device.name);
        writeln!(out, "{line}")?;
    }
    out.flush()
}

/// Appends `text` in double quotes, escaping quotes, backslashes, control characters and
/// bidirectional formatting characters as `\u{…}` so a name cannot rewrite the terminal
/// or reorder the line.
fn push_quoted(line: &mut String, text: &str) {
    line.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                line.push('\\');
                line.push(c);
            }
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => {
                let _ = write!(line, "\\u{{{:x}}}", u32::from(c));
            }
            c if c.is_control() => {
                let _ = write!(line, "\\u{{{:x}}}", u32::from(c));
            }
            c => line.push(c),
        }
    }
    line.push('"');
}

/// Why `--capture` produced no complete capture.
#[derive(Debug)]
pub enum CaptureError {
    Enumerate,
    Absent { vid: u16, pid: u16 },
    NoVendorCollection { vid: u16, pid: u16 },
    Port(windows::core::Error),
    Open {
        vid: u16,
        pid: u16,
        error: windows::core::Error,
    },
    /// Every opened collection stopped reading before the deadline.
    Lost { reports: usize },
    Output(io::Error),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Enumerate => f.write_str(
                "cannot list HID devices: Windows Configuration Manager failed; try again",
            ),
            CaptureError::Absent { vid, pid } => write!(
                f,
                "no HID device {vid:04X}:{pid:04X} is connected; connect or wake it, and check \
                 its VID:PID with `batterysight --dump`"
            ),
            CaptureError::NoVendorCollection { vid, pid } => write!(
                f,
                "{vid:04X}:{pid:04X} has no vendor-defined HID collection with input reports; \
                 capture never opens keyboard, mouse, gamepad or other system collections"
            ),
            CaptureError::Port(error) => write!(f, "cannot create an I/O completion port: {error}"),
            CaptureError::Open { vid, pid, error } => write!(
                f,
                "cannot open any vendor collection of {vid:04X}:{pid:04X} for reading: {error}; \
                 reconnect the device or close programs that may hold it exclusively, then try \
                 again"
            ),
            CaptureError::Lost { reports } => write!(
                f,
                "every collection stopped reading after {reports} {}; the device was probably \
                 unplugged or turned off",
                plural(*reports)
            ),
            CaptureError::Output(error) => write!(f, "cannot write the capture: {error}"),
        }
    }
}

impl From<io::Error> for CaptureError {
    fn from(error: io::Error) -> Self {
        CaptureError::Output(error)
    }
}

fn plural(reports: usize) -> &'static str {
    if reports == 1 { "report" } else { "reports" }
}

/// Captures `vid:pid`'s vendor-collection input reports for `minutes` to `out`, after the
/// header; a start line also goes to `console`. Returns the number of reports. Nothing is
/// printed to `out` when the device is absent or cannot be opened.
pub fn capture(
    vid: u16,
    pid: u16,
    minutes: u8,
    out: &mut dyn Write,
    console: &mut dyn Write,
) -> Result<usize, CaptureError> {
    let interfaces = hid_enum::enumerate().ok_or(CaptureError::Enumerate)?;
    let mut capture = Capture::open(vid, pid, interfaces)?;
    capture.start(minutes, out, console)?;
    capture.run(u64::from(minutes) * 60_000, out)
}

/// One opened collection, known in the output only by its ephemeral label `c{label}`.
struct Opened {
    id: CollectionId,
    label: usize,
    reading: bool,
}

/// The requested device's vendor collections, opened read-only.
struct Capture {
    reactor: Reactor,
    vid: u16,
    pid: u16,
    opened: Vec<Opened>,
    /// One header line per vendor collection, opened or not.
    collections: Vec<String>,
}

impl Capture {
    /// Opens, shared and read-only, every vendor collection with input reports among
    /// `interfaces` that is exactly `vid:pid`. Fails when none can be opened.
    fn open(vid: u16, pid: u16, interfaces: Vec<HidInterface>) -> Result<Capture, CaptureError> {
        let matching: Vec<HidInterface> = interfaces
            .into_iter()
            .filter(|iface| iface.collection.vid == vid && iface.collection.pid == pid)
            .collect();
        if matching.is_empty() {
            return Err(CaptureError::Absent { vid, pid });
        }
        let groups: Vec<_> = hid_enum::group(matching)
            .into_iter()
            .map(|(group, members)| {
                let vendor: Vec<HidInterface> = members
                    .into_iter()
                    .filter(|iface| {
                        hid_io::is_vendor(iface.collection.usage_page)
                            && iface.collection.input_len > 0
                    })
                    .collect();
                (group.id, vendor)
            })
            .filter(|(_, vendor)| !vendor.is_empty())
            .collect();
        if groups.is_empty() {
            return Err(CaptureError::NoVendorCollection { vid, pid });
        }
        let mut reactor = Reactor::new().map_err(CaptureError::Port)?;
        let mut opened = Vec::new();
        let mut collections = Vec::new();
        let mut first_error = None;
        let mut label = 0;
        for (device, (group, members)) in groups.iter().enumerate() {
            for iface in members {
                label += 1;
                let c = &iface.collection;
                let mut line = format!(
                    "c{label} device {} usage={:04X}:{:04X} input={}",
                    device + 1,
                    c.usage_page,
                    c.usage,
                    c.input_len
                );
                match reactor.open(*group, iface, true, false) {
                    Ok(true) => opened.push(Opened {
                        id: c.id,
                        label,
                        reading: true,
                    }),
                    Ok(false) => line.push_str(" not opened"),
                    Err(error) => {
                        let _ = write!(line, " not opened: {error}");
                        first_error.get_or_insert(error);
                    }
                }
                collections.push(line);
            }
        }
        if opened.is_empty() {
            return Err(CaptureError::Open {
                vid,
                pid,
                error: first_error.unwrap_or_else(|| windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_ACCESSDENIED,
                )),
            });
        }
        Ok(Capture {
            reactor,
            vid,
            pid,
            opened,
            collections,
        })
    }

    /// Writes the header to `out` and says what is captured on `console`.
    fn start(&self, minutes: u8, out: &mut dyn Write, console: &mut dyn Write) -> io::Result<()> {
        self.write_header(minutes, out)?;
        let _ = writeln!(
            console,
            "batterysight: capturing {:04X}:{:04X} for {minutes} min.",
            self.vid, self.pid
        );
        let _ = console.flush();
        Ok(())
    }

    fn write_header(&self, minutes: u8, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{CAPTURE_FORMAT}")?;
        writeln!(
            out,
            "# {:04X}:{:04X}, {minutes} min: input reports of its vendor collections, opened \
             read-only; nothing was sent to the device",
            self.vid, self.pid
        )?;
        writeln!(out, "# columns: ms since start, collection, report bytes in hex")?;
        for line in &self.collections {
            writeln!(out, "# {line}")?;
        }
        out.flush()
    }

    /// Prints reports for `duration` ms, then the trailer. Fails when every collection
    /// stopped reading first.
    fn run(&mut self, duration: Ms, out: &mut dyn Write) -> Result<usize, CaptureError> {
        let start = reactor::clock_ms();
        let deadline = start.saturating_add(duration);
        let mut reports = 0;
        loop {
            let now = reactor::clock_ms();
            if now >= deadline {
                break;
            }
            for event in self.reactor.wait(Some(deadline), now) {
                let (collection, bytes) = match event {
                    OsEvent::Report { collection, bytes } => (collection, Some(bytes)),
                    OsEvent::ReadFailed { collection } => (collection, None),
                    _ => continue,
                };
                let Some(opened) = self.opened.iter_mut().find(|o| o.id == collection) else {
                    continue;
                };
                let ms = reactor::clock_ms().saturating_sub(start);
                match bytes {
                    Some(bytes) => {
                        reports += 1;
                        writeln!(out, "{ms}\tc{}\t{}", opened.label, render(&bytes))?;
                    }
                    None if opened.reading => {
                        opened.reading = false;
                        writeln!(out, "# {ms}\tc{}\tread failed; no more reports", opened.label)?;
                    }
                    None => {}
                }
            }
            if self.opened.iter().all(|o| !o.reading) {
                writeln!(out, "# end: {reports} {}; every collection stopped", plural(reports))?;
                out.flush()?;
                return Err(CaptureError::Lost { reports });
            }
        }
        writeln!(out, "# end: {reports} {}", plural(reports))?;
        out.flush()?;
        Ok(reports)
    }
}

/// The report as uppercase hex bytes separated by spaces.
fn render(bytes: &[u8]) -> String {
    let mut line = String::with_capacity(bytes.len() * 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let _ = write!(line, "{byte:02X}");
    }
    line
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use batterysight_core::hid::{Collection, CollectionId};
    use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_DUPLEX, PIPE_ACCESS_OUTBOUND};

    use super::*;
    use crate::reactor::tests::Server;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// A synthetic interface with a path, container and node that must never be printed.
    fn interface(vid: u16, pid: u16, usage_page: u16, usage: u16) -> HidInterface {
        HidInterface {
            path: wide(r"\\?\hid#vid_1234&pid_5678&mi_02#7&1a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"),
            collection: Collection {
                id: CollectionId(1),
                vid,
                pid,
                usage_page,
                usage,
                interface: Some(2),
                input_len: 20,
                input_report_05: None,
                output_len: 20,
                feature_len: 0,
                product: String::new(),
            },
            container: Some([0xAB; 16]),
            node: Some(7),
        }
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn dump_lists_metadata_without_paths_containers_or_payloads() {
        let hid = [interface(0x1234, 0x5678, 0xFF00, 0x0002)];
        let bluetooth = [
            Metadata {
                name: "Synthetic Buds".into(),
                has_battery_property: true,
            },
            Metadata {
                name: "Speaker".into(),
                has_battery_property: false,
            },
        ];
        let mut out = Vec::new();
        write_dump(&mut out, &hid, |_| Some("Synthetic Receiver".into()), &bluetooth).unwrap();
        let out = text(out);
        assert!(out.starts_with("batterysight-dump-v1\n"), "{out}");
        assert!(!out.contains("review"), "{out}");
        assert!(out.contains(
            "hid 1234:5678 mi=02 usage=FF00:0002 input=20 output=20 feature=0 name=\"Synthetic Receiver\"\n"
        ));
        assert!(out.contains("bluetooth battery-property=yes name=\"Synthetic Buds\"\n"));
        assert!(out.contains("bluetooth battery-property=no name=\"Speaker\"\n"));
        let lower = out.to_ascii_lowercase();
        for leak in ["hid#", "1a2b3c4d", "4d1e55b2", "abababab", "{"] {
            assert!(!lower.contains(leak), "{leak} in {out}");
        }
        // Data lines carry no serial, path, container or payload fields.
        for line in lower.lines().filter(|line| !line.starts_with('#')) {
            for field in ["serial", "path", "container", "payload", "report"] {
                assert!(!line.contains(field), "{field} in {line}");
            }
        }
    }

    #[test]
    fn dump_prints_nothing_when_a_source_fails() {
        let mut out = Vec::new();
        let error = dump_from(&mut out, None, Ok(Vec::new()), |_| None).unwrap_err();
        assert!(matches!(error, DumpError::Hid), "{error}");
        assert!(out.is_empty());

        let hid = vec![interface(0x1234, 0x5678, 0xFF00, 0x0002)];
        let failed = Err(windows::Win32::Foundation::E_FAIL.into());
        let error = dump_from(&mut out, Some(hid), failed, |_| None).unwrap_err();
        assert!(matches!(error, DumpError::Bluetooth(_)), "{error}");
        assert!(out.is_empty());
    }

    #[test]
    fn dump_escapes_control_and_bidi_characters_in_names() {
        let hid = [interface(0x1234, 0x5678, 0x0001, 0x0002)];
        let bluetooth = [Metadata {
            name: "Pad\u{1b}[31m\n\u{202E}x\"\\".into(),
            has_battery_property: true,
        }];
        let mut out = Vec::new();
        write_dump(&mut out, &hid, |_| None, &bluetooth).unwrap();
        let out = text(out);
        assert!(out.contains("usage=0001:0002 input=20 output=20 feature=0 name=-\n"));
        assert!(out.contains(r#"name="Pad\u{1b}[31m\u{a}\u{202e}x\"\\""#), "{out}");
        assert!(!out.contains('\u{1b}') && !out.contains('\u{202E}'));
    }

    #[test]
    fn vid_pid_and_minutes_parse_strictly() {
        assert_eq!(parse_vid_pid("046D:C547"), Some((0x046D, 0xC547)));
        assert_eq!(parse_vid_pid("046d:c547"), Some((0x046D, 0xC547)));
        for bad in ["", "046D", "46D:C547", "046D:C5470", "0x6D:C547", "046D-C547", "046D:C54G", "+46D:C547"] {
            assert_eq!(parse_vid_pid(bad), None, "{bad}");
        }
        assert_eq!(parse_minutes("1"), Some(1));
        assert_eq!(parse_minutes("60"), Some(60));
        for bad in ["0", "61", "1.5", "+5", " 5", "-1", "", "100000000000000000000", "1m"] {
            assert_eq!(parse_minutes(bad), None, "{bad}");
        }
    }

    #[test]
    fn reports_are_written_as_read() {
        let mut reply = [0u8; 20];
        reply[..10].copy_from_slice(&[0x11, 0xFF, 0x83, 0xB5, 0x30, 0xDE, 0xAD, 0xBE, 0xEF, 0x04]);
        assert_eq!(
            render(&reply),
            "11 FF 83 B5 30 DE AD BE EF 04 00 00 00 00 00 00 00 00 00 00"
        );
        assert_eq!(render(&[0x10, 1, 0xDE]), "10 01 DE");
    }

    #[test]
    fn absent_device_and_missing_vendor_collections_are_errors() {
        let others = vec![
            interface(0x1234, 0x9999, 0xFF00, 0x0001),
            interface(0x4321, 0x5678, 0xFF00, 0x0001),
        ];
        let absent = Capture::open(0x1234, 0x5678, others).err().unwrap();
        assert!(matches!(absent, CaptureError::Absent { .. }));
        assert!(absent.to_string().contains("--dump"), "{absent}");

        let system = vec![
            interface(0x1234, 0x5678, 0x0001, 0x0002),
            interface(0x1234, 0x5678, 0x0001, 0x0006),
            interface(0x1234, 0x5678, 0x0001, 0x0005),
            interface(0x1234, 0x5678, 0x000C, 0x0001),
        ];
        let none = Capture::open(0x1234, 0x5678, system).err().unwrap();
        assert!(matches!(none, CaptureError::NoVendorCollection { .. }), "{none}");
    }

    #[test]
    fn unopenable_device_is_an_actionable_error() {
        let mut missing = interface(0x1234, 0x5678, 0xFF00, 0x0001);
        missing.path = wide(r"\\.\pipe\batterysight-test-capture-missing");
        let error = Capture::open(0x1234, 0x5678, vec![missing]).err().unwrap();
        assert!(matches!(error, CaptureError::Open { .. }), "{error}");
        assert!(!error.to_string().contains("pipe"), "{error}");
    }

    #[test]
    fn capture_reads_only_matching_vendor_collections_read_only_until_the_deadline() {
        // An outbound pipe refuses a client that asks for write access.
        let (server, mut vendor) = Server::new(PIPE_ACCESS_OUTBOUND);
        vendor.collection.vid = 0x1234;
        vendor.collection.pid = 0x5678;
        let mut unmatched = Vec::new();
        let mut interfaces = vec![vendor];
        for (vid, pid, page, usage) in [
            (0x1234, 0x5678, 0x0001, 0x0002), // mouse
            (0x1234, 0x5678, 0x0001, 0x0006), // keyboard
            (0x1234, 0x5678, 0x0001, 0x0005), // gamepad
            (0x1234, 0x5678, 0x000C, 0x0001), // consumer control
            (0x1234, 0x9999, 0xFF00, 0x0001), // other product
            (0x4321, 0x5678, 0xFF00, 0x0001), // other vendor
        ] {
            let (server, mut other) = Server::new(PIPE_ACCESS_DUPLEX);
            other.collection.vid = vid;
            other.collection.pid = pid;
            other.collection.usage_page = page;
            other.collection.usage = usage;
            interfaces.push(other);
            unmatched.push(server);
        }

        let mut capture = Capture::open(0x1234, 0x5678, interfaces).unwrap();
        for server in &unmatched {
            assert!(!server.has_client(), "an unmatched collection was opened");
        }
        let mut out = Vec::new();
        let mut console = Vec::new();
        capture.start(1, &mut out, &mut console).unwrap();
        let console = text(console);
        assert_eq!(console, "batterysight: capturing 1234:5678 for 1 min.\n");
        server.send(&[0x10, 0x01, 0x42]);
        let started = Instant::now();
        let reports = capture.run(300, &mut out).unwrap();
        let elapsed = started.elapsed().as_millis();
        assert!((300..2_000).contains(&elapsed), "{elapsed} ms");
        assert_eq!(reports, 1);

        let out = text(out);
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(CAPTURE_FORMAT));
        assert_eq!(
            lines.next(),
            Some(
                "# 1234:5678, 1 min: input reports of its vendor collections, opened read-only; \
                 nothing was sent to the device"
            )
        );
        assert_eq!(
            lines.next(),
            Some("# columns: ms since start, collection, report bytes in hex")
        );
        assert!(out.contains("# c1 device 1 usage=FF00:0001 input=7\n"), "{out}");
        let line = out
            .lines()
            .find(|line| !line.starts_with('#') && line.contains("\tc1\t"))
            .unwrap();
        let (ms, rest) = line.split_once('\t').unwrap();
        assert!(ms.parse::<u64>().unwrap() < 300, "{line}");
        assert_eq!(rest, "c1\t10 01 42");
        assert!(out.ends_with("# end: 1 report\n"), "{out}");
        for leak in ["pipe", "batterysight-test"] {
            assert!(!out.contains(leak), "{leak} in {out}");
        }
    }

    #[test]
    fn capture_that_loses_every_collection_fails() {
        let (server, mut vendor) = Server::new(PIPE_ACCESS_OUTBOUND);
        vendor.collection.vid = 0x1234;
        vendor.collection.pid = 0x5678;
        let mut capture = Capture::open(0x1234, 0x5678, vec![vendor]).unwrap();
        drop(server);
        let mut out = Vec::new();
        let error = capture.run(5_000, &mut out).err().unwrap();
        assert!(matches!(error, CaptureError::Lost { reports: 0 }), "{error}");
        assert!(text(out).contains("\tc1\tread failed"));
    }
}
