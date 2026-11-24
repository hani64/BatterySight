# BatterySight

BatterySight is a free, open-source (GPLv3) Windows app that shows the battery level of wireless devices on the taskbar. It never keeps a device awake: it listens for the battery reports devices send on their own and asks a device only on a few one-off events (see below). Every supported device entry carries a verification status and a source.

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
- you click "Refresh now",
- audio is playing to a headset that does not report its battery while in use (the PRO X Wireless): at most once every 5 minutes, only while sound is actually playing, and never while the headset is known to be off or asleep.

Otherwise it only listens. Opening the menu never queries; it shows the last known values. A device known to be asleep is not asked until it reports again. Audio is matched to a device through the container id Windows gives both a headset's audio endpoint and its HID interfaces; BatterySight only watches which audio sessions are active and reads the endpoint's peak meter, it never records or plays audio.

### Known limitations

- The PRO X Wireless headset does not report its battery while it is in use. Its level updates when the headset is switched on, when the PC resumes, on "Refresh now" and at most every 5 minutes while audio plays to it; during a long silent stretch (for example a muted call) it can lag behind.

## Privacy

BatterySight collects no telemetry. Its only network call is the update check (arriving in milestone 4).

Settings and battery history live in `%LOCALAPPDATA%\BatterySight` (`settings.toml`, `history.tsv`) and contain device ids, such as unit ids, as keys; they stay on your PC. `history.tsv` holds one line per sample (time, device key, level, charging state, awake/asleep/offline, millivolts when known, where the reading came from); a line with no level, `unknown`, `offline` and `os` marks where BatterySight started or the PC went to sleep, so the time after the device's previous line was not observed. Samples older than 30 days are removed when BatterySight starts.

For troubleshooting, setting the environment variable `BATTERYSIGHT_TRACE=1` makes it write every HID++ frame it sends and receives to `%LOCALAPPDATA%\BatterySight\sends.log`, along with when audio starts and stops on a device or cannot be watched (`audio` lines) and why it switched to tray icons (`ui fallback` lines). Tracing is off by default. The frames include device unit ids and other serial-like values, so review the file before sharing it.

## Usage

Start `batterysight.exe`. It shows each device's battery level on the taskbar, next to the notification area (and next to the clock on other monitors' taskbars). Click it for the menu:

- one submenu per device: show it on the taskbar or hide it, move it left or right, and its low-battery level (off, or 5 % to 40 %; 15 % unless you change it). A level at or below it is shown in red, and a Windows notification ("<name> is at N %") appears once when an awake, shown device that is not charging drops to it. It can appear again after the device charges or rises 10 points above that level, after you change the level, or after BatterySight restarts. In widget mode a temporary notification-area icon carries the notification and is removed a minute later.
- "Battery history…": a window with each device's level over the last 24 hours, 7 days or 30 days, with charging shaded and gaps while the device was asleep or off or while BatterySight or the PC was not running, and an estimate of the time left ("about N h", from the most recent stretch of at least 2 hours of use without charging, sleep or a restart; otherwise "not enough data"). A sample is recorded when a device's level, charging state or awake state changes, and at most every 30 minutes otherwise, only from readings BatterySight receives anyway: recording history never wakes or queries a device. Hidden devices are recorded too.
- "Show on": all monitors' taskbars, the main one only, or one monitor's.
- "Show as": the taskbar widget or one notification-area icon per device. Tray icons chosen here stay, across restarts, until you choose the widget again.
- "Start with Windows": adds or removes `BatterySight` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. It shows unchecked while Task Manager's "Startup apps" has BatterySight disabled; choosing it then enables it there again.
- "Refresh now" and "Exit".

Changes apply at once and are saved to `settings.toml`. Only one instance runs per session; starting another does nothing.

- `batterysight --tray-only` shows one notification-area icon per device instead. Use it if the widget does not appear on your Windows build. The app also switches to tray icons by itself when Windows refuses the widget window or the widget cannot find its place on the taskbar; it then tries the widget again 15 s, 1 min and 5 min later, and whenever Explorer restarts or the displays or settings change. If the widget stops responding (a stalled widget would freeze the taskbar's mouse input), it restarts itself with `--tray-only` (after about 1.5 s while Explorer is responsive, after about 5 s otherwise) and keeps tray icons until the next start. The menu's "Show as" says when tray icons show for one of these reasons; choosing "Taskbar widget" there tries the widget at once.
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
