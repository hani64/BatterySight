# BatterySight

BatterySight is a free, open-source (GPLv3) Windows app that shows the battery level of wireless devices on the taskbar. It replaces BatteryHub, which went offline on 2026-11-22. Unlike BatteryHub, it never keeps a device awake: it listens for the battery reports devices send on their own and asks a device only on a few one-off events (see below). Every supported device entry carries a verification status and a source.

## Status

Milestone 1: Logitech devices that speak HID++ only. The goal is to cover wireless mice, keyboards, headsets and controllers of other vendors too; none is supported yet.

Tested on the maintainer's hardware (Windows 11 build 26300 only); the levels shown matched G HUB's within 1 point (BatterySight rounds down):

- Logitech PRO X Wireless Gaming Headset (USB dongle 046D:0ABA)
- Logitech PRO X Superlight on its LIGHTSPEED receiver (046D:C547)

Other Logitech HID++ devices are expected to work from the protocol sources but are untested.

### When BatterySight asks a device

It sends a battery query only when:

- the app starts or a device is plugged in,
- a device reconnects (for example a mouse waking up and linking to its receiver again),
- the PC resumes from sleep,
- you click "Refresh now".

Otherwise it only listens. Opening the menu never queries; it shows the last known values. A device known to be asleep is not asked until it reports again.

### Known limitations

- The PRO X Wireless headset does not report its battery while it is in use. Its level updates when the headset is switched on, when the PC resumes and on "Refresh now", so it can lag behind during a long session until audio-gated updates land.

## Privacy

BatterySight collects no telemetry. Its only network call is the update check (arriving in milestone 4).

For troubleshooting, setting the environment variable `BATTERYSIGHT_TRACE=1` makes it write every HID++ frame it sends and receives to `%LOCALAPPDATA%\BatterySight\sends.log`, along with why it switched to tray icons (`ui fallback` lines). Tracing is off by default. The frames include device unit ids and other serial-like values, so review the file before sharing it.

## Usage

Start `batterysight.exe`. It shows each device's battery level on the taskbar, next to the notification area (and next to the clock on other monitors' taskbars). Click it for the device list, "Refresh now" and "Exit". Only one instance runs per session; starting another does nothing.

- `batterysight --tray-only` shows one notification-area icon per device instead. Use it if the widget does not appear on your Windows build. The app also switches to tray icons by itself when Windows refuses the widget window or the widget cannot find its place on the taskbar; it then tries the widget again 15 s, 1 min and 5 min later, and whenever Explorer restarts or the displays or settings change. If the widget stops responding (a stalled widget would freeze the taskbar's mouse input), it restarts itself with `--tray-only` (after about 1.5 s while Explorer is responsive, after about 5 s otherwise) and keeps tray icons until the next start.
- `batterysight --list` prints the devices it finds, one per line, and exits.

## Building

```
cargo build --release
```

The binary is written to `target/release/batterysight.exe`.

With the MSVC toolchain (Visual Studio Build Tools installed) nothing else is needed. With the `x86_64-pc-windows-gnu` toolchain, the `windows` crate needs GNU binutils (`dlltool` and `as`): install them, for example with `winget install --scope user BrechtSanders.WinLibs.POSIX.UCRT`, and point rustc at that `dlltool.exe` in your user `~/.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-gnu]
rustflags = ["-C", "dlltool=<path to WinLibs>/mingw64/bin/dlltool.exe"]
```

## License

GPL-3.0-only. See [LICENSE](LICENSE).
