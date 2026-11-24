//! The app's UI: the taskbar widget, the tray-icon fallback, their menu and the battery
//! history window.
//!
//! # Threads
//!
//! The UI thread (the main thread) owns a hidden host window, the widget windows (children
//! of Explorer's taskbars, so Explorer's taskbar thread shares this thread's input queue),
//! the tray icons in tray mode, a low-battery notification's temporary icon and the hover
//! card (a top-level popup, `card`). It only paints, positions, shows notifications and
//! handles menu and mouse input; its calls to Explorer are the ones `widget` and `tray`
//! list. The device runtime's `on_change` runs on the device thread and only posts
//! [`WM_APP_CHANGED`] here; the menu and the hover card read the snapshot, the in-memory
//! settings and history and the monitor list, and send no device traffic. Settings and the
//! last 30 days of battery history are loaded before the UI starts; changes are saved,
//! history samples (recorded from snapshot changes, never from device traffic of their
//! own, and a session break at start and as the PC sleeps) appended, and the Run value
//! written, by the store's writer thread ([`Store`]). The history window (a normal
//! top-level window) runs on its own thread, which this thread only sends data to
//! (`history_window`). UI Automation runs on its own windowless thread (`widget`), and the
//! [`watchdog`] on another.
//!
//! # Re-entrancy
//!
//! The calls that reach Explorer (`CreateWindowExW` and `DestroyWindow` of a child,
//! `SetWindowPos`, `UpdateLayeredWindow`, `Shell_NotifyIconW`) dispatch Explorer's sent
//! messages, such as `TaskbarCreated`, before they return, so the window procedures can be
//! re-entered while the UI state is borrowed. One rule covers it:
//!
//! - Messages from outside (`TaskbarCreated`, `WM_SETTINGCHANGE`, `WM_DISPLAYCHANGE`,
//!   `WM_POWERBROADCAST`, `WM_CLOSE`, WinEvent callbacks, `WM_DPICHANGED_AFTERPARENT`,
//!   clicks, the cursor moving on or leaving a widget, and tray callbacks) never touch the
//!   state where they arrive: they post one of our `WM_APP_*` messages.
//!   `WM_TIMER` only beats the watchdog or posts [`WM_APP_CHECK_WIDGET`],
//!   [`WM_APP_SELF_RETRY`], [`WM_APP_NOTIFY_DONE`] or [`WM_APP_HOVER_DUE`].
//! - Only `WM_APP_*` handlers borrow the state. One that arrives while the state is borrowed
//!   (a nested dispatch) is re-posted and handled after the current handler returns.
//! - No borrow is held across a modal loop: the menu is built from a snapshot read, then
//!   tracked with the state released; the chosen command borrows it again afterwards.

mod card;
mod chart;
mod dwrite;
pub mod fallback;
mod history_window;
mod menu;
mod notify;
pub mod render;
mod tray;
pub mod watchdog;
mod widget;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::thread::LocalKey;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use batterysight_core::history::{self, Recorder, Sample};
use batterysight_core::model::{
    Charge, CoarseLevel, DeviceKey, DeviceKind, DeviceState, Level, Presence,
};
use batterysight_core::settings::{DisplayMode, Settings, Threshold};
use batterysight_core::time::Ms;
use batterysight_win::runtime::{self, RuntimeHandle};
use batterysight_win::store::Store;
use batterysight_win::trace::ui_line;
use windows::Win32::Foundation::{ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{NIN_SELECT, NINF_KEY};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, IDC_ARROW, KillTimer, LoadCursorW, MSG, PBT_APMRESUMEAUTOMATIC,
    PBT_APMSUSPEND, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetTimer, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_ENDSESSION,
    WM_MOUSEMOVE, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};
use windows::core::{PCWSTR, w};

use card::Card;
use fallback::{TrayReason, should_retry_widget};
use menu::{MenuCommand, Model};
use notify::Notifier;
use tray::Tray;
use widget::{Hover, Widgets};

use crate::alerts::Alerts;

/// A tray icon chosen with the keyboard (not defined by the `windows` crate).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

/// What the UI shows for one device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceView {
    pub key: DeviceKey,
    pub glyph: char,
    pub text: String,
    pub dimmed: bool,
    pub charging: bool,
    /// Charged full on power: a check where a charging device has its bolt.
    pub full: bool,
    pub low: bool,
    /// Tells the device apart from the other shown devices of its kind ([`caption`]).
    pub caption: Option<String>,
    /// The hover card's text ([`card::card_text`]), one line each, for tray icons.
    pub tooltip: String,
}

impl DeviceView {
    /// Whether `self` draws like `other` (the tooltip aside).
    fn looks_like(&self, other: &DeviceView) -> bool {
        (
            &self.key,
            self.glyph,
            &self.text,
            self.dimmed,
            self.charging,
            self.full,
            self.low,
            &self.caption,
        ) == (
            &other.key,
            other.glyph,
            &other.text,
            other.dimmed,
            other.charging,
            other.full,
            other.low,
            &other.caption,
        )
    }
}

/// What the tooltips of tray icons are made from: the hover card's text
/// ([`card::card_text`]).
struct Tips<'a> {
    /// The history, for the time left.
    kept: &'a HashMap<DeviceKey, Vec<Sample>>,
    /// The monotonic time of the readings.
    now: Ms,
    /// The wall-clock time of the samples.
    unix_now: u64,
}

impl<'a> Tips<'a> {
    fn new(kept: &'a HashMap<DeviceKey, Vec<Sample>>) -> Tips<'a> {
        Tips {
            kept,
            now: tick_ms(),
            unix_now: unix_now_ms(),
        }
    }

    /// The card's lines of `s`, one per line.
    fn text(&self, s: &DeviceState) -> String {
        let samples = self.kept.get(&s.key).map_or(&[][..], Vec::as_slice);
        let left = history::time_left(samples, self.unix_now);
        card::card_text(s, left, self.now).join("\n")
    }
}

/// The devices to show: in the user's order, hidden devices left out. A disconnected
/// device (offline: the hub keeps it only through its removal grace) shows `×`. Devices of
/// a kind shown more than once get captions ([`captions_for`]). With `tips` (tray icons
/// show them) they get their tooltips; without, those stay empty.
fn views(states: &[DeviceState], settings: &Settings, tips: Option<&Tips>) -> Vec<DeviceView> {
    let shown: Vec<&DeviceState> = settings
        .ordered(states)
        .into_iter()
        .filter(|s| !settings.hidden.contains(&s.key))
        .collect();
    let captions = captions_for(&shown);
    shown
        .iter()
        .zip(captions)
        .map(|(s, caption)| {
            let tooltip = tips.map(|t| t.text(s)).unwrap_or_default();
            view(s, settings.threshold(&s.key), caption, tooltip)
        })
        .collect()
}

/// What tells each of `shown` apart from the other shown devices of its kind (`None` for
/// a device alone of its kind): the first 3 letters, in upper case, of its distinguishing
/// word ([`distinguishing_word`]). Where two of a kind would get the same, their words'
/// last 3 letters instead; where those clash too (identical names), their places (1, 2, …)
/// among that kind's clashing devices, in shown order.
fn captions_for(shown: &[&DeviceState]) -> Vec<Option<String>> {
    let words: Vec<Option<String>> = shown
        .iter()
        .map(|s| distinguishing_word(s, shown).map(str::to_uppercase))
        .collect();
    let mut captions: Vec<Option<String>> = words
        .iter()
        .map(|w| w.as_ref().map(|w| w.chars().take(3).collect()))
        .collect();
    for i in clashes(shown, &captions) {
        captions[i] = words[i].as_ref().map(|w| {
            // From the third character from the end (the whole word if shorter).
            let from = w.char_indices().rev().nth(2).map_or(0, |(at, _)| at);
            w[from..].to_owned()
        });
    }
    let clashing = clashes(shown, &captions);
    for &i in &clashing {
        let before = clashing
            .iter()
            .filter(|&&j| j < i && shown[j].kind == shown[i].kind)
            .count();
        captions[i] = Some((before + 1).to_string());
    }
    captions
}

/// The indices of the `captions` equal to another's of the same kind.
fn clashes(shown: &[&DeviceState], captions: &[Option<String>]) -> Vec<usize> {
    (0..shown.len())
        .filter(|&i| {
            captions[i].is_some()
                && (0..shown.len())
                    .any(|j| j != i && shown[j].kind == shown[i].kind && captions[j] == captions[i])
        })
        .collect()
}

/// The word of `s`'s name that tells it apart from the other `shown` devices of its kind:
/// the longest that none of their names has (the longest of all its words if they all do;
/// the first on a tie). `None` when no other shown device is of its kind.
fn distinguishing_word<'a>(s: &'a DeviceState, shown: &[&DeviceState]) -> Option<&'a str> {
    fn longest<'a>(words: impl Iterator<Item = &'a str>) -> Option<&'a str> {
        words.reduce(|best, w| {
            if w.chars().count() > best.chars().count() {
                w
            } else {
                best
            }
        })
    }
    let others: Vec<&str> = shown
        .iter()
        .filter(|o| o.kind == s.kind && o.key != s.key)
        .map(|o| o.name.as_str())
        .collect();
    if others.is_empty() {
        return None;
    }
    let shared = |word: &str| {
        others.iter().any(|name| {
            name.split_whitespace()
                .any(|w| w.eq_ignore_ascii_case(word))
        })
    };
    let words = || s.name.split_whitespace();
    longest(words().filter(|w| !shared(w))).or_else(|| longest(words()))
}

