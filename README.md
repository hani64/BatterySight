# BatterySight

BatterySight is a free, open-source (GPLv3) Windows app that shows the battery level of wireless devices on the taskbar. It never keeps a device awake: it listens for the battery reports devices send on their own and asks a device only on a few one-off events (see below). Every supported device entry carries a verification status and a source.

## Status

Milestone 2: Logitech devices that speak HID++ only. The goal is to cover wireless mice, keyboards, headsets and controllers of other vendors too; none is supported yet.

Tested on the maintainer's hardware (Windows 11 build 26300 only); the levels shown matched G HUB's within 1 point (BatterySight rounds down):

- Logitech PRO X Wireless Gaming Headset (USB dongle 046D:0ABA)
- Logitech PRO X Superlight on its LIGHTSPEED receiver (046D:C547)

Other Logitech HID++ devices are expected to work from the protocol sources but are untested.

### When BatterySight asks a device

It sends a battery query only when:

- the app starts or a device is plugged in; if a plugged-in device stops answering before it is recognised (a just plugged-in dongle can be busy), it is asked again after 2 s, 10 s and 60 s, then not until the next resume or "Refresh now",
- a device reconnects (for example a mouse waking up and linking to its receiver again),
- the PC resumes from sleep,
- you click "Refresh now" (this also asks again a plugged-in device that stopped answering before it was recognised, and a receiver none of whose devices was recognised),
- audio is playing to a headset that does not report its battery while in use (the PRO X Wireless): every 10 minutes by default (adjustable under "Check battery while in use", never more often than every 5 minutes; the first check after the headset wakes or is first found comes after 5), only while sound is actually playing, and never while the headset is known to be off or asleep. While such a headset is switched on and charging, it is also checked at that interval without any sound (these checks may delay its automatic power-off on the charger).

Otherwise it only listens. Opening the menu never queries; it shows the last known values. A device known to be asleep is not asked until it reports again. Audio is matched to a device through the container id Windows gives both a headset's audio endpoint and its HID interfaces; BatterySight only watches which audio sessions are active and reads the endpoint's peak meter, it never records or plays audio.

### Known limitations

- The PRO X Wireless headset does not report its battery while it is in use. Its level updates when the headset is switched on, when the PC resumes, on "Refresh now" and every 10 minutes by default (adjustable, never below 5; the first check after a wake or after it is first found comes after 5) while audio plays to it; during a long silent stretch (for example a muted call) it can lag behind.
- When "Show on" is set to one secondary monitor and that monitor is turned off and on again, the widget stays on the main taskbar until BatterySight restarts or that monitor is chosen again.

## Privacy

BatterySight collects no telemetry and makes no network calls; an update check is not yet available.

Settings and battery history live in `%LOCALAPPDATA%\BatterySight` (`settings.toml`, `history.tsv`) and contain device ids, such as unit ids, as keys; they stay on your PC. `history.tsv` holds one line per sample (time, device key, level, charging state, awake/asleep/offline, millivolts when known, where the reading came from); a line with no level, `unknown`, `offline` and `os` marks time around it that was not observed: before it when BatterySight starts or the PC wakes up, after it when the PC goes to sleep. Samples older than 30 days and lines it cannot read are removed when BatterySight starts; if that would remove complete lines it cannot read, or samples dated more than a day ahead, the file is first copied to `history.tsv.bad`.

For troubleshooting, setting the environment variable `BATTERYSIGHT_TRACE=1` makes it append to `%LOCALAPPDATA%\BatterySight\sends.log`: every HID++ frame it sends and receives, when audio starts and stops on a device or cannot be watched (`audio` lines), when a device plugged in and out too often is held back for a while and when it is opened again (`held` and `released` lines, with the device's interface paths), when a device could not be opened (`open failed` lines), when a device stopped answering before it was recognised and will be asked again or no longer (`discovery` lines), why it switched to tray icons (`ui fallback` lines), why "Start with Windows" could not be changed (`ui autostart not changed` lines), why it draws text with GDI instead of DirectWrite for the rest of the session (`ui text: DirectWrite off` lines), and when the settings or history file cannot be read or written or is set aside (`store` lines). Tracing is off by default. These lines may contain device ids (unit ids, audio container ids, interface paths and other serial-like values), so review the file before sharing it.

## Usage

Start `batterysight.exe`. It shows each device's battery level on the taskbar, next to the notification area (and next to the clock on other monitors' taskbars). A bolt follows the level while the device charges and a check once it is charged full on power; an asleep device is dimmed, and one that disconnects shows a dimmed × for about a minute before it is removed. When two shown devices are of the same kind (two mice, say), each gets a short caption from its name, such as "SUP" for a PRO X Superlight. Rest the mouse on a device for as long as a tooltip takes (Windows' tooltip delay, the double-click time) to see a small card just above the taskbar: its name and level, then "Charging" or "Full", or how long ago it last reported (after "Asleep" or "Disconnected" when it is), and the time left when there is an estimate, as in "PRO X Superlight · 77%" over "3 min ago · about 20 h left". Tray icons show the same two lines as their tooltip. Click it for the menu:

- one submenu per device: show it on the taskbar or hide it, move it left or right, and its low-battery level (off, or 5 % to 40 %; 15 % unless you change it). A level at or below it is shown in red, and a Windows notification ("<name> is at N %") appears once when an awake, shown device that is not charging drops to it. It can appear again after the device charges or rises 10 points above that level, after you change the level, or after BatterySight restarts. A device that only reports a rough level turns red at Low or Critical and notifies as "<name> is low". In widget mode a temporary notification-area icon carries the notification and is removed a minute later.
- "Check battery while in use", in the submenu of a device BatterySight has to ask for its battery (such as the PRO X Wireless): every 5, 10, 15, 30 or 60 minutes; 10 minutes (or the device's minimum, if longer) unless you change it. Intervals below the device's minimum (5 minutes for the PRO X Wireless) are not offered, and a shorter one written in `settings.toml` counts as the minimum. The first check after the device wakes or is first found (when BatterySight starts or the device is plugged in) comes at the minimum. These checks happen only while the device is in use (for the PRO X Wireless, while audio plays to it) or charging, never while it is idle on battery or asleep.
- "Battery history…": a window with each device's level over the last 24 hours, 7 days or 30 days, with charging shaded and gaps while the device was asleep or off or while BatterySight or the PC was not running, and an estimate of the time left ("about N h", from the most recent stretch of at least 2 hours of use without charging, sleep or a restart; otherwise "not enough data"). A sample is recorded when a device's level, charging state or awake state changes, and at most every 30 minutes otherwise, only from readings BatterySight receives anyway: recording history never wakes or queries a device. Hidden devices are recorded too. For a device whose level comes from its battery voltage (such as the PRO X Wireless), the reading it sends as it wakes reads several points high on a rested battery; when a reading in use follows within 10 minutes, the chart and the time-left estimate leave it out (it stays in `history.tsv`).
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
