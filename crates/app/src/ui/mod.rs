//! The app's UI: the taskbar widget, the tray-icon fallback and their menu.
//!
//! # Threads
//!
//! The UI thread (the main thread) owns a hidden host window, the widget windows (children
//! of Explorer's taskbars, so Explorer's taskbar thread shares this thread's input queue)
//! and, in tray mode, the tray icons. It only paints, positions and handles menu and mouse
//! input; its calls to Explorer are the ones `widget` and `tray` list. The device runtime's
//! `on_change` runs on the device thread and only posts [`WM_APP_CHANGED`] here; the menu
//! reads the snapshot and sends no device traffic. UI Automation runs on its own windowless
//! thread (`widget`), and the [`watchdog`] on another.
//!
//! # Re-entrancy
//!
//! The calls that reach Explorer (`CreateWindowExW` and `DestroyWindow` of a child,
//! `SetWindowPos`, `UpdateLayeredWindow`, `Shell_NotifyIconW`) dispatch Explorer's sent
//! messages, such as `TaskbarCreated`, before they return, so the window procedures can be
//! re-entered while the UI state is borrowed. One rule covers it:
//!
//! - Messages from outside (`TaskbarCreated`, `WM_SETTINGCHANGE`, `WM_DISPLAYCHANGE`,
//!   `WM_CLOSE`, WinEvent callbacks, `WM_DPICHANGED_AFTERPARENT`, clicks and tray callbacks)
//!   never touch the state where they arrive: they post one of our `WM_APP_*` messages.
//!   `WM_TIMER` only beats the watchdog.
//! - Only `WM_APP_*` handlers borrow the state. One that arrives while the state is borrowed
//!   (a nested dispatch) is re-posted and handled after the current handler returns.
//! - No borrow is held across a modal loop: the menu is built from a snapshot read, then
//!   tracked with the state released.

pub mod render;
mod tray;
pub mod watchdog;
mod widget;

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Duration;

use batterysight_core::model::{
    Charge, CoarseLevel, DeviceKind, DeviceState, Level, Presence, Verification,
};
use batterysight_win::runtime::{self, RuntimeHandle};
use windows::Win32::Foundation::{ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::{NIN_SELECT, NINF_KEY};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, IDC_ARROW, KillTimer, LoadCursorW, MF_DISABLED,
    MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TPM_BOTTOMALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_NULL, WM_SETTINGCHANGE, WM_TIMER,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use tray::Tray;
use widget::Widgets;

/// A device at or below this percentage is shown as low.
const LOW_PERCENT: u8 = 15;

/// A tray icon chosen with the keyboard (not defined by the `windows` crate).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

/// What the UI shows for one device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceView {
    pub glyph: char,
    pub text: String,
    pub dimmed: bool,
    pub charging: bool,
    pub low: bool,
    pub tooltip: String,
}

/// The devices to show, in snapshot order; offline devices are left out.
pub fn views(states: &[DeviceState]) -> Vec<DeviceView> {
    states
        .iter()
        .filter(|s| s.presence != Presence::Offline)
        .map(view)
        .collect()
}

fn view(s: &DeviceState) -> DeviceView {
    // Segoe Fluent Icons code points (the same in Segoe MDL2 Assets).
    let glyph = match s.kind {
        DeviceKind::Mouse => '\u{E962}',
        DeviceKind::Keyboard => '\u{E765}',
        DeviceKind::Headset => '\u{E7F6}',
        DeviceKind::Controller => '\u{E7FC}',
        DeviceKind::Other => '\u{E83F}',
    };
    let text = match s.level {
        Some(Level::Percent(p)) => format!("{p}%"),
        Some(Level::Coarse(c)) => match c {
            CoarseLevel::Critical => "Critical",
            CoarseLevel::Low => "Low",
            CoarseLevel::Good => "Good",
            CoarseLevel::Full => "Full",
        }
        .to_owned(),
        None => "?".to_owned(),
    };
    let charging = s.charge == Charge::Charging;
    let mut tooltip = format!("{} — {text}", s.name);
    if charging {
        tooltip.push_str(" charging");
    }
    tooltip.push_str(match s.verification {
        Verification::CommunityTested => " (community-tested)",
        Verification::SourceVerified => " (not yet tested on hardware)",
        Verification::HardwareTested | Verification::Generic => "",
    });
    DeviceView {
        glyph,
        text,
        dimmed: s.presence != Presence::Online,
        charging,
        low: matches!(s.level, Some(Level::Percent(p)) if p <= LOW_PERCENT),
        tooltip,
    }
}