/// One device; `threshold`: its low-battery threshold, which also colours it as low. A
/// disconnected device's level and charge are stale: it shows `×`, never low or charging.
fn view(
    s: &DeviceState,
    threshold: Threshold,
    caption: Option<String>,
    tooltip: String,
) -> DeviceView {
    // Segoe Fluent Icons code points (the same in Segoe MDL2 Assets).
    let glyph = match s.kind {
        DeviceKind::Mouse => '\u{E962}',
        DeviceKind::Keyboard => '\u{E765}',
        DeviceKind::Headset => '\u{E7F6}',
        DeviceKind::Controller => '\u{E7FC}',
        DeviceKind::Other => '\u{E83F}',
    };
    let connected = s.presence != Presence::Offline;
    let low = connected
        && match (s.level, threshold) {
            (Some(Level::Percent(p)), Threshold::Percent(t)) => p <= t,
            // As for alerts: a coarse low or critical is at any threshold.
            (Some(Level::Coarse(c)), Threshold::Percent(_)) => c <= CoarseLevel::Low,
            _ => false,
        };
    let text = if connected {
        s.level.map_or_else(|| "?".to_owned(), level_text)
    } else {
        "×".to_owned()
    };
    DeviceView {
        key: s.key.clone(),
        glyph,
        text,
        dimmed: s.presence != Presence::Online,
        charging: connected && s.charge == Charge::Charging,
        full: connected && s.charge == Charge::Full,
        low,
        caption,
        tooltip,
    }
}

/// "70%", or the word of a coarse level.
fn level_text(level: Level) -> String {
    match level {
        Level::Percent(p) => format!("{p}%"),
        Level::Coarse(c) => match c {
            CoarseLevel::Critical => "Critical",
            CoarseLevel::Low => "Low",
            CoarseLevel::Good => "Good",
            CoarseLevel::Full => "Full",
        }
        .to_owned(),
    }
}

/// A device's one-line label in the menu: "Name — 70%", with " charging" while charging;
/// "Name — disconnected" for a disconnected device (its level and charge are stale).
fn label(s: &DeviceState) -> String {
    if s.presence == Presence::Offline {
        return format!("{} — disconnected", s.name);
    }
    let text = s.level.map_or_else(|| "?".to_owned(), level_text);
    let mut label = format!("{} — {text}", s.name);
    if s.charge == Charge::Charging {
        label.push_str(" charging");
    }
    label
}

const HOST_CLASS: PCWSTR = w!("BatterySightHost");
const WIDGET_CLASS: PCWSTR = w!("BatterySightWidget");
const HISTORY_CLASS: PCWSTR = w!("BatterySightHistory");
const CARD_CLASS: PCWSTR = w!("BatterySightCard");

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
/// Writing the Run value failed (posted by the store's writer thread); `wparam`: the state
/// it was to get (1 on, 0 off).
const WM_APP_AUTOSTART_FAILED: u32 = WM_APP + 6;
/// Fall back to tray icons if no widget is shown, or in tray mode try the widget again
/// (posted by [`WIDGET_CHECK_TIMER`]).
const WM_APP_CHECK_WIDGET: u32 = WM_APP + 7;
/// "Exit" was chosen, or the host was asked to close.
const WM_APP_EXIT: u32 = WM_APP + 8;
/// Open the menu (a click on the widget or a tray icon).
const WM_APP_MENU: u32 = WM_APP + 9;
/// Tray icon callbacks (`NOTIFYICON_VERSION_4`).
const WM_APP_TRAY: u32 = WM_APP + 10;
/// Try the widget again on the self-retry schedule (posted by [`SELF_RETRY_TIMER`]).
const WM_APP_SELF_RETRY: u32 = WM_APP + 11;
/// Remove the notification's temporary icon (posted by [`NOTIFY_TIMER`]).
const WM_APP_NOTIFY_DONE: u32 = WM_APP + 12;
/// The PC resumed from sleep (`WM_POWERBROADCAST`, `PBT_APMRESUMEAUTOMATIC`).
const WM_APP_RESUMED: u32 = WM_APP + 13;
/// The PC is going to sleep (`WM_POWERBROADCAST`, `PBT_APMSUSPEND`).
const WM_APP_SUSPENDING: u32 = WM_APP + 14;
/// The cursor moved on a widget (queued once however many moves, [`schedule_hover`]).
const WM_APP_HOVER: u32 = WM_APP + 15;
/// The cursor rested on a device for the tooltip delay: show its card (posted by
/// [`HOVER_TIMER`]).
const WM_APP_HOVER_DUE: u32 = WM_APP + 16;
/// The cursor left a widget (`WM_MOUSELEAVE`): hide the card.
const WM_APP_HOVER_END: u32 = WM_APP + 17;
/// The cursor moved on a tray icon: bring the tooltips' ages up to date (queued once
/// however many moves).
const WM_APP_TIPS: u32 = WM_APP + 18;

/// Beats the watchdog while widgets exist, also inside the menu's modal loop.
const HEARTBEAT_TIMER: usize = 1;
/// One-shot: [`WIDGET_CHECK_MS`] after the widgets start or `TaskbarCreated`, checks that a
/// widget is shown; in tray mode, after a retry on a taskbar or display change failed, tries
/// the widget once more ([`Ui::retry_widget`]).
const WIDGET_CHECK_TIMER: usize = 2;
const WIDGET_CHECK_MS: u32 = 5_000;
/// One-shot: in tray mode after a failure that may pass, the next self-retry of the widget
/// ([`fallback::next_self_retry`]).
const SELF_RETRY_TIMER: usize = 3;
/// One-shot: removes the notification's temporary icon ([`notify::TEMP_ICON_MS`] after the
/// last notification), through [`WM_APP_NOTIFY_DONE`].
const NOTIFY_TIMER: usize = 4;
/// One-shot: the tooltip delay ([`hover_delay`]) after the cursor came to a device on a
/// widget, shows its card, through [`WM_APP_HOVER_DUE`].
const HOVER_TIMER: usize = 5;

