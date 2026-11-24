//! The click menu: what it lists ([`Model`], built from the snapshot and the in-memory
//! settings only, so opening it sends no device traffic and reads no file) and the popup
//! made from it ([`track`]).

use batterysight_core::model::{DeviceKey, DeviceState};
use batterysight_core::scheduler::DEFAULT_INTERVAL_MS;
use batterysight_core::settings::{
    DisplayMode, INTERVAL_MAX_MIN, MonitorChoice, Settings, Threshold,
};
use batterysight_core::time::Ms;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, HMENU, MF_CHECKED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, PostMessageW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_NULL,
};
use windows::core::PCWSTR;

use super::fallback::TrayReason;

/// The low-battery thresholds the menu offers; a hand-edited other one is listed too.
const THRESHOLDS: [Threshold; 8] = [
    Threshold::Off,
    Threshold::Percent(5),
    Threshold::Percent(10),
    Threshold::Percent(15),
    Threshold::Percent(20),
    Threshold::Percent(25),
    Threshold::Percent(30),
    Threshold::Percent(40),
];

/// The battery-check intervals (minutes) the menu offers, those at or above the device's
/// minimum; a hand-edited other one is listed too.
const INTERVALS: [u16; 5] = [5, 10, 15, 30, 60];

/// A monitor that can show the widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// `GetMonitorInfoW`'s `szDevice`, such as `\\.\DISPLAY2`; kept in
    /// [`MonitorChoice::Named`].
    pub device: String,
    /// "Display N (WxH)".
    pub label: String,
    pub primary: bool,
}

impl MonitorInfo {
    /// `index`: the monitor's place in the enumeration, numbering a device name without a
    /// number of its own.
    pub fn new(device: String, index: usize, width: i32, height: i32, primary: bool) -> Self {
        let n = display_number(&device).unwrap_or(index + 1);
        MonitorInfo {
            label: format!("Display {n} ({width}x{height})"),
            device,
            primary,
        }
    }
}

/// The number at the end of a monitor's device name (`\\.\DISPLAY2` → 2).
pub fn display_number(device: &str) -> Option<usize> {
    let name = device.trim_end_matches(|c: char| c.is_ascii_digit());
    device[name.len()..].parse().ok()
}

/// What a menu entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    ToggleShown(DeviceKey),
    MoveLeft(DeviceKey),
    MoveRight(DeviceKey),
    Threshold(DeviceKey, Threshold),
    /// Minutes between battery checks while the device is in use.
    Interval(DeviceKey, u16),
    History,
    ShowOn(MonitorChoice),
    ShowAs(DisplayMode),
    Autostart(bool),
    Refresh,
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Entry(Entry),
    Submenu { label: String, items: Vec<Item> },
    Separator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    /// The menu id of its command ([`Model::command`]); 0 for a line without one.
    pub id: usize,
    pub checked: bool,
    pub enabled: bool,
}

/// The menu's items, and the command behind each id.
#[derive(Debug)]
pub struct Model {
    pub items: Vec<Item>,
    /// Command `i` has id `i + 1` (0 is "nothing chosen" for `TrackPopupMenu`).
    commands: Vec<MenuCommand>,
}

impl Model {
    /// The menu for `states` (the snapshot) and `settings`; `fallback`: why tray icons
    /// show the devices, if they do; `can_autostart`: whether the Run value can be written
    /// (the exe path is known).
    pub fn new(
        states: &[DeviceState],
        settings: &Settings,
        monitors: &[MonitorInfo],
        fallback: Option<TrayReason>,
        can_autostart: bool,
    ) -> Model {
        let mut b = Builder::default();
        let mut items = Vec::new();
        // Disconnected devices too: the widget shows them through the hub's removal grace.
        let listed: Vec<&DeviceState> = settings.ordered(states);
        let shown: Vec<&DeviceKey> = listed
            .iter()
            .map(|s| &s.key)
            .filter(|key| !settings.hidden.contains(*key))
            .collect();
        if listed.is_empty() {
            items.push(note("No devices found"));
        }
        for s in listed {
            let at = shown.iter().position(|key| *key == &s.key);
            items.push(b.device(s, settings, at, shown.len()));
        }
        items.push(Item::Separator);
        items.push(b.entry("Battery history…", MenuCommand::History, false, true));
        items.push(Item::Submenu {
            label: "Show on".into(),
            items: b.monitors(&settings.monitors, monitors),
        });
        items.push(Item::Submenu {
            label: "Show as".into(),
            items: b.display(settings.display, fallback),
        });
        items.push(b.entry(
            "Start with Windows",
            MenuCommand::Autostart(!settings.autostart),
            settings.autostart,
            can_autostart,
        ));
        items.push(Item::Separator);
        items.push(b.entry("Refresh now", MenuCommand::Refresh, false, true));
        items.push(b.entry("Exit", MenuCommand::Exit, false, true));
        Model {
            items,
            commands: b.commands,
        }
    }

