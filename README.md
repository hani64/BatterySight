# BatterySight

BatterySight is a free, open-source (GPLv3) Windows app that shows the battery level of wireless devices on the taskbar. It never keeps a device awake: it listens for the battery reports devices send on their own and asks a device only on a few one-off events (see below). Every supported device entry carries a verification status and a source.

## Status

Milestone 3: Logitech HID++ devices, a first set of SteelSeries, HyperX, Pulsar and Rapoo devices, the battery level Windows keeps for Bluetooth devices, and Xbox controllers on a USB cable or the Xbox Wireless Adapter.

Every supported entry carries a verification status, which `batterysight --list` prints next to each device:

- `hardware-tested`: tested on the maintainer's hardware (Windows 11 build 26300 only).
- `source-verified`: built from public protocol sources (other open-source tools, their users' captures and vendor files), with synthetic test data, but not tested on hardware by the maintainer. It may not work, or may need a correction; a `--dump` or `--capture` report helps (see [Device reports for contributors](#device-reports-for-contributors)).

Hardware-tested; the levels shown matched G HUB's within 1 point (BatterySight rounds down):

- Logitech PRO X Wireless Gaming Headset (USB dongle 046D:0ABA)
- Logitech PRO X Superlight on its LIGHTSPEED receiver (046D:C547)

Source-verified (the VID:PID is that of the USB device BatterySight opens: the dongle, base station or receiver, or the device itself on a cable):

| Devices | Connection, VID:PID | How the level is kept up to date |
|---|---|---|
| Other Logitech HID++ devices | Logitech receivers and USB, 046D | Device reports, as for the tested ones |
| SteelSeries Arctis Nova 7 family, including Nova 7 Gen 2 | USB dongle, 1038: 2202, 2206, 220A, 223A, 227A, 22A4, 227E, 2258, 229E, 22AD, 22A1, 22A5, 22A7, 22A9, 2298, 22AB | Headset reports; asked 1.5 s after the dongle is found, on resume, "Refresh now" and once when the headset links up again |
| SteelSeries Arctis Nova 5 and 5X | USB dongle, 1038: 2232, 2253, 2264 | As the Nova 7 |
| SteelSeries Arctis Nova 3P and 3X Wireless | USB dongle, 1038: 2269, 226D | As the Nova 7 |
| SteelSeries Arctis 7+ family | USB dongle, 1038: 220E, 2212, 2216, 2236 | As the Nova 7 |
| SteelSeries Arctis GameBuds | USB dongle, 1038: 230A | As the Nova 7; shows the lower level of the buds in use, no charging state |
| SteelSeries Arctis Nova Pro Wireless | Base station, 1038: 12E0, 12E5, 225D | Headset reports; asked 5 s after the station is found (it needs that long to settle), on resume, "Refresh now" and once when the headset links up again. The spare battery is not shown |
| SteelSeries Arctis Nova Pro Omni | Base station, 1038: 2290 | As the Nova Pro Wireless |
| SteelSeries Arctis Nova Elite | Base station, 1038: 2244, 2246, 2270 | As the Nova Pro Wireless |
| HyperX Cloud III Wireless | USB dongle, 03F0: 05B7, 0C9D, 0FCC | Headset reports; asked only when the dongle is found, on resume and "Refresh now" (never during use: frequent questions reset its automatic power-off) |
| HyperX Cloud II Core Wireless | USB dongle, 03F0: 0995 | As the Cloud III |
| HyperX Cloud Alpha Wireless | USB dongle, 03F0: 098D | Asked when the dongle is found, on resume and "Refresh now", and while sound is audible on it at the "Check battery while in use" interval |
| HyperX Cloud Alpha 2 Wireless | USB dongle, 03F0: 08BE | Asked at most once each time the headset links up, and only if it has not reported its battery within 2 s; never otherwise |
| HyperX Pulsefire Haste 2 Wireless | Receiver, 03F0: 0F98 | As the Cloud Alpha 2, for the mouse |
| Pulsar Fusion2 1K and 8K dongles | Receiver, 3710: 5402, 5403 | The mouse reports when it links up; asked when the receiver is found, on resume, "Refresh now", once when the mouse links up without reporting within 2 s, and while the receiver reports the mouse linked at the "Check battery while in use" interval (default 10 minutes, at most every minute) |
| Pulsar Feinmann 8K dongle | Receiver, 3710: 5404 | Asked only when the receiver is found, on resume and "Refresh now" |
| Rapoo VT7, VT2 Pro Max, VT9 Pro V2, VT1 SE, VT7 SE, VT9 Pro (1st gen and 185A), VT3 Pro | Receiver or cable, 24AE: 1413/4613, 1423, 14A9/46A9, 1A11/4A11, 1A00/4A00, 1205, 185A, 1215/4415 (receiver/cable) | Never asked: it only listens to the status the mouse sends every few seconds while it is used. Shown asleep after 60 s without one. Charging is not shown (the sources disagree on its meaning) |
| Bluetooth headsets, speakers, controllers and other devices for which Windows shows a battery level | Bluetooth (Classic and LE) | Windows' own cached value, read on the PC: at start, when a device connects (and again 4 s later), on resume, "Refresh now" and every 60 s while one is connected. Nothing is sent to the device |
| Xbox controllers (Microsoft) | USB cable or Xbox Wireless Adapter | Windows' game-controller battery report (four levels, shown as Full, Good, Low and Critical), read on the PC when a controller is added, on resume, "Refresh now" and every 60 s while one is present. Nothing is sent to the controller; over Bluetooth it appears as a Bluetooth device instead |

Not supported: other models of these vendors, even with the same name on the box (a different PID often means a different protocol), other HyperX, Pulsar and SteelSeries protocols, and controllers of other makes.

### When BatterySight asks a device

Apart from turning on a Logitech receiver's device notifications, it never sends a device a command that changes its settings or state; everything else it sends is a battery or connection question. It sends a battery query only when:

- the app starts or a device is plugged in; if a plugged-in device stops answering before it is recognised (a just plugged-in dongle can be busy), it is asked again after 2 s, 10 s and 60 s, then not until the next resume or "Refresh now" (Logitech),
- a device reconnects (for example a mouse waking up and linking to its receiver again),
- the PC resumes from sleep,
- you click "Refresh now" (this also asks again a plugged-in device that stopped answering before it was recognised, and a receiver none of whose devices was recognised),
- audio is playing to a headset that does not report its battery while in use (the PRO X Wireless): every 10 minutes by default (adjustable under "Check battery while in use", never more often than every 5 minutes; the first check after the headset wakes or is first found comes after 5), only while sound is actually playing, and never while the headset is known to be off or asleep. While such a headset is switched on and charging, it is also checked at that interval without any sound (these checks may delay its automatic power-off on the charger).

The table above lists the exceptions: Rapoo mice, Bluetooth devices and Xbox controllers are never asked; SteelSeries headsets, the HyperX Cloud III and Cloud II Core and the Pulsar Feinmann are not asked during use; the HyperX Cloud Alpha 2 and Pulsefire Haste 2 are asked only as they link up; the Cloud Alpha Wireless is asked during use only while sound is audible on it (never while it charges silently), and a Pulsar Fusion2 mouse only while its receiver reports it linked. According to the sources, reading a Pulsar mouse neither wakes it nor keeps it awake.

Otherwise it only listens. Opening the menu never queries; it shows the last known values. A device known to be asleep is not asked until it reports again. Audio is matched to a device through the container id Windows gives both a headset's audio endpoint and its HID interfaces; BatterySight only watches which audio sessions are active and reads the endpoint's peak meter, it never records or plays audio.

### Known limitations

- The PRO X Wireless headset does not report its battery while it is in use. Its level updates when the headset is switched on, when the PC resumes, on "Refresh now" and every 10 minutes by default (adjustable, never below 5; the first check after a wake or after it is first found comes after 5) while audio plays to it or while it charges; during a long silent stretch on battery (for example a muted call) it can lag behind.
- Bluetooth levels are only as fresh as Windows' own value, and Windows gives no time for it: the card shows "Connected" rather than how old the value is. A Bluetooth LE device may not update that value until it reconnects. A Bluetooth device appears only while it is connected and has a value, and only once it has connected since BatterySight started; a device that BatterySight also reads directly (such as a Logitech device) is shown once.
- Xbox controllers report only four levels. Their level is the one Windows holds from the controller's own reports, with no time for it either, so the card shows "Connected" while one is present. Over Bluetooth, Windows' game-controller report is wrong, so such a controller shows only its Bluetooth level. The Xbox Wireless Adapter is untested.
- While a virtual game controller whose HID path carries no vendor id (vJoy, for example) is installed and present, Xbox controllers on a USB cable or the Xbox Wireless Adapter may not be shown: BatterySight cannot tell such a device from a Bluetooth Xbox controller, so it leaves the rows out rather than risk a wrong one.
- Open questions for the source-verified devices, where a report helps: whether the SteelSeries headsets send their events on Windows without SteelSeries GG and how often, and the exact report sizes of the Nova Pro Wireless station; whether the HyperX headsets answer on every hardware revision while NGENUITY runs, and how often the Cloud Alpha and Cloud II Core report; whether the Alpha 2 and Haste 2 announce linking up on a dongle never set up by NGENUITY; whether the Pulsar Feinmann announces linking up; Rapoo's charging state and how often the VT1 SE and VT7 SE send their status.
- When "Show on" is set to one secondary monitor and that monitor is turned off and on again, the widget stays on the main taskbar until BatterySight restarts or that monitor is chosen again.

## Privacy

BatterySight collects no telemetry and makes no network calls; an update check is not yet available.

Settings and battery history live in `%LOCALAPPDATA%\BatterySight` (`settings.toml`, `history.tsv`) and contain device ids, such as unit ids, as keys; they stay on your PC. `history.tsv` holds one line per sample (time, device key, level, charging state, awake/asleep/offline, millivolts when known, where the reading came from); a line with no level, `unknown`, `offline` and `os` marks time around it that was not observed: before it when BatterySight starts or the PC wakes up, after it when the PC goes to sleep. Samples older than 30 days and lines it cannot read are removed when BatterySight starts; if that would remove complete lines it cannot read, or samples dated more than a day ahead, the file is first copied to `history.tsv.bad`.

For troubleshooting, setting the environment variable `BATTERYSIGHT_TRACE=1` makes it append to `%LOCALAPPDATA%\BatterySight\sends.log`: every report it sends and receives on every device collection it opens, of any vendor, with the report bytes (`out` and `in` lines, and `feature-set`, `feature-get` and `feature-reply` lines for feature reports), a request it refused to send (`refused write` lines) and a feature request that failed (`feature failed` lines; these two carry no report bytes), when audio starts and stops on a device or cannot be watched (`audio` lines), when a device plugged in and out too often is held back for a while and when it is opened again (`held` and `released` lines, with the device's interface paths), when a device could not be opened (`open failed` lines), when a device stopped answering before it was recognised and will be asked again or no longer (`discovery` lines), why it switched to tray icons (`ui fallback` lines), why "Start with Windows" could not be changed (`ui autostart not changed` lines), why it draws text with GDI instead of DirectWrite for the rest of the session (`ui text: DirectWrite off` lines), and when the settings or history file cannot be read or written or is set aside (`store` lines). Tracing is off by default. These lines may contain device ids (unit ids, audio container ids, interface paths and other serial-like values, including in report bytes), so review the file before sharing it.

## Usage

Start `batterysight.exe`. It shows each device's battery level on the taskbar, next to the notification area (and next to the clock on other monitors' taskbars), or left of the weather when Windows 10's News and interests or Windows 11's Widgets button sits there; on Windows 10 the task buttons are narrowed to end where it starts. A bolt follows the level while the device charges and a check once it is charged full on power; an asleep device is dimmed, and one that disconnects shows a dimmed × for about a minute before it is removed. When two shown devices are of the same kind (two mice, say), each gets a short caption from its name, such as "SUP" for a PRO X Superlight. Rest the mouse on a device for as long as a tooltip takes (Windows' tooltip delay, the double-click time) to see a small card just above the taskbar: its name and level, then "Charging" or "Full"; otherwise "Connected" for a device that reports its battery changes on its own (such as the PRO X Superlight, a SteelSeries headset or a Bluetooth device), or how long ago a device BatterySight has to ask was last read; how long ago it last reported after "Asleep" or "Disconnected" when it is; and the time left when there is an estimate, as in "PRO X Superlight · 77%" over "Connected · about 20 h left", or "PRO X Wireless Gaming Headset · 60%" over "3 min ago · about 9 h left". Tray icons show the same two lines as their tooltip. Click it for the menu:

- one submenu per device: show it on the taskbar or hide it, move it left or right, and its low-battery level (off, or 5 % to 40 %; 15 % unless you change it). A level at or below it is shown in red, and a Windows notification ("<name> is at N %") appears once when an awake, shown device that is not charging drops to it. It can appear again after the device charges or rises 10 points above that level, after you change the level, or after BatterySight restarts. A device that only reports a rough level turns red at Low or Critical and notifies as "<name> is low". In widget mode a temporary notification-area icon carries the notification and is removed a minute later.
- "Check battery while in use", in the submenu of a device BatterySight has to ask for its battery (such as the PRO X Wireless): every 5, 10, 15, 30 or 60 minutes (a Pulsar Fusion2 also offers every minute); 10 minutes (or the device's minimum, if longer) unless you change it. Intervals below the device's minimum (5 minutes for the PRO X Wireless, 1 minute for a Pulsar Fusion2) are not offered, and a shorter one written in `settings.toml` counts as the minimum. The first check after the device wakes or is first found (when BatterySight starts or the device is plugged in) comes at the minimum. These checks happen only while the device is in use (for the PRO X Wireless, while audio plays to it) or charging, never while it is idle on battery or asleep.
- "Battery history…": a window with each device's level over the last 24 hours, 7 days or 30 days, with charging shaded and gaps while the device was asleep or off or while BatterySight or the PC was not running, and an estimate of the time left ("about N h", from the most recent stretch of at least 2 hours of use without charging, sleep or a restart; otherwise "not enough data"). A sample is recorded when a device's level, charging state or awake state changes, and at most every 30 minutes otherwise, only from readings BatterySight receives anyway: recording history never wakes or queries a device. Hidden devices are recorded too. For a device whose level comes from its battery voltage (such as the PRO X Wireless), the reading it sends as it wakes reads several points high on a rested battery; when a reading in use follows within 10 minutes, the chart and the time-left estimate leave it out (it stays in `history.tsv`).
- "Show on": all monitors' taskbars, the main one only, or one monitor's.
- "Show as": the taskbar widget or one notification-area icon per device. Tray icons chosen here stay, across restarts, until you choose the widget again.
- "Start with Windows": adds or removes `BatterySight` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. It shows unchecked while Task Manager's "Startup apps" has BatterySight disabled; choosing it then enables it there again.
- "Refresh now" and "Exit".

Changes apply at once and are saved to `settings.toml`. Only one instance runs per session; starting another does nothing.

- `batterysight --tray-only` shows one notification-area icon per device instead. Use it if the widget does not appear on your Windows build. The app also switches to tray icons by itself when Windows refuses the widget window or the widget cannot find its place on the taskbar; it then tries the widget again 15 s, 1 min and 5 min later, and whenever Explorer restarts or the displays or settings change. If the widget stops responding (a stalled widget would freeze the taskbar's mouse input), it restarts itself with `--tray-only` (after about 1.5 s while Explorer is responsive, after about 5 s otherwise) and keeps tray icons until the next start. The menu's "Show as" says when tray icons show for one of these reasons; choosing "Taskbar widget" there tries the widget at once.
- `batterysight --list` prints the devices it finds, one per line, and exits: name, connection, level and charging state, the device's key and its verification status (`hardware-tested`, `source-verified`), with `asleep` for a sleeping device. It prints once no reading has changed for 5 s, and after 10 s at the latest; while a SteelSeries Nova base station is plugged in, it waits at least 8 s, because the station is asked only 5 s after it is found. The key can be a device id, such as a Logitech unit id; review the list before you share it.

### Device reports for contributors

Two read-only tools help add support for a device. Both run next to a running BatterySight, send nothing to any device and write only to the console or to a file you redirect them to; nothing is uploaded.

- `batterysight --dump > dump.txt` lists every HID collection (VID:PID, interface, usage page and usage, report lengths, the name Windows cached) and every Bluetooth device's name and whether Windows has a battery property for it.
- `batterysight --capture 046D:C547 5 > capture.txt` records, for 1 to 60 whole minutes, the input reports of that VID:PID's vendor-defined collections, opened shared and read-only (keyboard, mouse and gamepad input is never opened). Each line holds the milliseconds since the start, a collection label (`c1`, `c2`, …) and the report bytes exactly as read.

`batterysight.exe` is a Windows (GUI) program, so a console does not wait for it: in cmd the prompt returns at once and `%ERRORLEVEL%` is not its exit code. To wait for `--list`, `--dump` or `--capture` to finish (a capture runs for minutes) and to get the exit code (0 success, 1 failure, 2 wrong arguments), start it with `start /wait` in cmd or `Start-Process -Wait` in PowerShell:

```
start /wait batterysight.exe --capture 046D:C547 5 > capture.txt
echo %ERRORLEVEL%
```

```powershell
$p = Start-Process batterysight.exe -ArgumentList '--capture', '046D:C547', '5' -NoNewWindow -Wait -PassThru -RedirectStandardOutput capture.txt
$p.ExitCode
```

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