/// How long the cursor rests on a device before its card shows: Windows' tooltip delay,
/// read each time a hover starts, so a changed setting applies at once. A tooltip's
/// initial delay (`TTDT_INITIAL`) is by default the double-click time ("The default delay
/// times are based on the double-click time … initial = GetDoubleClickTime()", 500 ms by
/// default: <https://learn.microsoft.com/windows/win32/controls/ttm-setdelaytime>).
fn hover_delay() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetDoubleClickTime() }
}

/// Sleep of the widget thread after the first click with `--debug-stall-widget`.
const DEBUG_STALL: Duration = Duration::from_secs(10);

/// The host window, for posting from any thread.
static HOST: AtomicIsize = AtomicIsize::new(0);
/// The registered `TaskbarCreated` message.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// Whether the next left click on the widget stalls its thread (`--debug-stall-widget`).
static DEBUG_STALL_ARMED: AtomicBool = AtomicBool::new(false);
/// Whether the next widget check reports no anchor (`--debug-force-no-anchor`).
static DEBUG_NO_ANCHOR_ARMED: AtomicBool = AtomicBool::new(false);
/// The writer of the settings (and the Run value). Not in [`Ui`]: the watchdog stops it
/// too, from its own thread, before it ends the process ([`stop_store`]).
static STORE: Mutex<Option<Store>> = Mutex::new(None);

thread_local! {
    /// The UI state; lives on the UI thread only. See the module doc for how it is borrowed.
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    /// A `WM_APP_LAYOUT` is queued.
    static LAYOUT_PENDING: Cell<bool> = const { Cell::new(false) };
    /// A `WM_APP_HOVER` is queued.
    static HOVER_PENDING: Cell<bool> = const { Cell::new(false) };
    /// A `WM_APP_TIPS` is queued.
    static TIPS_PENDING: Cell<bool> = const { Cell::new(false) };
    /// The menu's modal loop runs: no card shows meanwhile.
    static MENU_OPEN: Cell<bool> = const { Cell::new(false) };
}

pub struct Options {
    /// Tray icons instead of the widget for this session, and why; `None` starts the widget.
    pub tray: Option<TrayReason>,
    /// Debug builds only: stall the widget thread for 10 s after the first click.
    pub debug_stall: bool,
    /// Debug builds only: the first widget check reports no anchor, as if no widget showed.
    pub debug_no_anchor: bool,
}

/// Runs the UI on this thread until "Exit". Returns the process exit code. `store` has
/// loaded `settings` and `samples` (the history); it is stopped before this returns.
pub fn run(options: &Options, store: Store, settings: Settings, samples: Vec<Sample>) -> i32 {
    *STORE.lock().unwrap_or_else(PoisonError::into_inner) = Some(store);
    DEBUG_STALL_ARMED.store(options.debug_stall, Ordering::Relaxed);
    DEBUG_NO_ANCHOR_ARMED.store(options.debug_no_anchor, Ordering::Relaxed);
    let Some(host) = create_host() else {
        stop_store();
        return 1;
    };
    // The runtime starts before any widget exists: its start blocks this thread, which must
    // not happen while Explorer shares the thread's input queue.
    let runtime = match runtime::start(Box::new(|| post(WM_APP_CHANGED))) {
        Ok(runtime) => runtime,
        Err(_) => {
            // SAFETY: our own window.
            let _ = unsafe { DestroyWindow(host) };
            stop_store();
            return 1;
        }
    };
    // DirectWrite too, as loading it may read from disk.
    dwrite::init();
    let light = light_theme();
    let states = read_snapshot(&runtime);
    let unix_now = unix_now_ms();
    let mut history = HashMap::new();
    history::add(&mut history, samples, unix_now);
    // Without tooltips: the widget shows none.
    let plain = views(&states, &settings, None);
    let widgets = match options.tray {
        Some(reason) => Err(reason),
        None => {
            Widgets::new(&plain, light, &settings.monitors).map_err(|_| TrayReason::CreateFailed)
        }
    };
    let (surface, tray_reason, views) = match widgets {
        Ok(widgets) => {
            start_widget_checks(host);
            (Surface::Widgets(widgets), None, plain)
        }
        Err(reason) => {
            fallback::trace(reason);
            let views = views(&states, &settings, Some(&Tips::new(&history)));
            let mut tray = Tray::new(host);
            tray.show(&views, light);
            (Surface::Tray(tray), Some(reason), views)
        }
    };
    let mut ui = Ui {
        host,
        runtime,
        settings,
        exe: std::env::current_exe().ok(),
        views,
        light,
        surface,
        tray_reason,
        self_retry: None,
        alerts: Alerts::default(),
        notifier: Notifier::new(host),
        history,
        recorder: Recorder::default(),
        card: Card::new(host),
        hover: None,
    };
    // Nothing was observed while the app was not running.
    ui.session_break();
    ui.record(&states);
    ui.schedule_self_retry();
    UI.with(|cell| *cell.borrow_mut() = Some(ui));
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
    // Already done by "Exit"; this covers a message loop that ended otherwise. The store
    // first: a window that does not close in time must not hold up the writes.
    stop_store();
    history_window::close();
    // SAFETY: our own window and timers (killing one that is not set fails harmlessly).
    unsafe {
        let _ = KillTimer(Some(host), HEARTBEAT_TIMER);
        let _ = KillTimer(Some(host), WIDGET_CHECK_TIMER);
        let _ = KillTimer(Some(host), SELF_RETRY_TIMER);
        let _ = KillTimer(Some(host), NOTIFY_TIMER);
        let _ = KillTimer(Some(host), HOVER_TIMER);
        let _ = DestroyWindow(host);
    }
    dwrite::shutdown();
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
    /// The user's options, as last changed; saved through [`STORE`] on every change.
    /// `autostart` tracks the Run value, read once at startup.
    settings: Settings,
    /// This executable, for the Run value; `None` if Windows did not say.
    exe: Option<PathBuf>,
    /// What is shown, from the last snapshot read.
    views: Vec<DeviceView>,
    /// Whether the taskbar uses the light theme.
    light: bool,
    surface: Surface,
    /// Why tray icons show the devices; `None` while the widgets do.
    tray_reason: Option<TrayReason>,
    /// The self-retry schedule since the widget fell back. Kept until a widget check
    /// passes, so a widget that falls back again right after a retry continues the schedule
    /// instead of starting it anew.
    self_retry: Option<SelfRetry>,
    /// Which devices may raise a low-battery notification; in memory only.
    alerts: Alerts,
    notifier: Notifier,
    /// Each device's samples of the last 30 days, in time order: loaded at startup, then
    /// those recorded since.
    history: HashMap<DeviceKey, Vec<Sample>>,
    recorder: Recorder,
    /// The hover card, shown in widget mode.
    card: Card,
    /// The device under the cursor on a widget, while it is there: its card shows, or is
    /// due when [`HOVER_TIMER`] fires.
    hover: Option<Hover>,
}

/// When the widget fell back, and how many self-retries have run since.
struct SelfRetry {
    since: Instant,
    done: u8,
}