    /// The command behind menu id `id`; `None` for 0 (nothing chosen) or an unknown id.
    pub fn command(&self, id: usize) -> Option<MenuCommand> {
        self.commands.get(id.checked_sub(1)?).cloned()
    }
}

/// Applies `command` to `settings` and says whether they changed. `shown`: the keys of the
/// devices shown, in shown order (a move skips hidden devices). Commands that are not
/// settings (history, refresh, exit) change nothing.
pub fn change(settings: &mut Settings, command: &MenuCommand, shown: &[DeviceKey]) -> bool {
    let before = settings.clone();
    match command {
        MenuCommand::ToggleShown(key) => {
            let hide = !settings.hidden.contains(key);
            settings.set_hidden(key, hide);
        }
        MenuCommand::MoveLeft(key) => settings.move_left(key, shown),
        MenuCommand::MoveRight(key) => settings.move_right(key, shown),
        // The level it already has (an explicit 15 % is the default's) is no change.
        MenuCommand::Threshold(key, threshold) if settings.threshold(key) != *threshold => {
            settings.set_threshold(key, *threshold);
        }
        MenuCommand::Interval(key, minutes) => settings.set_interval(key, *minutes),
        MenuCommand::ShowOn(choice) => settings.monitors = choice.clone(),
        MenuCommand::ShowAs(display) => settings.display = *display,
        MenuCommand::Autostart(on) => settings.autostart = *on,
        MenuCommand::Threshold(..)
        | MenuCommand::History
        | MenuCommand::Refresh
        | MenuCommand::Exit => {}
    }
    *settings != before
}

/// Numbers the commands as their entries are made.
#[derive(Default)]
struct Builder {
    commands: Vec<MenuCommand>,
}

impl Builder {
    fn entry(
        &mut self,
        label: impl Into<String>,
        command: MenuCommand,
        checked: bool,
        enabled: bool,
    ) -> Item {
        self.commands.push(command);
        Item::Entry(Entry {
            label: label.into(),
            id: self.commands.len(),
            checked,
            enabled,
        })
    }

    /// A device's submenu; `at`: its place among the `shown` devices, `None` when hidden
    /// (a hidden device does not move).
    fn device(
        &mut self,
        s: &DeviceState,
        settings: &Settings,
        at: Option<usize>,
        shown: usize,
    ) -> Item {
        let key = &s.key;
        let threshold = settings.threshold(key);
        // Ids follow the menu's order.
        let mut items = vec![
            self.entry(
                "Show on taskbar",
                MenuCommand::ToggleShown(key.clone()),
                at.is_some(),
                true,
            ),
            self.entry(
                "Move left",
                MenuCommand::MoveLeft(key.clone()),
                false,
                at.is_some_and(|i| i > 0),
            ),
            self.entry(
                "Move right",
                MenuCommand::MoveRight(key.clone()),
                false,
                at.is_some_and(|i| i + 1 < shown),
            ),
        ];
        let mut alerts = THRESHOLDS.to_vec();
        if !alerts.contains(&threshold) {
            alerts.push(threshold);
            alerts.sort_by_key(|t| match t {
                Threshold::Off => 0,
                Threshold::Percent(p) => u16::from(*p) + 1,
            });
        }
        let alerts = alerts
            .into_iter()
            .map(|t| {
                let label = match t {
                    Threshold::Off => "Off".to_owned(),
                    Threshold::Percent(p) => format!("{p} %"),
                };
                self.entry(
                    label,
                    MenuCommand::Threshold(key.clone(), t),
                    t == threshold,
                    true,
                )
            })
            .collect();
        items.push(Item::Submenu {
            label: "Low-battery alert".into(),
            items: alerts,
        });
        // Only for a device BatterySight asks while it is in use.
        if let Some(min_ms) = s.poll_min_ms {
            items.push(Item::Submenu {
                label: "Check battery while in use".into(),
                items: self.intervals(key, settings.interval(key), min_ms),
            });
        }
        Item::Submenu {
            label: super::label(s),
            items,
        }
    }