const HOST_CLASS: PCWSTR = w!("BatterySightHost");
const WIDGET_CLASS: PCWSTR = w!("BatterySightWidget");

/// The snapshot changed (posted by the device thread).
const WM_APP_CHANGED: u32 = WM_APP + 1;
/// Lay the widgets out again (a taskbar moved or resized, or the DPI changed).
const WM_APP_LAYOUT: u32 = WM_APP + 2;
/// The UI Automation thread found the secondary taskbars' clocks.
const WM_APP_CLOCKS: u32 = WM_APP + 3;
/// Explorer restarted (`TaskbarCreated`).
const WM_APP_TASKBAR_CREATED: u32 = WM_APP + 4;
/// Displays or settings changed (`WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE`).
const WM_APP_ENVIRONMENT: u32 = WM_APP + 5;
/// "Refresh now" was chosen.
const WM_APP_REFRESH: u32 = WM_APP + 6;
/// "Exit" was chosen, or the host was asked to close.
const WM_APP_EXIT: u32 = WM_APP + 7;
/// Open the menu (a click on the widget or a tray icon).
const WM_APP_MENU: u32 = WM_APP + 8;
/// Tray icon callbacks (`NOTIFYICON_VERSION_4`).
const WM_APP_TRAY: u32 = WM_APP + 9;

const HEARTBEAT_TIMER: usize = 1;
const MENU_REFRESH: usize = 1;
const MENU_EXIT: usize = 2;

/// Sleep of the widget thread after the first click with `--debug-stall-widget`.
const DEBUG_STALL: Duration = Duration::from_secs(10);

/// The host window, for posting from any thread.
static HOST: AtomicIsize = AtomicIsize::new(0);
/// The registered `TaskbarCreated` message.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// Whether the next left click on the widget stalls its thread (`--debug-stall-widget`).
static DEBUG_STALL_ARMED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The UI state; lives on the UI thread only. See the module doc for how it is borrowed.
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    /// A `WM_APP_LAYOUT` is queued.
    static LAYOUT_PENDING: Cell<bool> = const { Cell::new(false) };
}

pub struct Options {
    /// Tray icons instead of the widget for this session.
    pub tray_only: bool,
    /// Debug builds only: stall the widget thread for 10 s after the first click.
    pub debug_stall: bool,
}

/// Runs the UI on this thread until "Exit". Returns the process exit code.
pub fn run(options: &Options) -> i32 {
    DEBUG_STALL_ARMED.store(options.debug_stall, Ordering::Relaxed);
    let Some(host) = create_host() else {
        return 1;
    };
    // The runtime starts before any widget exists: its start blocks this thread, which must
    // not happen while Explorer shares the thread's input queue.
    let runtime = match runtime::start(Box::new(|| post(WM_APP_CHANGED))) {
        Ok(runtime) => runtime,
        Err(_) => {
            // SAFETY: our own window.
            let _ = unsafe { DestroyWindow(host) };
            return 1;
        }
    };
    let light = light_theme();
    let views = views(&read_snapshot(&runtime));
    let surface = if options.tray_only {
        None
    } else {
        Widgets::new(&views, light).ok().map(Surface::Widgets)
    };
    let surface = surface.unwrap_or_else(|| {
        let mut tray = Tray::new(host);
        tray.show(&views, light);
        Surface::Tray(tray)
    });
    if matches!(surface, Surface::Widgets(_)) {
        watchdog::arm(true);
        watchdog::spawn();
    }
    UI.with(|ui| {
        *ui.borrow_mut() = Some(Ui {
            host,
            runtime,
            views,
            light,
            surface,
        });
    });
    // SAFETY: our own window; the timer beats the watchdog, also inside the menu's loop.
    unsafe { SetTimer(Some(host), HEARTBEAT_TIMER, watchdog::INTERVAL_MS, None) };
    let mut msg = MSG::default();
    // SAFETY: `msg` outlives each call. GetMessageW returns -1 on error, 0 on WM_QUIT.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        watchdog::beat();
        // SAFETY: `msg` came from GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // SAFETY: our own window and timer.
    unsafe {
        let _ = KillTimer(Some(host), HEARTBEAT_TIMER);
        let _ = DestroyWindow(host);
    }
    0
}