impl Ui {
    fn handle(&mut self, msg: u32, wp: WPARAM) {
        match msg {
            WM_APP_CHANGED => self.changed(),
            WM_APP_LAYOUT => {
                self.show();
                self.follow_hover(None);
            }
            WM_APP_CLOCKS => {
                if let Surface::Widgets(w) = &mut self.surface {
                    w.take_clocks();
                }
                self.show();
                self.follow_hover(None);
            }
            WM_APP_TASKBAR_CREATED => {
                // The taskbar the card sat above is gone.
                self.end_hover();
                // The old Explorer took the temporary icon with it.
                self.notifier.remove();
                if self.retry_widget(true) {
                    return;
                }
                let result = match &mut self.surface {
                    Surface::Widgets(w) => w.taskbar_created(&self.views, self.light),
                    Surface::Tray(t) => {
                        t.readd(&self.views, self.light);
                        Ok(())
                    }
                };
                if result.is_err() {
                    self.fall_back_to_tray(TrayReason::CreateFailed);
                } else if matches!(self.surface, Surface::Widgets(_)) {
                    check_widget_later(self.host);
                }
            }
            WM_APP_ENVIRONMENT => {
                // Monitors, DPI or theme may have changed under the card.
                self.end_hover();
                self.light = light_theme();
                if self.retry_widget(true) {
                    return;
                }
                let result = match &mut self.surface {
                    Surface::Widgets(w) => w.environment_changed(&self.views, self.light),
                    Surface::Tray(t) => {
                        t.show(&self.views, self.light);
                        Ok(())
                    }
                };
                if result.is_err() {
                    self.fall_back_to_tray(TrayReason::CreateFailed);
                }
            }
            WM_APP_AUTOSTART_FAILED => {
                // The Run value kept its old state: so does the checkmark.
                let wanted = wp.0 != 0;
                if self.settings.autostart == wanted {
                    self.settings.autostart = !wanted;
                    self.save();
                }
            }
            WM_APP_CHECK_WIDGET => {
                if let Surface::Widgets(w) = &self.surface {
                    if take_debug_no_anchor() || w.missing() {
                        self.fall_back_to_tray(TrayReason::NoAnchor);
                    } else {
                        // The widget holds: a later fallback starts the schedule anew.
                        self.self_retry = None;
                    }
                } else {
                    self.retry_widget(false);
                }
            }
            WM_APP_SELF_RETRY if self.tray_reason.is_some_and(should_retry_widget) => {
                if let Some(s) = &mut self.self_retry {
                    s.done = s.done.saturating_add(1);
                }
                if !self.retry_widget(false) {
                    self.schedule_self_retry();
                }
            }
            WM_APP_NOTIFY_DONE => self.notifier.remove(),
            // Nothing is observed while the PC sleeps. Recorded as it goes, the break is
            // queued ahead of whatever the resume brings; recorded again on resume, it also
            // covers a reading taken between the suspend notice and the sleep (a no-op for
            // devices not read since the first).
            WM_APP_SUSPENDING | WM_APP_RESUMED => self.session_break(),
            WM_APP_HOVER => self.hover(),
            WM_APP_HOVER_DUE => self.hover_due(),
            WM_APP_HOVER_END => self.end_hover(),
            // Ages in the tooltips ("updated N min ago") move on without a new snapshot.
            WM_APP_TIPS => self.update_views(&read_snapshot(&self.runtime)),
            _ => {}
        }
    }

    /// A new snapshot: records its history samples, shows it, and notifies the devices
    /// whose fresh readings reached their low-battery threshold ([`Alerts::check`]).
    fn changed(&mut self) {
        let states = read_snapshot(&self.runtime);
        self.record(&states);
        self.update_views(&states);
        self.follow_hover(Some(&states));
        let alerts = self.alerts.check(&states, &self.settings);
        let Some(first) = alerts.first() else {
            return;
        };
        // In tray mode the device's own icon carries the notification.
        let on = match &self.surface {
            Surface::Tray(t) => t.icon_id(&first.key),
            Surface::Widgets(_) => None,
        };
        let mut view = self.views.iter().find(|v| v.key == first.key).cloned();
        // A temporary icon (widget mode) needs the tooltip the widget's views lack.
        if let Some(v) = view.as_mut().filter(|v| v.tooltip.is_empty())
            && let Some(s) = states.iter().find(|s| s.key == v.key)
        {
            v.tooltip = Tips::new(&self.history).text(s);
        }
        self.notifier.show(&alerts, on, view.as_ref(), self.light);
    }

    /// Records the samples `states` give (hidden devices too).
    fn record(&mut self, states: &[DeviceState]) {
        let now = unix_now_ms();
        let samples = self.recorder.observe(states, now);
        self.keep(samples, states, now);
    }

    /// Records a session break for every device read since its last one
    /// ([`Recorder::session_break`]): what follows was not observed from it.
    fn session_break(&mut self) {
        let now = unix_now_ms();
        let samples = self.recorder.session_break(&self.history, now);
        self.keep(samples, &read_snapshot(&self.runtime), now);
    }

    /// Appends `samples` to `history.tsv` (by the store's writer), keeps them in
    /// [`Ui::history`] and hands them, with the devices of `states`, to an open history
    /// window.
    fn keep(&mut self, samples: Vec<Sample>, states: &[DeviceState], now: u64) {
        if !samples.is_empty() {
            with_store(|store| {
                for s in &samples {
                    store.append_history(s.to_line());
                }
            });
            history::add(&mut self.history, samples.iter().cloned(), now);
        }
        if history_window::is_open() {
            let devices = history_window::devices(states, &self.settings, &self.history);
            history_window::update(devices, &samples);
        }
    }

    /// "Battery history…": opens the history window with a copy of the history.
    fn open_history(&self) {
        let states = read_snapshot(&self.runtime);
        let devices = history_window::devices(&states, &self.settings, &self.history);
        history_window::open(devices, &self.history);
    }

    /// Shows the views of `states` if they changed; the widgets only if they look
    /// different (they show no tooltips, so their views have none).
    fn update_views(&mut self, states: &[DeviceState]) {
        let views = self.views_of(states, matches!(self.surface, Surface::Tray(_)));
        if views == self.views {
            return;
        }
        let looks = views.len() != self.views.len()
            || views.iter().zip(&self.views).any(|(a, b)| !a.looks_like(b));
        self.views = views;
        match &mut self.surface {
            Surface::Widgets(_) if !looks => return,
            Surface::Widgets(w) => w.invalidate(),
            Surface::Tray(_) => {}
        }
        self.show();
    }

    /// The views of `states`; with `tips` (for tray icons), their tooltips too.
    fn views_of(&self, states: &[DeviceState], tips: bool) -> Vec<DeviceView> {
        let tips = tips.then(|| Tips::new(&self.history));
        views(states, &self.settings, tips.as_ref())
    }

    /// The cursor moved on a widget. On a device other than the one hovered so far, the
    /// card switches to it at once if one shows, else shows after the tooltip delay
    /// ([`hover_delay`]). On the same device, a shown card follows a widget that moved.
    fn hover(&mut self) {
        let Some(hover) = self.hit() else {
            self.end_hover();
            return;
        };
        if self.hover.as_ref().is_some_and(|h| h.key == hover.key) {
            if self.card.shown().is_some() && self.hover.as_ref() != Some(&hover) {
                self.hover = Some(hover);
                self.show_card(&read_snapshot(&self.runtime));
            }
            return;
        }
        self.hover = Some(hover);
        if self.card.shown().is_some() {
            self.show_card(&read_snapshot(&self.runtime));
        } else {
            // SAFETY: our own window; setting an existing timer id restarts it.
            unsafe { SetTimer(Some(self.host), HOVER_TIMER, hover_delay(), None) };
        }
    }

    /// After a new snapshot (`fresh`) or a layout: a shown card follows the device under
    /// the cursor, repainted with the new readings or moved with its widget; a hover the
    /// cursor has left ends (should its `WM_MOUSELEAVE` have been lost).
    fn follow_hover(&mut self, fresh: Option<&[DeviceState]>) {
        if self.hover.is_none() {
            return;
        }
        let Some(hover) = self.hit() else {
            self.end_hover();
            return;
        };
        let moved = self.hover.as_ref() != Some(&hover);
        if self.card.shown().is_none() || (fresh.is_none() && !moved) {
            return;
        }
        self.hover = Some(hover);
        match fresh {
            Some(states) => self.show_card(states),
            None => self.show_card(&read_snapshot(&self.runtime)),
        }
    }