    /// "Check battery while in use": the presets at or above the device's minimum
    /// (`min_ms`, rounded up to whole minutes) and `chosen` when it is none of them. The
    /// checked entry is the interval in effect: `chosen`, or the default without one, but
    /// never below the minimum.
    fn intervals(&mut self, key: &DeviceKey, chosen: Option<u16>, min_ms: Ms) -> Vec<Item> {
        let minutes_of = |ms: Ms| {
            u16::try_from(ms.div_ceil(60_000))
                .unwrap_or(u16::MAX)
                .clamp(1, INTERVAL_MAX_MIN)
        };
        let min = minutes_of(min_ms);
        let current = chosen
            .unwrap_or_else(|| minutes_of(DEFAULT_INTERVAL_MS))
            .max(min);
        let mut minutes: Vec<u16> = INTERVALS.into_iter().filter(|&m| m >= min).collect();
        if !minutes.contains(&current) {
            minutes.push(current);
            minutes.sort_unstable();
        }
        minutes
            .into_iter()
            .map(|m| {
                self.entry(
                    format!("Every {m} min"),
                    MenuCommand::Interval(key.clone(), m),
                    m == current,
                    true,
                )
            })
            .collect()
    }

    /// "Show on": all, main, each monitor; a chosen monitor that is not connected stays
    /// listed (checked), so the choice is visible.
    fn monitors(&mut self, chosen: &MonitorChoice, monitors: &[MonitorInfo]) -> Vec<Item> {
        let mut items = vec![
            self.entry(
                "All monitors",
                MenuCommand::ShowOn(MonitorChoice::All),
                *chosen == MonitorChoice::All,
                true,
            ),
            self.entry(
                "Main monitor",
                MenuCommand::ShowOn(MonitorChoice::Main),
                *chosen == MonitorChoice::Main,
                true,
            ),
        ];
        for m in monitors {
            let label = if m.primary {
                format!("{}, main", m.label)
            } else {
                m.label.clone()
            };
            let choice = MonitorChoice::Named(m.device.clone());
            let checked = *chosen == choice;
            items.push(self.entry(label, MenuCommand::ShowOn(choice), checked, true));
        }
        if let MonitorChoice::Named(device) = chosen
            && !monitors.iter().any(|m| m.device == *device)
        {
            let name =
                display_number(device).map_or_else(|| device.clone(), |n| format!("Display {n}"));
            items.push(self.entry(
                format!("{name} (not connected)"),
                MenuCommand::ShowOn(chosen.clone()),
                true,
                true,
            ));
        }
        items
    }

    /// "Show as"; while the user's choice is the widget but tray icons show the devices,
    /// a line says why.
    fn display(&mut self, chosen: DisplayMode, fallback: Option<TrayReason>) -> Vec<Item> {
        let mut items = vec![
            self.entry(
                "Taskbar widget",
                MenuCommand::ShowAs(DisplayMode::Widget),
                chosen == DisplayMode::Widget,
                true,
            ),
            self.entry(
                "Tray icons",
                MenuCommand::ShowAs(DisplayMode::Tray),
                chosen == DisplayMode::Tray,
                true,
            ),
        ];
        let why = match fallback {
            _ if chosen == DisplayMode::Tray => None,
            None | Some(TrayReason::UserChoice) => None,
            Some(TrayReason::TrayOnlyFlag) => Some("started with --tray-only"),
            Some(TrayReason::Watchdog) => Some("the widget stopped responding"),
            Some(TrayReason::CreateFailed) => Some("Windows refused the widget"),
            Some(TrayReason::NoAnchor) => Some("no taskbar showed the widget"),
        };
        if let Some(why) = why {
            items.push(Item::Separator);
            items.push(note(&format!("Tray icons for now: {why}")));
        }
        items
    }
}

/// A line without a command.
fn note(label: &str) -> Item {
    Item::Entry(Entry {
        label: label.into(),
        id: 0,
        checked: false,
        enabled: false,
    })
}