/// What shows the devices.
enum Surface {
    Widgets(Widgets),
    Tray(Tray),
}

struct Ui {
    host: HWND,
    runtime: RuntimeHandle,
    /// What is shown, from the last snapshot read.
    views: Vec<DeviceView>,
    /// Whether the taskbar uses the light theme.
    light: bool,
    surface: Surface,
}

impl Ui {
    fn handle(&mut self, msg: u32) {
        match msg {
            WM_APP_CHANGED => {
                let views = views(&read_snapshot(&self.runtime));
                if views != self.views {
                    self.views = views;
                    if let Surface::Widgets(w) = &mut self.surface {
                        w.invalidate();
                    }
                    self.show();
                }
            }
            WM_APP_LAYOUT => self.show(),
            WM_APP_CLOCKS => {
                if let Surface::Widgets(w) = &mut self.surface {
                    w.take_clocks();
                }
                self.show();
            }
            WM_APP_TASKBAR_CREATED => {
                let result = match &mut self.surface {
                    Surface::Widgets(w) => w.taskbar_created(&self.views, self.light),
                    Surface::Tray(t) => {
                        t.readd(&self.views, self.light);
                        Ok(())
                    }
                };
                if result.is_err() {
                    self.fall_back_to_tray();
                }
            }
            WM_APP_ENVIRONMENT => {
                self.light = light_theme();
                let result = match &mut self.surface {
                    Surface::Widgets(w) => w.environment_changed(&self.views, self.light),
                    Surface::Tray(t) => {
                        t.show(&self.views, self.light);
                        Ok(())
                    }
                };
                if result.is_err() {
                    self.fall_back_to_tray();
                }
            }
            WM_APP_REFRESH => self.runtime.refresh_now(),
            _ => {}
        }
    }

    /// Shows the current views and lays the widgets out.
    fn show(&mut self) {
        let result = match &mut self.surface {
            Surface::Widgets(w) => w.layout(&self.views, self.light),
            Surface::Tray(t) => {
                t.show(&self.views, self.light);
                Ok(())
            }
        };
        if result.is_err() {
            self.fall_back_to_tray();
        }
    }

    /// Windows refused a widget window: tray icons for the rest of the session.
    fn fall_back_to_tray(&mut self) {
        watchdog::arm(false);
        let mut tray = Tray::new(self.host);
        tray.show(&self.views, self.light);
        // Dropping the widgets destroys their windows and hooks.
        self.surface = Surface::Tray(tray);
    }

    /// Removes the widgets or tray icons, then stops the runtime (bounded by its 2 s limit)
    /// and ends the message loop.
    fn exit(self) {
        watchdog::arm(false);
        drop(self.surface);
        self.runtime.stop();
        // SAFETY: no pointers involved.
        unsafe { PostQuitMessage(0) };
    }

    /// One line per device for the menu, from the snapshot: no device traffic.
    fn menu_lines(&self) -> Vec<String> {
        views(&read_snapshot(&self.runtime))
            .into_iter()
            .map(|v| v.tooltip)
            .collect()
    }
}

fn read_snapshot(runtime: &RuntimeHandle) -> Vec<DeviceState> {
    runtime
        .snapshot
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Runs `f` on the UI state; `None` when it is borrowed (a nested dispatch) or gone.
fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|ui| ui.try_borrow_mut().ok()?.as_mut().map(f))
}