    /// [`HOVER_TIMER`] fired: shows the card if the cursor is still on the device.
    fn hover_due(&mut self) {
        let Some(hover) = self.hit() else {
            self.end_hover();
            return;
        };
        if self.hover.as_ref().is_some_and(|h| h.key == hover.key) {
            self.hover = Some(hover);
            self.show_card(&read_snapshot(&self.runtime));
        }
    }

    /// The device under the cursor on a widget; none while the menu is open. Reads
    /// window-manager state only.
    fn hit(&self) -> Option<Hover> {
        let Surface::Widgets(w) = &self.surface else {
            return None;
        };
        if MENU_OPEN.get() {
            return None;
        }
        let mut at = POINT::default();
        // SAFETY: `at` outlives the call.
        unsafe { GetCursorPos(&mut at) }.ok()?;
        w.hit(at)
    }

    /// Shows (or repaints) the card of the hovered device from `states` and the history in
    /// memory; a device no longer shown ends the hover.
    fn show_card(&mut self, states: &[DeviceState]) {
        let Some(hover) = &self.hover else {
            return;
        };
        let shown = self.views.iter().any(|v| v.key == hover.key);
        let Some(state) = states.iter().find(|s| shown && s.key == hover.key) else {
            self.end_hover();
            return;
        };
        let samples = self.history.get(&hover.key).map_or(&[][..], Vec::as_slice);
        let left = history::time_left(samples, unix_now_ms());
        let lines = card::card_text(state, left, tick_ms());
        self.card.show(hover, &lines, self.light);
        if self.card.shown().is_none() {
            // Not drawn: the next move on the device tries again.
            self.hover = None;
        }
    }

    /// Hides the card and forgets the hovered device (the cursor left, the menu opens,
    /// the surface or the taskbars change, or the app exits).
    fn end_hover(&mut self) {
        // SAFETY: our own window and timer (killing one that is not set fails harmlessly).
        let _ = unsafe { KillTimer(Some(self.host), HOVER_TIMER) };
        self.hover = None;
        self.card.hide();
    }

    /// Replaces what shows the devices; dropping the old surface removes its windows or
    /// icons. A notification's temporary icon goes too: it belongs to the old surface, and
    /// so does the hover card.
    fn set_surface(&mut self, surface: Surface) {
        self.end_hover();
        self.notifier.remove();
        self.surface = surface;
    }

    /// Shows the current views and lays the widgets out.
    fn show(&mut self) {
        let result = match &mut self.surface {
            Surface::Widgets(w) => w.layout(&self.views, self.light, &self.settings.monitors),
            Surface::Tray(t) => {
                t.show(&self.views, self.light);
                Ok(())
            }
        };
        if result.is_err() {
            self.fall_back_to_tray(TrayReason::CreateFailed);
        }
    }

    /// Windows refused a widget window, or none is shown: tray icons, until a taskbar or
    /// display change or a self-retry brings the widget back ([`Ui::retry_widget`]).
    fn fall_back_to_tray(&mut self, reason: TrayReason) {
        self.tray_reason = Some(reason);
        watchdog::arm(false);
        // SAFETY: our own window and timers; nothing reads the heartbeat in tray mode.
        unsafe {
            let _ = KillTimer(Some(self.host), HEARTBEAT_TIMER);
            let _ = KillTimer(Some(self.host), WIDGET_CHECK_TIMER);
        }
        // Tray icons show tooltips, which the widget's views lack.
        self.views = self.views_of(&read_snapshot(&self.runtime), true);
        let mut tray = Tray::new(self.host);
        tray.show(&self.views, self.light);
        // Dropping the widgets destroys their windows and hooks.
        self.set_surface(Surface::Tray(tray));
        // Only now: the widgets no longer share Explorer's input queue.
        fallback::trace(reason);
        self.schedule_self_retry();
    }