/// Shows `model` at the cursor and returns the chosen command. The caller holds no borrow
/// of the UI state: the menu runs a modal loop.
pub fn track(model: &Model, host: HWND) -> Option<MenuCommand> {
    // SAFETY: the menus are ours and destroyed here (submenus with their parent); the
    // strings outlive the calls that copy them.
    let chosen = unsafe {
        let menu = CreatePopupMenu().ok()?;
        append(menu, &model.items);
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // The menu closes on a click elsewhere only if its owner is in the foreground.
        let _ = SetForegroundWindow(host);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            at.x,
            at.y,
            None,
            host,
            None,
        )
        .0;
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen
    };
    model.command(usize::try_from(chosen).ok()?)
}

/// Appends `items` to `menu`, submenus included.
///
/// # Safety
/// `menu` is a live menu of this thread.
unsafe fn append(menu: HMENU, items: &[Item]) {
    for item in items {
        // SAFETY: `menu` is live; `text` outlives each call, which copies it.
        unsafe {
            match item {
                Item::Separator => {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
                Item::Entry(e) => {
                    let text = wide(&menu_text(&e.label));
                    let mut flags = MF_STRING;
                    if e.checked {
                        flags |= MF_CHECKED;
                    }
                    if !e.enabled {
                        flags |= MF_GRAYED;
                    }
                    let _ = AppendMenuW(menu, flags, e.id, PCWSTR(text.as_ptr()));
                }
                Item::Submenu { label, items } => {
                    let Ok(sub) = CreatePopupMenu() else { continue };
                    append(sub, items);
                    let text = wide(&menu_text(label));
                    // A submenu that was not attached is not destroyed with `menu`.
                    let attached = AppendMenuW(
                        menu,
                        MF_STRING | MF_POPUP,
                        sub.0 as usize,
                        PCWSTR(text.as_ptr()),
                    );
                    if attached.is_err() {
                        let _ = DestroyMenu(sub);
                    }
                }
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// `label` as menu text: a single `&` would mark a mnemonic.
fn menu_text(label: &str) -> String {
    label.replace('&', "&&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{
        Charge, DeviceKind, Level, Presence, Source, Verification, Via,
    };

    fn device(key: &str, name: &str, kind: DeviceKind, percent: u8) -> DeviceState {
        DeviceState {
            key: DeviceKey(key.into()),
            name: name.into(),
            kind,
            via: Via::Receiver,
            level: Some(Level::Percent(percent)),
            millivolts: None,
            charge: Charge::Discharging,
            presence: Presence::Online,
            updated_ms: 0,
            source: Source::Query,
            verification: Verification::HardwareTested,
            poll_min_ms: None,
        }
    }

    fn two_devices() -> Vec<DeviceState> {
        vec![
            device("mouse", "PRO X Superlight", DeviceKind::Mouse, 79),
            device(
                "headset",
                "PRO X Wireless Gaming Headset",
                DeviceKind::Headset,
                77,
            ),
        ]
    }

    fn two_monitors() -> Vec<MonitorInfo> {
        vec![
            MonitorInfo::new(r"\\.\DISPLAY1".into(), 0, 2560, 1440, true),
            MonitorInfo::new(r"\\.\DISPLAY2".into(), 1, 1920, 1080, false),
        ]
    }

    /// The menu as text: submenus indented under "label ▸", checks as " ✓", disabled
    /// entries as " (disabled)", separators as "---".
    fn lines(items: &[Item]) -> Vec<String> {
        fn walk(items: &[Item], depth: usize, out: &mut Vec<String>) {
            let indent = "  ".repeat(depth);
            for item in items {
                match item {
                    Item::Entry(e) => out.push(format!(
                        "{indent}{}{}{}",
                        e.label,
                        if e.checked { " ✓" } else { "" },
                        if e.enabled { "" } else { " (disabled)" },
                    )),
                    Item::Submenu { label, items } => {
                        out.push(format!("{indent}{label} ▸"));
                        walk(items, depth + 1, out);
                    }
                    Item::Separator => out.push(format!("{indent}---")),
                }
            }
        }
        let mut out = Vec::new();
        walk(items, 0, &mut out);
        out
    }

    fn thresholds(checked: &str) -> Vec<String> {
        ["Off", "5 %", "10 %", "15 %", "20 %", "25 %", "30 %", "40 %"]
            .iter()
            .map(|t| {
                let mark = if *t == checked { " ✓" } else { "" };
                format!("    {t}{mark}")
            })
            .collect()
    }

    /// Every entry with a command, as (id, label).
    fn entries(items: &[Item]) -> Vec<&Entry> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Item::Entry(e) => out.push(e),
                Item::Submenu { items, .. } => out.extend(entries(items)),
                Item::Separator => {}
            }
        }
        out
    }

    #[test]
    fn menu_model_matches_spec() {
        let model = Model::new(
            &two_devices(),
            &Settings::default(),
            &two_monitors(),
            None,
            true,
        );
        let mut expected = vec!["PRO X Superlight — 79% ▸".to_owned()];
        expected.extend(
            [
                "  Show on taskbar ✓",
                "  Move left (disabled)",
                "  Move right",
                "  Low-battery alert ▸",
            ]
            .map(String::from),
        );
        expected.extend(thresholds("15 %"));
        expected.push("PRO X Wireless Gaming Headset — 77% ▸".into());
        expected.extend(
            [
                "  Show on taskbar ✓",
                "  Move left",
                "  Move right (disabled)",
                "  Low-battery alert ▸",
            ]
            .map(String::from),
        );
        expected.extend(thresholds("15 %"));
        expected.extend(
            [
                "---",
                "Battery history…",
                "Show on ▸",
                "  All monitors ✓",
                "  Main monitor",
                "  Display 1 (2560x1440), main",
                "  Display 2 (1920x1080)",
                "Show as ▸",
                "  Taskbar widget ✓",
                "  Tray icons",
                "Start with Windows",
                "---",
                "Refresh now",
                "Exit",
            ]
            .map(String::from),
        );
        assert_eq!(lines(&model.items), expected);
    }

    #[test]
    fn menu_follows_settings() {
        let mouse = DeviceKey("mouse".into());
        let headset = DeviceKey("headset".into());
        let mut settings = Settings {
            order: vec![headset.clone(), mouse.clone()],
            monitors: MonitorChoice::Named(r"\\.\DISPLAY2".into()),
            display: DisplayMode::Tray,
            autostart: true,
            ..Settings::default()
        };
        settings.set_hidden(&mouse, true);
        settings.set_threshold(&headset, Threshold::Off);
        // Hand-edited: not one of the menu's values.
        settings.set_threshold(&mouse, Threshold::Percent(80));
        let model = Model::new(
            &two_devices(),
            &settings,
            &two_monitors(),
            Some(TrayReason::UserChoice),
            true,
        );
        let text = lines(&model.items);
        let at = |line: &str| text.iter().position(|l| l == line).unwrap();
        // User order; the hidden mouse stays listed, unchecked, and cannot move.
        assert!(at("PRO X Wireless Gaming Headset — 77% ▸") < at("PRO X Superlight — 79% ▸"));
        let mouse_at = at("PRO X Superlight — 79% ▸");
        assert_eq!(
            text[mouse_at + 1..mouse_at + 4],
            [
                "  Show on taskbar",
                "  Move left (disabled)",
                "  Move right (disabled)"
            ]
        );
        // The headset is the only one shown: it cannot move either.
        let headset_at = at("PRO X Wireless Gaming Headset — 77% ▸");
        assert_eq!(
            text[headset_at + 1..headset_at + 6],
            [
                "  Show on taskbar ✓",
                "  Move left (disabled)",
                "  Move right (disabled)",
                "  Low-battery alert ▸",
                "    Off ✓",
            ]
        );
        // The hand-edited 80 % shows checked, in its place after 40 %.
        assert_eq!(
            text[mouse_at + 12..mouse_at + 14],
            ["    40 %", "    80 % ✓"]
        );
        assert!(text.contains(&"  Display 2 (1920x1080) ✓".to_owned()));
        assert!(text.contains(&"  All monitors".to_owned()));
        assert!(text.contains(&"  Tray icons ✓".to_owned()));
        assert!(text.contains(&"Start with Windows ✓".to_owned()));
        // The user's own choice needs no explanation.
        assert!(!text.iter().any(|l| l.contains("for now")));
    }

    #[test]
    fn named_monitor_not_connected_stays_checked() {
        let settings = Settings {
            monitors: MonitorChoice::Named(r"\\.\DISPLAY3".into()),
            ..Settings::default()
        };
        let model = Model::new(&two_devices(), &settings, &two_monitors(), None, true);
        let text = lines(&model.items);
        assert!(text.contains(&"  Display 3 (not connected) ✓".to_owned()));
    }

    #[test]
    fn automatic_fallback_is_explained() {
        for (reason, why) in [
            (TrayReason::Watchdog, "the widget stopped responding"),
            (TrayReason::TrayOnlyFlag, "started with --tray-only"),
            (TrayReason::CreateFailed, "Windows refused the widget"),
            (TrayReason::NoAnchor, "no taskbar showed the widget"),
        ] {
            let model = Model::new(
                &two_devices(),
                &Settings::default(),
                &two_monitors(),
                Some(reason),
                true,
            );
            let text = lines(&model.items);
            let at = text.iter().position(|l| l == "Show as ▸").unwrap();
            assert_eq!(
                text[at + 1..at + 5],
                [
                    "  Taskbar widget ✓".to_owned(),
                    "  Tray icons".to_owned(),
                    "  ---".to_owned(),
                    format!("  Tray icons for now: {why} (disabled)"),
                ],
                "{reason:?}"
            );
        }
    }

    #[test]
    fn no_devices_found() {
        let model = Model::new(&[], &Settings::default(), &two_monitors(), None, true);
        assert_eq!(
            lines(&model.items)[..3],
            [
                "No devices found (disabled)",
                "---",
                // History is kept for devices not connected now.
                "Battery history…"
            ]
        );
    }

    #[test]
    fn disconnected_devices_are_listed_with_their_submenu() {
        // The widget shows a disconnected device (through the hub's removal grace), so the
        // menu lists it, and counts it among the shown devices for moves.
        let mut states = two_devices();
        states[0].presence = Presence::Offline;
        let model = Model::new(&states, &Settings::default(), &two_monitors(), None, true);
        assert_eq!(
            lines(&model.items)[..5],
            [
                "PRO X Superlight — disconnected ▸",
                "  Show on taskbar ✓",
                "  Move left (disabled)",
                "  Move right",
                "  Low-battery alert ▸",
            ]
        );
    }

    #[test]
    fn command_ids_round_trip() {
        let mouse = DeviceKey("mouse".into());
        let headset = DeviceKey("headset".into());
        let model = Model::new(
            &two_devices(),
            &Settings::default(),
            &two_monitors(),
            None,
            true,
        );
        let entries = entries(&model.items);
        let mut ids: Vec<usize> = entries.iter().map(|e| e.id).filter(|&id| id != 0).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "ids are unique");
        let decoded: Vec<MenuCommand> = ids.iter().map(|&id| model.command(id).unwrap()).collect();
        let mut expected = Vec::new();
        for key in [&mouse, &headset] {
            expected.extend([
                MenuCommand::ToggleShown(key.clone()),
                MenuCommand::MoveLeft(key.clone()),
                MenuCommand::MoveRight(key.clone()),
            ]);
            for t in [0, 5, 10, 15, 20, 25, 30, 40] {
                let t = if t == 0 {
                    Threshold::Off
                } else {
                    Threshold::Percent(t)
                };
                expected.push(MenuCommand::Threshold(key.clone(), t));
            }
        }
        expected.extend([
            MenuCommand::History,
            MenuCommand::ShowOn(MonitorChoice::All),
            MenuCommand::ShowOn(MonitorChoice::Main),
            MenuCommand::ShowOn(MonitorChoice::Named(r"\\.\DISPLAY1".into())),
            MenuCommand::ShowOn(MonitorChoice::Named(r"\\.\DISPLAY2".into())),
            MenuCommand::ShowAs(DisplayMode::Widget),
            MenuCommand::ShowAs(DisplayMode::Tray),
            MenuCommand::Autostart(true),
            MenuCommand::Refresh,
            MenuCommand::Exit,
        ]);
        assert_eq!(decoded, expected);
        // Each command's entry carries the id that decodes to it.
        for e in entries.iter().filter(|e| e.id != 0) {
            let command = model.command(e.id).unwrap();
            let again = entries
                .iter()
                .find(|other| model.command(other.id).as_ref() == Some(&command))
                .unwrap();
            assert_eq!(again.id, e.id);
        }
        assert_eq!(model.command(0), None);
        assert_eq!(model.command(count + 1), None);
    }

    #[test]
    fn autostart_entry_turns_it_off_when_on() {
        let settings = Settings {
            autostart: true,
            ..Settings::default()
        };
        let model = Model::new(&two_devices(), &settings, &two_monitors(), None, true);
        let entry = entries(&model.items)
            .into_iter()
            .find(|e| e.label == "Start with Windows")
            .unwrap();
        assert_eq!(model.command(entry.id), Some(MenuCommand::Autostart(false)));
    }

    #[test]
    fn autostart_is_greyed_without_the_exe_path() {
        let model = Model::new(
            &two_devices(),
            &Settings::default(),
            &two_monitors(),
            None,
            false,
        );
        assert!(lines(&model.items).contains(&"Start with Windows (disabled)".to_owned()));
    }

    fn keys(list: &[&str]) -> Vec<DeviceKey> {
        list.iter().map(|k| DeviceKey((*k).into())).collect()
    }

    #[test]
    fn commands_change_settings() {
        let shown = keys(&["a", "b", "c"]);
        let [a, b, _] = &shown[..] else {
            unreachable!()
        };
        let mut s = Settings::default();
        assert!(change(&mut s, &MenuCommand::ToggleShown(b.clone()), &shown));
        assert!(s.hidden.contains(b));
        assert!(change(&mut s, &MenuCommand::ToggleShown(b.clone()), &shown));
        assert!(s.hidden.is_empty());
        assert!(change(&mut s, &MenuCommand::MoveRight(a.clone()), &shown));
        assert_eq!(s.order, keys(&["b", "a", "c"]));
        assert!(change(&mut s, &MenuCommand::MoveLeft(a.clone()), &shown));
        assert_eq!(s.order, keys(&["a", "b", "c"]));
        let thirty = MenuCommand::Threshold(a.clone(), Threshold::Percent(30));
        assert!(change(&mut s, &thirty, &shown));
        assert_eq!(s.threshold(a), Threshold::Percent(30));
        assert!(change(
            &mut s,
            &MenuCommand::ShowOn(MonitorChoice::Main),
            &shown
        ));
        assert_eq!(s.monitors, MonitorChoice::Main);
        assert!(change(
            &mut s,
            &MenuCommand::ShowAs(DisplayMode::Tray),
            &shown
        ));
        assert_eq!(s.display, DisplayMode::Tray);
        assert!(change(&mut s, &MenuCommand::Autostart(true), &shown));
        assert!(s.autostart);
    }

    #[test]
    fn commands_that_change_nothing_say_so() {
        let shown = keys(&["a", "b"]);
        let mut s = Settings::default();
        let a = shown[0].clone();
        for command in [
            // The checked entries again.
            MenuCommand::Threshold(a.clone(), Threshold::Percent(15)),
            MenuCommand::ShowOn(MonitorChoice::All),
            MenuCommand::ShowAs(DisplayMode::Widget),
            MenuCommand::Autostart(false),
            // Not settings.
            MenuCommand::History,
            MenuCommand::Refresh,
            MenuCommand::Exit,
        ] {
            assert!(!change(&mut s, &command, &shown), "{command:?}");
        }
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn monitor_label_numbers_by_device_name() {
        let m = MonitorInfo::new(r"\\.\DISPLAY12".into(), 0, 3840, 2160, false);
        assert_eq!(m.label, "Display 12 (3840x2160)");
        // No number in the name: the place in the enumeration.
        let m = MonitorInfo::new("WEIRD".into(), 2, 800, 600, false);
        assert_eq!(m.label, "Display 3 (800x600)");
    }

    /// The mouse and the headset, the headset asked during use with minimum `min_ms`.
    fn polled_headset(min_ms: u64) -> Vec<DeviceState> {
        let mut states = two_devices();
        states[1].poll_min_ms = Some(min_ms);
        states
    }

    /// The entries of the only "Check battery while in use" submenu.
    fn intervals(states: &[DeviceState], settings: &Settings) -> Vec<String> {
        let model = Model::new(states, settings, &two_monitors(), None, true);
        let text = lines(&model.items);
        let at = text
            .iter()
            .position(|l| l == "  Check battery while in use ▸")
            .unwrap();
        text[at + 1..]
            .iter()
            .take_while(|l| l.starts_with("    "))
            .map(|l| l.trim_start().to_owned())
            .collect()
    }

    #[test]
    fn interval_submenu_only_for_polled_devices() {
        let model = Model::new(
            &polled_headset(300_000),
            &Settings::default(),
            &two_monitors(),
            None,
            true,
        );
        let text = lines(&model.items);
        let submenu = "  Check battery while in use ▸";
        assert_eq!(text.iter().filter(|l| *l == submenu).count(), 1);
        // The headset's, right after its low-battery alert.
        let headset_at = text
            .iter()
            .position(|l| l == "PRO X Wireless Gaming Headset — 77% ▸")
            .unwrap();
        assert_eq!(text[headset_at + 4], "  Low-battery alert ▸");
        assert_eq!(text[headset_at + 13], submenu);
        // Nothing for the mouse, which reports by itself.
        let mouse_at = text
            .iter()
            .position(|l| l == "PRO X Superlight — 79% ▸")
            .unwrap();
        assert!(mouse_at < headset_at);
        assert!(!text[mouse_at..headset_at].contains(&submenu.to_owned()));
    }

    #[test]
    fn interval_presets_respect_minimum() {
        let settings = Settings::default();
        assert_eq!(
            intervals(&polled_headset(600_000), &settings),
            [
                "Every 10 min ✓",
                "Every 15 min",
                "Every 30 min",
                "Every 60 min"
            ]
        );
        // The 5-minute headset: 5 offered, the 10-minute default checked.
        assert_eq!(
            intervals(&polled_headset(300_000), &settings),
            [
                "Every 5 min",
                "Every 10 min ✓",
                "Every 15 min",
                "Every 30 min",
                "Every 60 min"
            ]
        );
        // A minimum between presets is rounded up to whole minutes: only presets above it.
        assert_eq!(
            intervals(&polled_headset(400_000), &settings),
            [
                "Every 10 min ✓",
                "Every 15 min",
                "Every 30 min",
                "Every 60 min"
            ]
        );
        // A minimum above the default is the default.
        assert_eq!(
            intervals(&polled_headset(900_000), &settings),
            ["Every 15 min ✓", "Every 30 min", "Every 60 min"]
        );
    }

    #[test]
    fn hand_edited_interval_is_listed() {
        let mut settings = Settings::default();
        settings.set_interval(&DeviceKey("headset".into()), 45);
        assert_eq!(
            intervals(&polled_headset(300_000), &settings),
            [
                "Every 5 min",
                "Every 10 min",
                "Every 15 min",
                "Every 30 min",
                "Every 45 min ✓",
                "Every 60 min"
            ]
        );
    }

    #[test]
    fn interval_below_minimum_shows_minimum_checked() {
        let mut settings = Settings::default();
        settings.set_interval(&DeviceKey("headset".into()), 1);
        assert_eq!(
            intervals(&polled_headset(300_000), &settings),
            [
                "Every 5 min ✓",
                "Every 10 min",
                "Every 15 min",
                "Every 30 min",
                "Every 60 min"
            ]
        );
    }

    #[test]
    fn interval_entries_carry_their_command() {
        let headset = DeviceKey("headset".into());
        let model = Model::new(
            &polled_headset(300_000),
            &Settings::default(),
            &two_monitors(),
            None,
            true,
        );
        let entry = entries(&model.items)
            .into_iter()
            .find(|e| e.label == "Every 30 min")
            .unwrap();
        assert_eq!(
            model.command(entry.id),
            Some(MenuCommand::Interval(headset, 30))
        );
    }

    #[test]
    fn interval_command_sets_and_reports_change() {
        let shown = keys(&["a", "b"]);
        let a = &shown[0];
        let mut s = Settings::default();
        let thirty = MenuCommand::Interval(a.clone(), 30);
        assert!(change(&mut s, &thirty, &shown));
        assert_eq!(s.interval(a), Some(30));
        // The value it already has.
        assert!(!change(&mut s, &thirty, &shown));
        assert_eq!(s.interval(a), Some(30));
        let ten = MenuCommand::Interval(a.clone(), 10);
        assert!(change(&mut s, &ten, &shown));
        assert_eq!(s.interval(a), Some(10));
    }

    #[test]
    fn ampersands_are_kept_literal() {
        let states = [device("k", "Keys & Mice", DeviceKind::Keyboard, 50)];
        let model = Model::new(&states, &Settings::default(), &[], None, true);
        let Item::Submenu { label, .. } = &model.items[0] else {
            panic!("device submenu first");
        };
        assert_eq!(label, "Keys & Mice — 50%");
        assert_eq!(menu_text(label), "Keys && Mice — 50%");
    }
}