fn ui_busy() -> bool {
    UI.with(|ui| ui.try_borrow_mut().is_err())
}

/// Posts `msg` to the host window, from any thread.
fn post(msg: u32) {
    let host = HWND(HOST.load(Ordering::Relaxed) as *mut c_void);
    if !host.is_invalid() {
        // SAFETY: no pointers involved; a stale handle only makes the post fail.
        let _ = unsafe { PostMessageW(Some(host), msg, WPARAM(0), LPARAM(0)) };
    }
}

/// Queues one `WM_APP_LAYOUT`, however many events ask for it.
fn schedule_layout() {
    if !LAYOUT_PENDING.replace(true) {
        post(WM_APP_LAYOUT);
    }
}

/// Whether this click should stall the widget thread (`--debug-stall-widget`, once).
fn take_debug_stall() -> bool {
    DEBUG_STALL_ARMED.swap(false, Ordering::Relaxed)
}

fn hinstance() -> HINSTANCE {
    // SAFETY: the module handle of this exe.
    unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_or_else(|_| HINSTANCE::default(), |m| HINSTANCE(m.0))
}

/// Registers the window classes and creates the hidden host window. It is a top-level
/// window rather than a message-only one, which would not receive broadcasts such as
/// `TaskbarCreated`.
fn create_host() -> Option<HWND> {
    let instance = hinstance();
    // SAFETY: literal strings; the classes' procedures live for the whole process.
    unsafe {
        let host_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(host_proc),
            hInstance: instance,
            lpszClassName: HOST_CLASS,
            ..Default::default()
        };
        let widget_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(widget::widget_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: WIDGET_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&host_class) == 0 || RegisterClassExW(&widget_class) == 0 {
            return None;
        }
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );
        let host = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            HOST_CLASS,
            w!("BatterySight"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()?;
        HOST.store(host.0 as isize, Ordering::Relaxed);
        Some(host)
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
    match msg {
        WM_TIMER => watchdog::beat(),
        // Never handled where they arrive (see the module doc); WM_SETTINGCHANGE's string
        // is not needed, every change is re-read.
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE => post(WM_APP_ENVIRONMENT),
        _ if msg == taskbar_created && msg != 0 => post(WM_APP_TASKBAR_CREATED),
        WM_CLOSE => post(WM_APP_EXIT),
        WM_ENDSESSION if wp.0 != 0 => exit(),
        WM_APP_EXIT => exit(),
        WM_APP_MENU => menu(hwnd),
        WM_APP_TRAY => {
            let event = (lp.0 & 0xFFFF) as u32;
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                post(WM_APP_MENU);
            }
        }
        WM_APP_CHANGED..=WM_APP_REFRESH => {
            if ui_busy() {
                // SAFETY: no pointers involved.
                let _ = unsafe { PostMessageW(Some(hwnd), msg, wp, lp) };
                return LRESULT(0);
            }
            if msg == WM_APP_LAYOUT {
                LAYOUT_PENDING.set(false);
            }
            with_ui(|ui| ui.handle(msg));
        }
        // SAFETY: forwarding this window's own message.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

/// Takes the state out first, so nothing can be handled on it while it shuts down.
fn exit() {
    if ui_busy() {
        post(WM_APP_EXIT);
        return;
    }
    if let Some(ui) = UI.with(|ui| ui.borrow_mut().take()) {
        ui.exit();
    }
}

/// Shows the menu at the cursor: one read-only line per device, "Refresh now" and "Exit".
fn menu(host: HWND) {
    if ui_busy() {
        post(WM_APP_MENU);
        return;
    }
    let Some(lines) = with_ui(|ui| ui.menu_lines()) else {
        return;
    };
    // SAFETY: the menu and the strings outlive their use; the state is not borrowed while
    // the menu's modal loop runs.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        if lines.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, w!("No devices found"));
        }
        for line in &lines {
            // A single `&` would mark a mnemonic.
            let text: Vec<u16> = line
                .replace('&', "&&")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let _ = AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, PCWSTR(text.as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_REFRESH, w!("Refresh now"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("Exit"));
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
        .0 as usize;
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match chosen {
            MENU_REFRESH => post(WM_APP_REFRESH),
            MENU_EXIT => post(WM_APP_EXIT),
            _ => {}
        }
    }
}

/// Whether the taskbar uses the light theme (it follows the Windows mode, not the app mode).
fn light_theme() -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` and `size` outlive the call and match RRF_RT_REG_DWORD.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && value != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{
        Charge, CoarseLevel, DeviceKey, DeviceKind, Level, Presence, Source, Verification, Via,
    };

    fn state(name: &str, kind: DeviceKind, level: Option<Level>) -> DeviceState {
        DeviceState {
            key: DeviceKey(name.into()),
            name: name.into(),
            kind,
            via: Via::Receiver,
            level,
            millivolts: None,
            charge: Charge::Discharging,
            presence: Presence::Online,
            updated_ms: 0,
            source: Source::Query,
            verification: Verification::HardwareTested,
        }
    }

    fn percent(name: &str, p: u8) -> DeviceState {
        state(name, DeviceKind::Mouse, Some(Level::Percent(p)))
    }

    #[test]
    fn asleep_is_dimmed_and_keeps_text() {
        let s = DeviceState {
            presence: Presence::Asleep,
            ..percent("M", 40)
        };
        let v = views(&[s]);
        assert_eq!(v.len(), 1);
        assert!(v[0].dimmed);
        assert_eq!(v[0].text, "40%");
        assert!(!views(&[percent("M", 40)])[0].dimmed);
    }

    #[test]
    fn low_threshold_is_15() {
        let v = views(&[percent("A", 15), percent("B", 16), percent("C", 0)]);
        assert_eq!(
            v.iter().map(|v| v.low).collect::<Vec<_>>(),
            [true, false, true]
        );
    }

    #[test]
    fn offline_devices_hidden() {
        let off = DeviceState {
            presence: Presence::Offline,
            ..percent("Off", 50)
        };
        let v = views(&[off, percent("On", 60)]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "60%");
    }

    #[test]
    fn tooltip_labels_status() {
        let with = |verification| DeviceState {
            verification,
            ..percent("PRO X", 40)
        };
        let tips: Vec<String> = [
            Verification::HardwareTested,
            Verification::SourceVerified,
            Verification::CommunityTested,
            Verification::Generic,
        ]
        .into_iter()
        .map(|status| views(&[with(status)])[0].tooltip.clone())
        .collect();
        assert_eq!(
            tips,
            [
                "PRO X — 40%",
                "PRO X — 40% (not yet tested on hardware)",
                "PRO X — 40% (community-tested)",
                "PRO X — 40%",
            ]
        );
    }

    #[test]
    fn charging_in_flag_and_tooltip() {
        let s = DeviceState {
            charge: Charge::Charging,
            ..percent("M", 70)
        };
        let v = &views(&[s])[0];
        assert!(v.charging);
        assert_eq!(v.tooltip, "M — 70% charging");
        assert!(!views(&[percent("M", 70)])[0].charging);
    }

    #[test]
    fn coarse_level_is_a_word_and_unknown_is_a_question_mark() {
        let v = views(&[
            state(
                "K",
                DeviceKind::Keyboard,
                Some(Level::Coarse(CoarseLevel::Good)),
            ),
            state("P", DeviceKind::Controller, None),
        ]);
        assert_eq!(v[0].text, "Good");
        assert_eq!(v[0].tooltip, "K — Good");
        assert!(!v[0].low);
        assert_eq!(v[1].text, "?");
    }

    #[test]
    fn glyph_by_kind() {
        let glyphs: Vec<char> = [
            DeviceKind::Mouse,
            DeviceKind::Keyboard,
            DeviceKind::Headset,
            DeviceKind::Controller,
            DeviceKind::Other,
        ]
        .into_iter()
        .map(|kind| views(&[state("D", kind, None)])[0].glyph)
        .collect();
        assert_eq!(
            glyphs,
            ['\u{E962}', '\u{E765}', '\u{E7F6}', '\u{E7FC}', '\u{E83F}']
        );
    }
}