    /// In tray mode after a failure that may pass ([`should_retry_widget`]), sets
    /// [`SELF_RETRY_TIMER`] for the next self-retry, counted from the fallback
    /// ([`fallback::self_retry_delay`]); after the last one, only taskbar and display
    /// changes try again.
    fn schedule_self_retry(&mut self) {
        if !self.tray_reason.is_some_and(should_retry_widget) {
            return;
        }
        let s = self.self_retry.get_or_insert_with(|| SelfRetry {
            since: Instant::now(),
            done: 0,
        });
        let elapsed = u64::try_from(s.since.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(delay) = fallback::self_retry_delay(s.done, elapsed) {
            // SAFETY: our own window; setting an existing timer id restarts it.
            unsafe { SetTimer(Some(self.host), SELF_RETRY_TIMER, delay, None) };
        }
    }

    /// In tray mode after a failure that may pass ([`should_retry_widget`]), tries the
    /// widget again and switches to it once one shows; the tray icons stay until then. A
    /// failed try keeps tray mode and its reason. With `again_later` (a taskbar or display
    /// change: Explorer may lay its taskbars out only after it), a failed try is repeated
    /// once, [`WIDGET_CHECK_MS`] later; then the next change tries again. Returns whether it
    /// switched. Taskbar and display changes leave the self-retry schedule as it is.
    fn retry_widget(&mut self, again_later: bool) -> bool {
        if !self.tray_reason.is_some_and(should_retry_widget) {
            return false;
        }
        // A widget shows at once only next to the primary taskbar's notification area;
        // without it, the widgets built (secondary windows, a UI Automation walk) would be
        // thrown away. Dropping widgets that do not show destroys their windows and hooks.
        let widgets = if widget::primary_anchor_ready() {
            Widgets::new(&self.views, self.light, &self.settings.monitors)
                .ok()
                .filter(Widgets::shown)
        } else {
            None
        };
        let Some(widgets) = widgets else {
            if again_later {
                check_widget_later(self.host);
            }
            return false;
        };
        // SAFETY: our own window and timer (killing one that is not set fails harmlessly).
        let _ = unsafe { KillTimer(Some(self.host), SELF_RETRY_TIMER) };
        self.tray_reason = None;
        // Dropping the tray removes its icons, before the watchdog is armed: a hung
        // Explorer stalls those calls, not the widget.
        self.set_surface(Surface::Widgets(widgets));
        start_widget_checks(self.host);
        true
    }

    /// "Show as: Taskbar widget": widgets again, whatever made the session use tray icons
    /// (the user's choice beats the automatic fallback). If Windows refuses them, tray
    /// icons stay as after any refusal.
    fn choose_widget(&mut self) {
        if matches!(self.surface, Surface::Widgets(_)) {
            return;
        }
        // SAFETY: our own window and timer (killing one that is not set fails harmlessly).
        let _ = unsafe { KillTimer(Some(self.host), SELF_RETRY_TIMER) };
        self.self_retry = None;
        match Widgets::new(&self.views, self.light, &self.settings.monitors) {
            Ok(widgets) => {
                self.tray_reason = None;
                // Dropping the tray removes its icons, before the watchdog is armed.
                self.set_surface(Surface::Widgets(widgets));
                start_widget_checks(self.host);
            }
            Err(_) => {
                self.tray_reason = Some(TrayReason::CreateFailed);
                fallback::trace(TrayReason::CreateFailed);
                self.schedule_self_retry();
            }
        }
    }

    /// "Show as: Tray icons": tray icons that nothing turns back into the widget
    /// ([`should_retry_widget`]) until the user chooses it again.
    fn choose_tray(&mut self) {
        match self.surface {
            Surface::Widgets(_) => self.fall_back_to_tray(TrayReason::UserChoice),
            Surface::Tray(_) => {
                self.tray_reason = Some(TrayReason::UserChoice);
                self.self_retry = None;
                // SAFETY: our own window and timers (killing one that is not set fails
                // harmlessly).
                unsafe {
                    let _ = KillTimer(Some(self.host), SELF_RETRY_TIMER);
                    let _ = KillTimer(Some(self.host), WIDGET_CHECK_TIMER);
                }
            }
        }
    }

    /// Carries out a command chosen in the menu; a changed setting is shown at once and
    /// saved off this thread.
    fn apply(&mut self, command: MenuCommand) {
        match command {
            MenuCommand::Refresh => {
                self.runtime.refresh_now();
                return;
            }
            MenuCommand::History => {
                self.open_history();
                return;
            }
            MenuCommand::Exit => {
                post(WM_APP_EXIT);
                return;
            }
            // Greyed in the menu: the Run value needs the exe path.
            MenuCommand::Autostart(_) if self.exe.is_none() => return,
            _ => {}
        }
        let shown = self.shown_keys();
        let changed = menu::change(&mut self.settings, &command, &shown);
        match command {
            // Even unchanged: chosen during an automatic fallback, the widget is wanted
            // now, and tray icons become the user's choice.
            MenuCommand::ShowAs(DisplayMode::Widget) => self.choose_widget(),
            MenuCommand::ShowAs(DisplayMode::Tray) => self.choose_tray(),
            _ if !changed => {}
            MenuCommand::ToggleShown(_) | MenuCommand::MoveLeft(_) | MenuCommand::MoveRight(_) => {
                self.update_views(&read_snapshot(&self.runtime));
            }
            MenuCommand::Threshold(key, _) => {
                // A new threshold gets its own alert, even after one at the old threshold.
                self.alerts.rearm(&key);
                self.update_views(&read_snapshot(&self.runtime));
            }
            MenuCommand::ShowOn(_) => {
                if matches!(self.surface, Surface::Widgets(_)) {
                    self.show();
                    if matches!(self.surface, Surface::Widgets(_)) {
                        check_widget_later(self.host);
                    }
                } else {
                    // A settings change, like a display change, tries a widget that
                    // failed again.
                    self.retry_widget(true);
                }
            }
            MenuCommand::Autostart(on) => {
                if let Some(exe) = self.exe.clone() {
                    with_store(|store| store.run(Box::new(move || write_autostart(&exe, on))));
                }
            }
            // Not settings; carried out above.
            MenuCommand::History | MenuCommand::Refresh | MenuCommand::Exit => {}
        }
        if changed {
            self.save();
        }
    }

    /// The keys of the devices shown, in shown order.
    fn shown_keys(&self) -> Vec<DeviceKey> {
        self.views.iter().map(|v| v.key.clone()).collect()
    }

    /// Queues writing the settings on the store's writer thread.
    fn save(&self) {
        let settings = self.settings.clone();
        with_store(|store| store.save_settings(settings));
    }

    /// Removes the hover card, the widgets or tray icons and a notification's temporary
    /// icon, finishes the queued writes (at most 1 s), closes the history window (at most
    /// 1 s), then stops the runtime (bounded by its 2 s limit) and ends the message loop.
    fn exit(self) {
        watchdog::arm(false);
        drop(self.card);
        drop(self.surface);
        drop(self.notifier);
        // First: a window that does not close in time must not hold up the writes.
        stop_store();
        history_window::close();
        self.runtime.stop();
        // SAFETY: no pointers involved.
        unsafe { PostQuitMessage(0) };
    }

    /// The menu, from the snapshot, the in-memory settings and the monitor list: no device
    /// traffic, no disk.
    fn menu_model(&self) -> Model {
        Model::new(
            &read_snapshot(&self.runtime),
            &self.settings,
            &widget::monitors(),
            self.tray_reason,
            self.exe.is_some(),
        )
    }
}

/// Writes the Run value (on the store's writer thread). A failure is traced and reported
/// to the UI thread, so the menu's checkmark keeps following the Run value.
fn write_autostart(exe: &Path, on: bool) {
    if let Err(e) = crate::autostart::set(exe, on) {
        ui_line(&format!("autostart not changed: {e}"));
        post_with(WM_APP_AUTOSTART_FAILED, usize::from(on));
    }
}

/// Runs `f` on the store, unless it is stopped. Never waits for the lock: it is held only
/// for a send like this one or to take the store out ([`stop_store`]), never while the
/// store stops. A miss means the watchdog is taking it out to end the process, so the send
/// is dropped.
fn with_store(f: impl FnOnce(&Store)) {
    let guard = match STORE.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    if let Some(store) = guard.as_ref() {
        f(store);
    }
}

/// Finishes the queued writes (waiting at most 1 s) and stops the store's writer; later
/// saves are dropped. Called on every way out: "Exit", the end of the session, a failed
/// start, and by the watchdog before it ends the process. The store is taken out under the
/// lock and stopped after it is released. Never waits for the lock: on the UI thread a miss
/// means the watchdog is taking the store out; on the watchdog's, that the UI thread is
/// sending to it at that instant, and the writer is then ended with the process
/// (`settings.toml` is replaced atomically, so at most that change is lost).
pub fn stop_store() {
    let store = match STORE.try_lock() {
        Ok(mut guard) => guard.take(),
        Err(TryLockError::Poisoned(e)) => e.into_inner().take(),
        Err(TryLockError::WouldBlock) => None,
    };
    if let Some(store) = store {
        store.stop();
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
    post_with(msg, 0);
}

/// The host window (null before it exists), from any thread.
fn host() -> HWND {
    HWND(HOST.load(Ordering::Relaxed) as *mut c_void)
}

/// Posts `msg` with `wparam` to the host window, from any thread.
fn post_with(msg: u32, wparam: usize) {
    let host = host();
    if !host.is_invalid() {
        // SAFETY: no pointers involved; a stale handle only makes the post fail.
        let _ = unsafe { PostMessageW(Some(host), msg, WPARAM(wparam), LPARAM(0)) };
    }
}

/// Queues one `WM_APP_LAYOUT`, however many events ask for it.
fn schedule_layout() {
    post_once(&LAYOUT_PENDING, WM_APP_LAYOUT);
}

/// Queues one `WM_APP_HOVER`, however many moves ask for it.
fn schedule_hover() {
    post_once(&HOVER_PENDING, WM_APP_HOVER);
}

/// Posts `msg` unless `pending` says it is queued; its handler clears `pending`.
fn post_once(pending: &'static LocalKey<Cell<bool>>, msg: u32) {
    if !pending.replace(true) {
        post(msg);
    }
}

/// Starts (or restarts) the one-shot check that a widget is shown.
fn check_widget_later(host: HWND) {
    // SAFETY: our own window; setting an existing timer id restarts it.
    unsafe { SetTimer(Some(host), WIDGET_CHECK_TIMER, WIDGET_CHECK_MS, None) };
}

/// Widgets now show the devices: arms the watchdog and its heartbeat, and checks later
/// that a widget is shown.
fn start_widget_checks(host: HWND) {
    watchdog::arm(true);
    watchdog::spawn();
    // SAFETY: our own window; setting an existing timer id restarts it.
    unsafe { SetTimer(Some(host), HEARTBEAT_TIMER, watchdog::INTERVAL_MS, None) };
    check_widget_later(host);
}

/// Whether this click should stall the widget thread (`--debug-stall-widget`, once).
fn take_debug_stall() -> bool {
    DEBUG_STALL_ARMED.swap(false, Ordering::Relaxed)
}

/// Whether this widget check reports no anchor (`--debug-force-no-anchor`, once).
fn take_debug_no_anchor() -> bool {
    DEBUG_NO_ANCHOR_ARMED.swap(false, Ordering::Relaxed)
}

fn hinstance() -> HINSTANCE {
    // SAFETY: the module handle of this exe.
    unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_or_else(|_| HINSTANCE::default(), |m| HINSTANCE(m.0))
}

/// The wall-clock time in unix milliseconds, for history samples (0 before 1970).
pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The monotonic time of the readings (`DeviceState::updated_ms`).
fn tick_ms() -> Ms {
    // SAFETY: no arguments.
    unsafe { GetTickCount64() }
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
        let history_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            // Repainted whole on resize: the chart scales with the window.
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(history_window::window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: HISTORY_CLASS,
            ..Default::default()
        };
        let card_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(card::window_proc),
            hInstance: instance,
            lpszClassName: CARD_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&host_class) == 0
            || RegisterClassExW(&widget_class) == 0
            || RegisterClassExW(&history_class) == 0
            || RegisterClassExW(&card_class) == 0
        {
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
        WM_TIMER if wp.0 == WIDGET_CHECK_TIMER => {
            // SAFETY: our own window and timer; the check runs once.
            let _ = unsafe { KillTimer(Some(hwnd), WIDGET_CHECK_TIMER) };
            post(WM_APP_CHECK_WIDGET);
        }
        WM_TIMER if wp.0 == SELF_RETRY_TIMER => {
            // SAFETY: our own window and timer; each self-retry runs once.
            let _ = unsafe { KillTimer(Some(hwnd), SELF_RETRY_TIMER) };
            post(WM_APP_SELF_RETRY);
        }
        WM_TIMER if wp.0 == NOTIFY_TIMER => {
            // SAFETY: our own window and timer; the icon is removed once.
            let _ = unsafe { KillTimer(Some(hwnd), NOTIFY_TIMER) };
            post(WM_APP_NOTIFY_DONE);
        }
        WM_TIMER if wp.0 == HOVER_TIMER => {
            // SAFETY: our own window and timer; the card is due once.
            let _ = unsafe { KillTimer(Some(hwnd), HOVER_TIMER) };
            post(WM_APP_HOVER_DUE);
        }
        WM_TIMER => watchdog::beat(),
        // Never handled where they arrive (see the module doc); WM_SETTINGCHANGE's string
        // is not needed, every change is re-read.
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE => post(WM_APP_ENVIRONMENT),
        WM_POWERBROADCAST => {
            match wp.0 as u32 {
                PBT_APMSUSPEND => post(WM_APP_SUSPENDING),
                PBT_APMRESUMEAUTOMATIC => post(WM_APP_RESUMED),
                _ => {}
            }
            return LRESULT(1);
        }
        _ if msg == taskbar_created && msg != 0 => post(WM_APP_TASKBAR_CREATED),
        WM_CLOSE => post(WM_APP_EXIT),
        WM_ENDSESSION if wp.0 != 0 => exit(),
        WM_APP_EXIT => exit(),
        WM_APP_MENU => menu(hwnd),
        WM_APP_TRAY => {
            let event = (lp.0 & 0xFFFF) as u32;
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                post(WM_APP_MENU);
            } else if event == WM_MOUSEMOVE {
                post_once(&TIPS_PENDING, WM_APP_TIPS);
            }
        }
        WM_APP_CHANGED..=WM_APP_CHECK_WIDGET
        | WM_APP_SELF_RETRY
        | WM_APP_NOTIFY_DONE
        | WM_APP_RESUMED..=WM_APP_TIPS => {
            if ui_busy() {
                // SAFETY: no pointers involved.
                let _ = unsafe { PostMessageW(Some(hwnd), msg, wp, lp) };
                return LRESULT(0);
            }
            match msg {
                WM_APP_LAYOUT => LAYOUT_PENDING.set(false),
                WM_APP_HOVER => HOVER_PENDING.set(false),
                WM_APP_TIPS => TIPS_PENDING.set(false),
                _ => {}
            }
            with_ui(|ui| ui.handle(msg, wp));
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

/// Hides the hover card, shows the menu at the cursor and carries out the command chosen.
fn menu(host: HWND) {
    if ui_busy() {
        post(WM_APP_MENU);
        return;
    }
    let Some(model) = with_ui(|ui| {
        ui.end_hover();
        ui.menu_model()
    }) else {
        return;
    };
    // The state is not borrowed while the menu's modal loop runs; no card shows meanwhile.
    MENU_OPEN.set(true);
    let command = menu::track(&model, host);
    MENU_OPEN.set(false);
    if let Some(command) = command {
        with_ui(|ui| ui.apply(command));
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
    use batterysight_core::model::{Source, Verification, Via};
    use batterysight_core::settings::Threshold;

    /// The views of `states` with `settings`, without tooltips (as for the widget).
    fn shown(states: &[DeviceState], settings: &Settings) -> Vec<DeviceView> {
        views(states, settings, None)
    }

    fn all(states: &[DeviceState]) -> Vec<DeviceView> {
        shown(states, &Settings::default())
    }

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

    fn texts(v: &[DeviceView]) -> Vec<&str> {
        v.iter().map(|v| v.text.as_str()).collect()
    }

    #[test]
    fn views_follow_order_and_hide() {
        let states = [percent("A", 10), percent("B", 20), percent("C", 30)];
        let mut settings = Settings {
            order: vec![DeviceKey("C".into()), DeviceKey("A".into())],
            ..Settings::default()
        };
        // User order first; devices it does not list follow in snapshot order.
        assert_eq!(texts(&shown(&states, &settings)), ["30%", "10%", "20%"]);
        settings.set_hidden(&DeviceKey("A".into()), true);
        let v = shown(&states, &settings);
        assert_eq!(texts(&v), ["30%", "20%"]);
        assert_eq!(v[0].key, DeviceKey("C".into()));
    }

    #[test]
    fn low_uses_device_threshold() {
        let mut settings = Settings::default();
        settings.set_threshold(&DeviceKey("A".into()), Threshold::Percent(30));
        settings.set_threshold(&DeviceKey("B".into()), Threshold::Off);
        let v = shown(&[percent("A", 25), percent("B", 5)], &settings);
        assert!(v[0].low, "25 % at a 30 % threshold");
        assert!(!v[1].low, "Off is never low");
        // A device without a threshold keeps the 15 % default.
        let v = shown(&[percent("A", 25), percent("C", 15)], &Settings::default());
        assert!(!v[0].low);
        assert!(v[1].low);
    }

    #[test]
    fn coarse_low_and_critical_are_low() {
        let coarse =
            |name: &str, c: CoarseLevel| state(name, DeviceKind::Headset, Some(Level::Coarse(c)));
        let states = [
            coarse("C", CoarseLevel::Critical),
            coarse("L", CoarseLevel::Low),
            coarse("G", CoarseLevel::Good),
            coarse("F", CoarseLevel::Full),
        ];
        assert_eq!(
            all(&states).iter().map(|v| v.low).collect::<Vec<_>>(),
            [true, true, false, false]
        );
        // Off is never low; neither is a disconnected device.
        let mut settings = Settings::default();
        settings.set_threshold(&DeviceKey("L".into()), Threshold::Off);
        let off = DeviceState {
            presence: Presence::Offline,
            ..coarse("C", CoarseLevel::Critical)
        };
        let v = shown(&[coarse("L", CoarseLevel::Low), off], &settings);
        assert!(!v[0].low && !v[1].low);
    }

    #[test]
    fn asleep_is_dimmed_and_keeps_text() {
        let s = DeviceState {
            presence: Presence::Asleep,
            ..percent("M", 40)
        };
        let v = all(&[s]);
        assert_eq!(v.len(), 1);
        assert!(v[0].dimmed);
        assert_eq!(v[0].text, "40%");
        assert!(!all(&[percent("M", 40)])[0].dimmed);
    }

    #[test]
    fn low_threshold_is_15() {
        let v = all(&[percent("A", 15), percent("B", 16), percent("C", 0)]);
        assert_eq!(
            v.iter().map(|v| v.low).collect::<Vec<_>>(),
            [true, false, true]
        );
    }

    #[test]
    fn offline_seen_device_shows_cross() {
        // The hub keeps a disconnected device (offline) only through its removal grace.
        let off = DeviceState {
            presence: Presence::Offline,
            charge: Charge::Charging,
            ..percent("Off", 5)
        };
        let states = [percent("On", 60), off];
        let mut settings = Settings {
            order: vec![DeviceKey("Off".into())],
            ..Settings::default()
        };
        let v = shown(&states, &settings);
        assert_eq!(texts(&v), ["×", "60%"], "kept, in the user's order");
        assert!(v[0].dimmed);
        // Its last level and charge are stale: no low colour, no bolt.
        assert!(!v[0].low && !v[0].charging && !v[0].full);
        settings.set_hidden(&DeviceKey("Off".into()), true);
        assert_eq!(texts(&shown(&states, &settings)), ["60%"]);
    }

    #[test]
    fn full_when_charged_full_on_power() {
        let full = DeviceState {
            charge: Charge::Full,
            ..percent("M", 100)
        };
        let v = &all(&[full])[0];
        assert!(v.full);
        assert!(!v.charging);
        assert_eq!(v.text, "100%");
        assert!(!all(&[percent("M", 100)])[0].full);
    }

    fn captions(v: &[DeviceView]) -> Vec<Option<&str>> {
        v.iter().map(|v| v.caption.as_deref()).collect()
    }

    #[test]
    fn captions_only_for_same_kind() {
        let states = [
            percent("PRO X Superlight", 50),
            state("G915", DeviceKind::Keyboard, None),
            percent("G305", 60),
        ];
        let mut settings = Settings::default();
        assert_eq!(
            captions(&shown(&states, &settings)),
            [Some("SUP"), None, Some("G30")]
        );
        // Only shown devices count: with one mouse hidden, the other needs no caption.
        settings.set_hidden(&DeviceKey("G305".into()), true);
        assert_eq!(captions(&shown(&states, &settings)), [None, None]);
        // A disconnected device is shown, so it counts.
        let off = DeviceState {
            presence: Presence::Offline,
            ..percent("G305", 60)
        };
        let v = all(&[percent("PRO X Superlight", 50), off]);
        assert_eq!(captions(&v), [Some("SUP"), Some("G30")]);
    }

    #[test]
    fn caption_from_distinguishing_word() {
        let two = |a: &str, b: &str| {
            let v = all(&[percent(a, 50), percent(b, 50)]);
            [v[0].caption.clone(), v[1].caption.clone()].map(Option::unwrap)
        };
        // The longest word the other name lacks, its first 3 letters in upper case.
        assert_eq!(two("PRO X Superlight", "G305"), ["SUP", "G30"]);
        assert_eq!(two("MX Master 3S", "MX Anywhere 3"), ["MAS", "ANY"]);
        // Shared words do not count, whatever their case; ties go to the first.
        assert_eq!(two("G502 X PLUS", "g502 lightspeed"), ["PLU", "LIG"]);
        assert_eq!(two("G502 X PLUS", "G305"), ["G50", "G30"]);
        // A name whose words are all shared falls back to its longest word.
        assert_eq!(two("PRO X Superlight", "PRO X Superlight 2"), ["SUP", "2"]);
    }

    #[test]
    fn equal_captions_fall_back_to_last_letters_then_ordinals() {
        let headset = |name: &str| state(name, DeviceKind::Headset, None);
        let v = all(&[headset("G733"), headset("G735"), percent("G305", 50)]);
        // "G73" twice: the words' last 3 letters; the lone mouse needs no caption.
        assert_eq!(captions(&v), [Some("733"), Some("735"), None]);
        // Identical names: ordinals in shown order, only for the devices that clash.
        let twin = DeviceState {
            key: DeviceKey("other".into()),
            ..percent("G305", 50)
        };
        let v = all(&[percent("G305", 50), percent("PRO X Superlight", 50), twin]);
        assert_eq!(captions(&v), [Some("1"), Some("SUP"), Some("2")]);
    }

    #[test]
    fn label_omits_verification_status() {
        let with = |verification| DeviceState {
            verification,
            ..percent("PRO X", 40)
        };
        for status in [
            Verification::HardwareTested,
            Verification::SourceVerified,
            Verification::CommunityTested,
            Verification::Generic,
        ] {
            assert_eq!(label(&with(status)), "PRO X — 40%");
        }
    }

    #[test]
    fn label_says_disconnected() {
        let off = DeviceState {
            presence: Presence::Offline,
            charge: Charge::Charging,
            ..percent("PRO X", 40)
        };
        assert_eq!(label(&off), "PRO X — disconnected");
    }

    #[test]
    fn charging_in_flag_and_label() {
        let s = DeviceState {
            charge: Charge::Charging,
            ..percent("M", 70)
        };
        assert!(all(std::slice::from_ref(&s))[0].charging);
        assert_eq!(label(&s), "M — 70% charging");
        assert!(!all(&[percent("M", 70)])[0].charging);
    }

    #[test]
    fn coarse_level_is_a_word_and_unknown_is_a_question_mark() {
        let k = state(
            "K",
            DeviceKind::Keyboard,
            Some(Level::Coarse(CoarseLevel::Good)),
        );
        let v = all(&[k.clone(), state("P", DeviceKind::Controller, None)]);
        assert_eq!(v[0].text, "Good");
        assert_eq!(label(&k), "K — Good");
        assert!(!v[0].low);
        assert_eq!(v[1].text, "?");
    }

    #[test]
    fn tooltip_is_the_card_text() {
        const HOUR: u64 = 3_600_000;
        /// 2026-11-24 00:00 UTC.
        const T: u64 = 1_795_478_400_000;
        let s = percent("M", 80);
        let sample = |at, p| Sample {
            at_unix_ms: at,
            key: s.key.clone(),
            level: Some(Level::Percent(p)),
            charge: Charge::Discharging,
            presence: Presence::Online,
            millivolts: None,
            source: Source::Query,
        };
        let mut kept = HashMap::new();
        kept.insert(
            s.key.clone(),
            vec![
                sample(T, 90),
                sample(T + HOUR, 85),
                sample(T + 2 * HOUR, 80),
            ],
        );
        let tips = Tips {
            kept: &kept,
            now: 5 * 60_000,
            unix_now: T + 2 * HOUR,
        };
        let one = std::slice::from_ref(&s);
        let v = views(one, &Settings::default(), Some(&tips));
        // 10 % in 2 h: 80 % lasts 16 h.
        assert_eq!(v[0].tooltip, "M · 80%\n5 min ago · about 16 h left");
        // The widget shows no tooltips: none is made for it.
        assert_eq!(views(one, &Settings::default(), None)[0].tooltip, "");
    }

    #[test]
    fn a_tooltip_alone_does_not_change_the_look() {
        let a = &all(&[percent("M", 80)])[0];
        let b = DeviceView {
            tooltip: "other".into(),
            ..a.clone()
        };
        assert!(a.looks_like(&b));
        let c = DeviceView {
            text: "79%".into(),
            ..a.clone()
        };
        assert!(!a.looks_like(&c));
        let full = DeviceView {
            full: true,
            ..a.clone()
        };
        assert!(!a.looks_like(&full));
        let captioned = DeviceView {
            caption: Some("SUP".into()),
            ..a.clone()
        };
        assert!(!a.looks_like(&captioned));
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
        .map(|kind| all(&[state("D", kind, None)])[0].glyph)
        .collect();
        assert_eq!(
            glyphs,
            ['\u{E962}', '\u{E765}', '\u{E7F6}', '\u{E7FC}', '\u{E83F}']
        );
    }
}
