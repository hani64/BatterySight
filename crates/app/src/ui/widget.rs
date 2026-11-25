//! The in-taskbar widget: a layered child window of `Shell_TrayWnd` and of every
//! `Shell_SecondaryTrayWnd` that "Show on" chooses, drawn with `UpdateLayeredWindow`.
//!
//! A widget sits left of its taskbar's notification area (`TrayNotifyWnd`), or of the clock
//! on a secondary taskbar without one, and steps past what Windows puts there
//! ([`step_past`]): Windows 10's weather button (News and interests) and Windows 11's
//! Widgets button, which sits next to the notification area on a left-aligned taskbar. On
//! Windows 10 the primary taskbar's task buttons (`ReBarWindow32`) reach behind the widget
//! after every layout pass of Explorer's, so they are narrowed to end where it starts, and
//! Explorer lays them out over the full width again when the widget hides or goes.
//!
//! Positioning is event-driven. WinEvent hooks on the taskbars' threads only (not the rest of
//! Explorer) report when a taskbar, its notification area, its clock window, its weather
//! button or its task buttons move or resize, when a taskbar's direct child appears or hides, and when a
//! taskbar's children change z-order (which can cover the widget); the UI thread then lays
//! the widgets out again, which also raises a covered widget. Auto-hide needs nothing: a
//! child moves with its parent. Windows 11's taskbars are XAML: a secondary one has no
//! window for its clock, and none has one for its Widgets button, so their positions come
//! from UI Automation on a windowless MTA thread ([`uia_thread`]), queried again only on
//! `TaskbarCreated`, `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE`, a new widget on such a taskbar
//! or one that changed width, and every 5 s while a Widgets button sits beside a notification
//! area (its weather text changes its width).
//!
//! The only Explorer-directed calls this module makes on the UI thread are `CreateWindowExW` /
//! `DestroyWindow` of the child, `SetWindowPos` on it, `UpdateLayeredWindow` and a posted
//! `WM_SIZE` that has a taskbar lay its task buttons out again, each measured to stay
//! bounded. The task buttons are resized on another windowless thread ([`narrower_thread`]):
//! on the UI thread, whose input queue Explorer's taskbar thread shares, even an
//! asynchronous `SetWindowPos` would wait for Explorer. Everything else here reads
//! window-manager state locally.

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use batterysight_core::model::DeviceKey;
use batterysight_core::settings::MonitorChoice;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow, ScreenToClient,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    SetWinEventHook, TreeScope_Descendants, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, FindWindowExW,
    FindWindowW, GA_PARENT, GW_HWNDPREV, GWL_STYLE, GetAncestor, GetClassNameW, GetClientRect,
    GetWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, HWND_TOP, IsWindow,
    IsWindowVisible, MA_NOACTIVATE, MONITORINFOF_PRIMARY, OBJID_WINDOW, PostMessageW,
    SIZE_RESTORED, SWP_ASYNCWINDOWPOS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA, UpdateLayeredWindow,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CONTEXTMENU, WM_DPICHANGED_AFTERPARENT,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SIZE, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_VISIBLE,
};
use windows::core::{BOOL, PCWSTR, w};

use super::menu::{MonitorInfo, display_number};
use super::{
    DeviceView, WIDGET_CLASS, WM_APP_HOVER_END, WM_APP_MENU, WM_APP_PARTS, hinstance, post, render,
};

/// How often UI Automation looks again for a secondary taskbar's clock that is missing
/// (Explorer starting), and for how many rounds.
const CLOCK_RETRY: Duration = Duration::from_millis(500);
const CLOCK_RETRIES: u32 = 20;

/// How often UI Automation reads again where a Widgets button beside a notification area is.
const WIDGETS_POLL: Duration = Duration::from_secs(5);

/// The cursor left the window (asked for with `TrackMouseEvent`); the `windows` crate
/// defines it only with its `Win32_UI_Controls` feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Windows refused to create a widget window: tray icons, until the widget is tried again.
#[derive(Debug)]
pub struct CreateFailed;

/// The widgets on every taskbar, the WinEvent hooks, the UI Automation thread and the thread
/// that narrows Windows 10's task buttons.
pub struct Widgets {
    widgets: Vec<Widget>,
    /// What UI Automation found on Windows 11 taskbars.
    parts: Vec<Parts>,
    hooks: Vec<HWINEVENTHOOK>,
    /// (process, thread) of the taskbars the hooks watch.
    hooked: Vec<(u32, u32)>,
    uia: Option<Uia>,
    /// Started the first time task buttons need narrowing.
    narrower: Option<Narrower>,
    /// Which taskbars get a widget ("Show on").
    monitors: MonitorChoice,
}

struct Widget {
    hwnd: HWND,
    bar: HWND,
    primary: bool,
    /// What the window shows; `None` repaints at the next layout.
    painted: Option<Painted>,
    /// (left in the taskbar's client area, width) as last set, while shown.
    pos: Option<(i32, i32)>,
    /// Width of the taskbar when UI Automation was last asked about it ([`uia_reads`]).
    bar_width: i32,
    /// Each shown device's columns of the painted strip: (key, x0, x1).
    devices: Vec<(DeviceKey, i32, i32)>,
    /// The taskbar's task buttons were narrowed for this widget ([`narrow_task_buttons`]).
    narrowed: bool,
}

/// What UI Automation found on a Windows 11 (XAML) taskbar, as x offsets from its left edge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Parts {
    bar: isize,
    /// The clock's left edge; looked up on secondary taskbars only, which have no window for
    /// it.
    clock: Option<i32>,
    /// The Widgets button's left and right edges.
    widgets: Option<(i32, i32)>,
}

/// The device under the cursor on a widget ([`Widgets::hit`]), in screen coordinates.
#[derive(Debug, PartialEq)]
pub struct Hover {
    pub key: DeviceKey,
    /// The device's columns of the widget.
    pub device: RECT,
    /// The taskbar the widget is on.
    pub bar: RECT,
    /// The DPI the widget is painted for.
    pub dpi: u32,
}

#[derive(Clone, Copy, PartialEq)]
struct Painted {
    dpi: u32,
    height: i32,
    light: bool,
    size: SIZE,
}

struct Uia {
    /// The taskbars to look at: (taskbar, primary).
    requests: Sender<Vec<(isize, bool)>>,
    results: Receiver<Vec<Parts>>,
}

struct Narrower {
    /// (task buttons, width, height).
    requests: Sender<(isize, i32, i32)>,
}

impl Widgets {
    /// Hooks Explorer and creates a widget on every taskbar `monitors` chooses ([`select`];
    /// none while Explorer is not running; `TaskbarCreated` brings them).
    pub fn new(
        views: &[DeviceView],
        light: bool,
        monitors: &MonitorChoice,
    ) -> Result<Widgets, CreateFailed> {
        let mut widgets = Widgets {
            widgets: Vec::new(),
            parts: Vec::new(),
            hooks: Vec::new(),
            hooked: Vec::new(),
            uia: None,
            narrower: None,
            monitors: monitors.clone(),
        };
        widgets.arrange(views, light)?;
        Ok(widgets)
    }

    /// Repaints every widget at the next [`Widgets::layout`] (the views changed).
    pub fn invalidate(&mut self) {
        for w in &mut self.widgets {
            w.painted = None;
        }
    }

    /// Lays the widgets out on the taskbars `monitors` now chooses (see [`Widgets::arrange`]).
    pub fn layout(
        &mut self,
        views: &[DeviceView],
        light: bool,
        monitors: &MonitorChoice,
    ) -> Result<(), CreateFailed> {
        if self.monitors != *monitors {
            self.monitors = monitors.clone();
        }
        self.arrange(views, light)
    }

    /// Creates widgets on new chosen taskbars, drops those whose taskbar is gone or no
    /// longer chosen, repaints those whose DPI, height, theme or views changed, and moves
    /// them next to their anchor.
    fn arrange(&mut self, views: &[DeviceView], light: bool) -> Result<(), CreateFailed> {
        self.hook_taskbars();
        let parts = &self.parts;
        let bars = select(&taskbars(), &self.monitors, monitor_device, |bar| {
            anchor_x(bar, parts, window_rect(bar)).is_some()
        });
        self.widgets.retain_mut(|w| {
            // SAFETY: plain handle checks.
            let alive =
                unsafe { IsWindow(Some(w.hwnd)).as_bool() && IsWindow(Some(w.bar)).as_bool() };
            let keep = alive && bars.iter().any(|&(bar, _)| bar == w.bar);
            if !keep {
                remove(w);
            }
            keep
        });
        let mut ask_uia = false;
        for (bar, primary) in bars {
            if self.widgets.iter().any(|w| w.bar == bar) {
                continue;
            }
            if primary && tray_notify(bar).is_none() {
                continue; // taskbar not laid out yet
            }
            self.widgets.push(create(bar, primary)?);
            ask_uia |= uia_reads(bar, primary);
        }
        for w in &mut self.widgets {
            let bar_rect = window_rect(w.bar);
            if bar_rect.right - bar_rect.left != w.bar_width && uia_reads(w.bar, w.primary) {
                w.bar_width = bar_rect.right - bar_rect.left;
                ask_uia = true;
            }
            // SAFETY: plain handle query.
            let dpi = unsafe { GetDpiForWindow(w.bar) }.max(96);
            let height = client_rect(w.bar).bottom;
            let stale = w
                .painted
                .is_none_or(|p| p.dpi != dpi || p.height != height || p.light != light);
            if stale {
                paint(w, views, dpi, height, light);
            }
            if let Some(p) = w.painted {
                let anchor = anchor_x(w.bar, &self.parts, bar_rect);
                let obstacles = obstacles(w.bar, &self.parts, bar_rect);
                position(w, anchor, &obstacles, p, &mut self.narrower);
            }
        }
        if ask_uia {
            self.ask_uia();
        }
        Ok(())
    }

    /// `TaskbarCreated`: Explorer restarted. Its old taskbars took our windows with them.
    pub fn taskbar_created(
        &mut self,
        views: &[DeviceView],
        light: bool,
    ) -> Result<(), CreateFailed> {
        self.destroy_windows();
        self.parts.clear();
        self.unhook(); // the taskbars run on new threads; `arrange` hooks them
        self.arrange(views, light)?;
        self.ask_uia();
        Ok(())
    }

    /// `WM_DISPLAYCHANGE` or `WM_SETTINGCHANGE`: monitors, DPI, taskbar size or theme may
    /// have changed.
    pub fn environment_changed(
        &mut self,
        views: &[DeviceView],
        light: bool,
    ) -> Result<(), CreateFailed> {
        self.arrange(views, light)?;
        self.ask_uia();
        Ok(())
    }

    /// Takes what the UI Automation thread last found on Windows 11 taskbars.
    pub fn take_parts(&mut self) {
        let Some(uia) = &self.uia else { return };
        loop {
            match uia.results.try_recv() {
                Ok(parts) => self.parts = parts,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.uia = None;
                    break;
                }
            }
        }
    }

    /// Whether no widget is shown although a taskbar exists; see [`needs_tray`].
    pub fn missing(&self) -> bool {
        needs_tray(!taskbars().is_empty(), self.shown())
    }

    /// Whether a widget is shown on some taskbar.
    pub fn shown(&self) -> bool {
        self.widgets.iter().any(|w| w.pos.is_some())
    }

    /// The device under `at` (screen coordinates) on a shown widget. Reads window-manager
    /// state only.
    pub fn hit(&self, at: POINT) -> Option<Hover> {
        self.widgets.iter().find_map(|w| {
            let p = w.painted.filter(|_| w.pos.is_some())?;
            let r = window_rect(w.hwnd);
            if !(r.top..r.bottom).contains(&at.y) || !(r.left..r.right).contains(&at.x) {
                return None;
            }
            let (key, x0, x1) = device_at(&w.devices, at.x - r.left)?;
            Some(Hover {
                key: key.clone(),
                device: RECT {
                    left: r.left + x0,
                    right: r.left + x1,
                    ..r
                },
                bar: window_rect(w.bar),
                dpi: p.dpi,
            })
        })
    }

    /// Destroys the windows and removes the hooks. The UIA thread ends when `self` drops.
    pub fn destroy(&mut self) {
        self.destroy_windows();
        self.unhook();
    }

    fn destroy_windows(&mut self) {
        for mut w in self.widgets.drain(..) {
            remove(&mut w);
        }
    }

    /// Has the UI Automation thread look at the taskbars with a widget that need it
    /// ([`uia_reads`]); with none, a running thread stops reading Widgets buttons again.
    fn ask_uia(&mut self) {
        let bars: Vec<(isize, bool)> = self
            .widgets
            .iter()
            .filter(|w| uia_reads(w.bar, w.primary))
            .map(|w| (w.bar.0 as isize, w.primary))
            .collect();
        if bars.is_empty() && self.uia.is_none() {
            return;
        }
        if self.uia.is_none() {
            self.uia = spawn_uia();
        }
        if let Some(uia) = &self.uia
            && uia.requests.send(bars).is_err()
        {
            self.uia = None;
        }
    }

    /// Watches the taskbars' threads (only those) for windows that move, resize, appear,
    /// hide or change z-order (see [`on_win_event`]). Re-hooks only when those threads
    /// changed, as after an Explorer restart.
    fn hook_taskbars(&mut self) {
        let mut threads: Vec<(u32, u32)> = taskbars()
            .into_iter()
            .map(|(bar, _)| {
                let mut pid = 0;
                // SAFETY: plain handle query; `pid` outlives the call.
                let tid = unsafe { GetWindowThreadProcessId(bar, Some(&mut pid)) };
                (pid, tid)
            })
            .filter(|&(pid, tid)| pid != 0 && tid != 0)
            .collect();
        threads.sort_unstable();
        threads.dedup();
        if threads == self.hooked {
            return;
        }
        self.unhook();
        for &(pid, tid) in &threads {
            for event in [
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_SHOW,
                EVENT_OBJECT_HIDE,
                EVENT_OBJECT_REORDER,
            ] {
                // SAFETY: `on_win_event` matches WINEVENTPROC; out-of-context, so it runs on
                // this thread while it retrieves messages.
                let hook = unsafe {
                    SetWinEventHook(
                        event,
                        event,
                        None,
                        Some(on_win_event),
                        pid,
                        tid,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    )
                };
                if !hook.is_invalid() {
                    self.hooks.push(hook);
                }
            }
        }
        self.hooked = threads;
    }

    fn unhook(&mut self) {
        for hook in self.hooks.drain(..) {
            // SAFETY: the hook was installed by this thread.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
        self.hooked.clear();
    }
}

impl Drop for Widgets {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Lays out again (posted, so it never runs inside whatever call this event interrupted)
/// for the events [`is_layout_trigger`] accepts: on a taskbar's thread, a taskbar, its
/// notification area, its weather button or its task buttons move or resize, a taskbar or
/// its notification area appears, a taskbar's direct child appears (it can cover the
/// widget) or hides (the widget may move back), or a taskbar's children change z-order.
unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.is_invalid() {
        return;
    }
    let window = object == OBJID_WINDOW.0 && child == CHILDID_SELF as i32;
    // SAFETY: plain handle query.
    let parent = unsafe { GetAncestor(hwnd, GA_PARENT) };
    if is_layout_trigger(event, window, &class_name(hwnd), &class_name(parent)) {
        super::schedule_layout();
    }
}

const TASKBAR_CLASSES: [&str; 2] = ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"];
const TRAY_NOTIFY_CLASS: &str = "TrayNotifyWnd";
/// Windows 10's weather button (News and interests), a direct child of the taskbar left of
/// the notification area: `ShellFeedsTaskbarViewWindow` on some builds, `DynamicContent2` on
/// others (19045, where `DynamicContent1` is the search highlights icon in the search box).
const FEEDS_CLASSES: [&str; 2] = ["ShellFeedsTaskbarViewWindow", "DynamicContent2"];
/// Windows 10's task buttons, a direct child of the primary taskbar.
const TASK_BUTTONS_CLASS: &str = "ReBarWindow32";
/// A Windows 10 secondary taskbar's clock.
const CLOCK_CLASS: &str = "ClockButton";
/// The child window that hosts a Windows 11 taskbar's XAML content.
const XAML_CLASS: &str = "Windows.UI.Composition.DesktopWindowContentBridge";

/// Whether a WinEvent asks for a layout. `window`: the event is about a whole window
/// (`OBJID_WINDOW`, `CHILDID_SELF`); `class` and `parent_class`: of the event's window and
/// of its parent.
fn is_layout_trigger(event: u32, window: bool, class: &str, parent_class: &str) -> bool {
    let taskbar = |c: &str| TASKBAR_CLASSES.contains(&c);
    // What the widget sits next to, steps past or narrows: their moves and resizes move it.
    let beside = || {
        taskbar(parent_class)
            && (FEEDS_CLASSES.contains(&class)
                || class == TASK_BUTTONS_CLASS
                || class == CLOCK_CLASS)
    };
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => {
            window && (taskbar(class) || class == TRAY_NOTIFY_CLASS || beside())
        }
        // A taskbar shown again (its tray's visibility follows it without an event of its
        // own), its notification area, or any direct child of a taskbar (it may cover us).
        EVENT_OBJECT_SHOW => {
            window && (taskbar(class) || class == TRAY_NOTIFY_CLASS || taskbar(parent_class))
        }
        // A direct child of a taskbar went, such as the weather button turned off.
        EVENT_OBJECT_HIDE => window && taskbar(parent_class),
        EVENT_OBJECT_REORDER => taskbar(class),
        _ => false,
    }
}

/// The window class of `hwnd` (a local lookup, no message); empty for no window.
fn class_name(hwnd: HWND) -> String {
    if hwnd.is_invalid() {
        return String::new();
    }
    let mut buf = [0u16; 32];
    // SAFETY: `buf` outlives the call.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0)])
}

/// Whether the session must switch to tray icons: a taskbar exists but no widget is shown on
/// any (for example, a Windows build without a visible `TrayNotifyWnd`), so nothing would
/// show the devices or offer "Exit". Without any taskbar, tray icons cannot show either. A
/// later taskbar or display change tries the widget again.
fn needs_tray(taskbar: bool, shown: bool) -> bool {
    taskbar && !shown
}

/// The device at `x` (from the widget's left edge) of `devices` (key, x0, x1).
fn device_at(devices: &[(DeviceKey, i32, i32)], x: i32) -> Option<&(DeviceKey, i32, i32)> {
    devices.iter().find(|(_, x0, x1)| (*x0..*x1).contains(&x))
}

/// The window procedure of the widget. Clicks open the menu and the cursor's moves and
/// leaving drive the hover card, all through messages posted to the host window; nothing
/// here touches the UI state.
pub unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_CONTEXTMENU => LRESULT(0),
        // Handled here: DefWindowProc would turn a right-button-up into WM_CONTEXTMENU,
        // which a child passes to its parent, the taskbar.
        WM_LBUTTONUP | WM_RBUTTONUP => {
            if msg == WM_LBUTTONUP && super::take_debug_stall() {
                thread::sleep(super::DEBUG_STALL);
                return LRESULT(0);
            }
            post(WM_APP_MENU);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // Asks for WM_MOUSELEAVE (again: each one ends the tracking).
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: `track` outlives the call; our own window.
            let _ = unsafe { TrackMouseEvent(&mut track) };
            super::schedule_hover();
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            post(WM_APP_HOVER_END);
            LRESULT(0)
        }
        WM_DPICHANGED_AFTERPARENT => {
            super::schedule_layout();
            // SAFETY: forwarding this window's own message.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        // SAFETY: forwarding this window's own message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn create(bar: HWND, primary: bool) -> Result<Widget, CreateFailed> {
    // SAFETY: the class is registered by `ui::run`; literal strings.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE,
            WIDGET_CLASS,
            w!("BatterySight"),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            Some(bar),
            None,
            Some(hinstance()),
            None,
        )
    }
    .map_err(|_| CreateFailed)?;
    Ok(Widget {
        hwnd,
        bar,
        primary,
        painted: None,
        pos: None,
        bar_width: 0,
        devices: Vec::new(),
        narrowed: false,
    })
}

/// Gives the taskbar its task buttons' full width back and destroys the widget's window.
fn remove(w: &mut Widget) {
    restore_task_buttons(w);
    destroy(w.hwnd);
}

fn destroy(hwnd: HWND) {
    // SAFETY: our own window; skipped when Explorer already took it down with its parent.
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Renders the views and hands the bitmap to the window (per-pixel alpha), and keeps each
/// device's columns for [`Widgets::hit`]. The position is set separately; a changed width
/// moves the window at the next [`position`].
fn paint(w: &mut Widget, views: &[DeviceView], dpi: u32, height: i32, light: bool) {
    let Some((painter, columns)) = render::widget_strip(views, dpi, height, light) else {
        return;
    };
    let size = painter.size();
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: every pointer outlives the call; the painter's DC holds the bitmap.
    let painted = unsafe {
        UpdateLayeredWindow(
            w.hwnd,
            None,
            None,
            Some(&size),
            Some(painter.dc()),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };
    if painted.is_ok() {
        w.painted = Some(Painted {
            dpi,
            height,
            light,
            size,
        });
        w.devices = views
            .iter()
            .zip(columns)
            .map(|(v, (x0, x1))| (v.key.clone(), x0, x1))
            .collect();
    }
}

/// Puts the widget left of `anchor` (screen x) and of the `obstacles` in the way
/// ([`step_past`]), or hides it when there is no anchor. On Windows 10's primary taskbar it
/// narrows the task buttons to end where the widget starts, even when the widget stays put:
/// Explorer widens them again on its own layout passes.
fn position(
    w: &mut Widget,
    anchor: Option<i32>,
    obstacles: &[(i32, i32)],
    p: Painted,
    narrower: &mut Option<Narrower>,
) {
    let shown = style_visible(w.hwnd);
    let Some(anchor) = anchor else {
        if shown {
            // SAFETY: our own window.
            let _ = unsafe {
                SetWindowPos(
                    w.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        w.pos = None;
        restore_task_buttons(w);
        return;
    };
    let gap = 4 * p.dpi as i32 / 96;
    let mut point = POINT {
        x: step_past(anchor, p.size.cx + gap, obstacles),
        y: window_rect(w.bar).top,
    };
    // SAFETY: `point` outlives the call.
    let _ = unsafe { ScreenToClient(w.bar, &mut point) };
    let left = point.x - p.size.cx - gap;
    if w.primary {
        narrow_task_buttons(w, left, narrower);
    }
    // Something above us, such as a new child starting below Explorer's
    // DesktopWindowContentBridge, would cover the widget.
    // SAFETY: plain handle query.
    let covered = unsafe { GetWindow(w.hwnd, GW_HWNDPREV) }.is_ok();
    if w.pos == Some((left, p.size.cx)) && shown && !covered {
        return;
    }
    w.pos = Some((left, p.size.cx));
    // SAFETY: our own window.
    let _ = unsafe {
        SetWindowPos(
            w.hwnd,
            Some(HWND_TOP),
            left,
            0,
            p.size.cx,
            p.size.cy,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
}

fn style_visible(hwnd: HWND) -> bool {
    // SAFETY: plain handle query.
    (unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32 & WS_VISIBLE.0) != 0
}

/// The right edge (screen x) for a strip `width` wide that ends at `anchor` at the latest:
/// moved left past every obstacle, a screen x span (left, right), it would overlap, until
/// none does. Each step lands on an obstacle's left edge, so it ends.
fn step_past(anchor: i32, width: i32, obstacles: &[(i32, i32)]) -> i32 {
    let mut edge = anchor;
    while let Some(left) = obstacles
        .iter()
        .filter(|&&(l, r)| l < r && l < edge && r > edge - width)
        .map(|&(l, _)| l)
        .min()
    {
        edge = left;
    }
    edge
}

/// Windows 10: narrows `w`'s taskbar's task buttons so they end at `left`, the widget's left
/// edge in the taskbar's client area; on the narrower thread, started here when first needed.
fn narrow_task_buttons(w: &mut Widget, left: i32, narrower: &mut Option<Narrower>) {
    if is_xaml(w.bar) {
        return; // Windows 11: the task buttons are XAML and do not reach the notification area
    }
    let Some(buttons) = child(w.bar, TASK_BUTTONS_CLASS) else {
        return;
    };
    let r = window_rect(buttons);
    let mut start = POINT {
        x: r.left,
        y: r.top,
    };
    // SAFETY: `start` outlives the call.
    let _ = unsafe { ScreenToClient(w.bar, &mut start) };
    let Some(width) = narrowed_width(start.x, r.right - r.left, left) else {
        return;
    };
    if narrower.is_none() {
        *narrower = spawn_narrower();
    }
    match narrower {
        Some(n)
            if n.requests
                .send((buttons.0 as isize, width, r.bottom - r.top))
                .is_ok() =>
        {
            w.narrowed = true;
        }
        _ => *narrower = None,
    }
}

/// The width that task buttons starting at client x `start`, `width` wide, get so they end
/// at `widget_left`; `None` when they end there already or start right of it.
fn narrowed_width(start: i32, width: i32, widget_left: i32) -> Option<i32> {
    (start < widget_left && start + width > widget_left).then_some(widget_left - start)
}

/// Has Explorer lay `w`'s taskbar's task buttons out again over the full width, when they
/// were narrowed for the widget that now hides or goes. A posted `WM_SIZE` with the
/// taskbar's own size, as on any resize of the taskbar; it never waits for Explorer.
fn restore_task_buttons(w: &mut Widget) {
    if !std::mem::take(&mut w.narrowed) {
        return;
    }
    let c = client_rect(w.bar);
    let size = (c.right & 0xFFFF) | ((c.bottom & 0xFFFF) << 16);
    // SAFETY: a posted message; a stale handle only makes the post fail.
    let _ = unsafe {
        PostMessageW(
            Some(w.bar),
            WM_SIZE,
            WPARAM(SIZE_RESTORED as usize),
            LPARAM(size as isize),
        )
    };
}

fn spawn_narrower() -> Option<Narrower> {
    let (requests, requested) = mpsc::channel();
    thread::Builder::new()
        .name("batterysight-taskbuttons".into())
        .spawn(move || narrower_thread(&requested))
        .ok()?;
    Some(Narrower { requests })
}

/// Resizes the task buttons as last asked. It owns no windows, so its input queue is not
/// Explorer's and `SWP_ASYNCWINDOWPOS` only posts the request to Explorer's taskbar thread.
fn narrower_thread(requests: &Receiver<(isize, i32, i32)>) {
    while let Ok(first) = requests.recv() {
        let (buttons, width, height) = requests.try_iter().last().unwrap_or(first);
        let buttons = HWND(buttons as *mut c_void);
        if class_name(buttons) != TASK_BUTTONS_CLASS {
            continue; // gone with an Explorer restart
        }
        // SAFETY: plain call on a window handle; posted, see above.
        let _ = unsafe {
            SetWindowPos(
                buttons,
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            )
        };
    }
}

/// Primary taskbar first, then every secondary taskbar.
fn taskbars() -> Vec<(HWND, bool)> {
    let mut out = Vec::new();
    // SAFETY: literal class names.
    unsafe {
        if let Ok(primary) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) {
            out.push((primary, true));
        }
        let mut after = None;
        while let Ok(bar) = FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), PCWSTR::null())
        {
            out.push((bar, false));
            after = Some(bar);
        }
    }
    out
}

/// The taskbars that get a widget for `choice`, from `bars` (taskbar, primary) in
/// [`taskbars`] order; `device` names a taskbar's monitor, `anchored` says whether a widget
/// on it can be placed now ([`anchor_x`]). Every taskbar while none of the chosen ones can:
/// the named monitor is disconnected or has no taskbar, or a secondary taskbar's clock is
/// not known yet (UI Automation answers after the widgets are made). So a widget shows at
/// once next to the primary's notification area, and the widgets narrow to the choice when
/// its anchor arrives ([`Widgets::take_parts`], then a layout).
fn select<T: Copy>(
    bars: &[(T, bool)],
    choice: &MonitorChoice,
    device: impl Fn(T) -> String,
    anchored: impl Fn(T) -> bool,
) -> Vec<(T, bool)> {
    let chosen: Vec<(T, bool)> = bars
        .iter()
        .copied()
        .filter(|&(bar, primary)| match choice {
            MonitorChoice::All => true,
            MonitorChoice::Main => primary,
            MonitorChoice::Named(name) => device(bar) == *name,
        })
        .collect();
    if chosen.iter().any(|&(bar, _)| anchored(bar)) {
        chosen
    } else {
        bars.to_vec()
    }
}

/// The device name (`szDevice`) of the monitor `hwnd` is mostly on; reads window-manager
/// state only.
fn monitor_device(hwnd: HWND) -> String {
    // SAFETY: plain handle query.
    monitor_info(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
        .map(|(device, _, _)| device)
        .unwrap_or_default()
}

/// (device name, rectangle, primary) of `monitor`.
fn monitor_info(monitor: HMONITOR) -> Option<(String, RECT, bool)> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a MONITORINFOEXW with its size set, as the call expects.
    unsafe { GetMonitorInfoW(monitor, &raw mut info.monitorInfo) }
        .as_bool()
        .then(|| {
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(32);
            (
                String::from_utf16_lossy(&info.szDevice[..len]),
                info.monitorInfo.rcMonitor,
                info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            )
        })
}

/// The monitors, in the order of their numbers ("Display N"); for the menu. Reads
/// window-manager state only.
pub fn monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn add(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
        // SAFETY: `list` is the `Vec` passed below, alive for the whole enumeration.
        let list = unsafe { &mut *(list.0 as *mut Vec<HMONITOR>) };
        list.push(monitor);
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: `add` matches MONITORENUMPROC; `handles` outlives the call.
    let _ =
        unsafe { EnumDisplayMonitors(None, None, Some(add), LPARAM(&raw mut handles as isize)) };
    let mut out: Vec<MonitorInfo> = handles
        .into_iter()
        .filter_map(monitor_info)
        .enumerate()
        .map(|(i, (device, r, primary))| {
            MonitorInfo::new(device, i, r.right - r.left, r.bottom - r.top, primary)
        })
        .collect();
    out.sort_by_key(|m| display_number(&m.device).unwrap_or(usize::MAX));
    out
}

/// Whether the primary taskbar has a visible notification area: the anchor without which no
/// new widget shows at once ([`Widgets::shown`] right after [`Widgets::new`]). Reads
/// window-manager state only.
pub fn primary_anchor_ready() -> bool {
    // SAFETY: literal class name.
    unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }
        .is_ok_and(|bar| tray_notify(bar).is_some())
}

/// The visible, non-empty notification area of `bar`.
fn tray_notify(bar: HWND) -> Option<RECT> {
    visible_child(bar, TRAY_NOTIFY_CLASS)
}

/// The first direct child of `parent` of window class `class`.
fn child(parent: HWND, class: &str) -> Option<HWND> {
    let class: Vec<u16> = class.encode_utf16().chain([0]).collect();
    // SAFETY: `class` is NUL-terminated and outlives the call.
    unsafe { FindWindowExW(Some(parent), None, PCWSTR(class.as_ptr()), PCWSTR::null()) }.ok()
}

/// The screen rectangle of `parent`'s first direct child of class `class`, when it is
/// visible and not empty.
fn visible_child(parent: HWND, class: &str) -> Option<RECT> {
    let hwnd = child(parent, class)?;
    let r = window_rect(hwnd);
    // SAFETY: plain handle query.
    (unsafe { IsWindowVisible(hwnd) }.as_bool() && r.right > r.left).then_some(r)
}

/// Whether `bar` is a Windows 11 taskbar, whose content is XAML.
fn is_xaml(bar: HWND) -> bool {
    child(bar, XAML_CLASS).is_some()
}

/// Whether UI Automation has to look at `bar` ([`uia_thread`]): a Windows 11 taskbar for its
/// Widgets button, and a secondary taskbar with neither a notification area nor a clock
/// window (Windows 11, or Explorer still starting) for its clock.
fn uia_reads(bar: HWND, primary: bool) -> bool {
    is_xaml(bar)
        || (!primary && tray_notify(bar).is_none() && visible_child(bar, CLOCK_CLASS).is_none())
}

/// Screen x of what the widget sits left of: the notification area, or on a secondary
/// taskbar without one the clock: its window on Windows 10, else the one UI Automation
/// found.
fn anchor_x(bar: HWND, parts: &[Parts], bar_rect: RECT) -> Option<i32> {
    if let Some(r) = tray_notify(bar) {
        return Some(r.left);
    }
    if !is_xaml(bar)
        && let Some(r) = visible_child(bar, CLOCK_CLASS)
    {
        return Some(r.left);
    }
    parts
        .iter()
        .find(|p| p.bar == bar.0 as isize)
        .and_then(|p| p.clock)
        .map(|offset| bar_rect.left + offset)
}

/// What the widget must not cover on `bar`, as screen x spans (left, right): Windows 10's
/// weather button, read on every layout because its text changes its width, and the
/// Widgets button UI Automation found on a Windows 11 taskbar.
fn obstacles(bar: HWND, parts: &[Parts], bar_rect: RECT) -> Vec<(i32, i32)> {
    let feeds = FEEDS_CLASSES
        .iter()
        .filter_map(|class| visible_child(bar, class))
        .map(|r| (r.left, r.right));
    let widgets = parts
        .iter()
        .filter(|p| p.bar == bar.0 as isize)
        .filter_map(|p| p.widgets)
        .map(|(l, r)| (bar_rect.left + l, bar_rect.left + r));
    feeds.chain(widgets).collect()
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: `r` outlives the call.
    let _ = unsafe { GetWindowRect(hwnd, &mut r) };
    r
}

fn client_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: `r` outlives the call.
    let _ = unsafe { GetClientRect(hwnd, &mut r) };
    r
}

fn spawn_uia() -> Option<Uia> {
    let (requests, requested) = mpsc::channel();
    let (found, results) = mpsc::channel();
    thread::Builder::new()
        .name("batterysight-uia".into())
        .spawn(move || uia_thread(&requested, &found))
        .ok()?;
    Some(Uia { requests, results })
}

/// UI Automation goes to Explorer's UI thread, so it runs here, on a thread that owns no
/// windows, never on the widget's thread. Each request names the taskbars to look at and is
/// answered with what they show ([`look_up`]). While a secondary taskbar's clock is missing
/// (Explorer starting), it looks again every 500 ms for up to 10 s; while a Widgets button
/// sits beside a notification area, it reads where that button is again every 5 s (its
/// weather text changes its width) and tells the UI thread when it moved.
fn uia_thread(requests: &Receiver<Vec<(isize, bool)>>, results: &Sender<Vec<Parts>>) {
    // SAFETY: initialises COM for this thread only; balanced below.
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        return;
    }
    serve_parts(requests, results);
    // SAFETY: balances CoInitializeEx; every COM object was dropped in `serve_parts`.
    unsafe { CoUninitialize() };
}

fn serve_parts(requests: &Receiver<Vec<(isize, bool)>>, results: &Sender<Vec<Parts>>) {
    // SAFETY: COM is initialised on this thread.
    let Ok(uia) = (unsafe {
        CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
    }) else {
        return;
    };
    // SAFETY: as above.
    let Ok(all) = (unsafe { uia.CreateTrueCondition() }) else {
        return;
    };
    let mut bars = Vec::new();
    let mut found: Vec<Found> = Vec::new();
    let mut sent: Option<Vec<Parts>> = None;
    let mut tries = 0;
    loop {
        let retry = found.iter().any(|f| f.clock_missing) && tries < CLOCK_RETRIES;
        let wait = if retry {
            Some(CLOCK_RETRY)
        } else if found.iter().any(Found::beside_tray) {
            Some(WIDGETS_POLL)
        } else {
            None
        };
        let asked = match wait {
            None => match requests.recv() {
                Ok(asked) => Some(asked),
                Err(_) => return,
            },
            Some(wait) => match requests.recv_timeout(wait) {
                Ok(asked) => Some(asked),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        let answer = asked.is_some();
        if let Some(asked) = asked {
            bars = requests.try_iter().last().unwrap_or(asked);
            tries = 0;
            found = look_up(&uia, &all, &bars);
        } else if retry {
            tries += 1;
            found = look_up(&uia, &all, &bars);
        } else {
            for f in &mut found {
                f.read_widgets_again(&uia, &all);
            }
        }
        let parts: Vec<Parts> = found.iter().map(|f| f.parts).collect();
        if answer || sent.as_ref() != Some(&parts) {
            if results.send(parts.clone()).is_err() {
                return;
            }
            sent = Some(parts);
            post(WM_APP_PARTS);
        }
    }
}

/// What UI Automation found on one taskbar, with the Widgets button to read again.
struct Found {
    parts: Parts,
    primary: bool,
    /// A secondary taskbar showed no clock (Explorer starting).
    clock_missing: bool,
    button: Option<IUIAutomationElement>,
}

impl Found {
    /// Whether the Widgets button sits beside the notification area (a left-aligned
    /// taskbar), where the widget may have to step past it.
    fn beside_tray(&self) -> bool {
        let bar = window_rect(HWND(self.parts.bar as *mut c_void));
        self.parts
            .widgets
            .is_some_and(|(left, _)| beside_tray(left, bar.right - bar.left))
    }

    /// Reads the Widgets button's place again; looks the taskbar over again when the button
    /// went (Widgets turned off, the button made anew).
    fn read_widgets_again(&mut self, uia: &IUIAutomation, all: &IUIAutomationCondition) {
        let Some(button) = &self.button else { return };
        let bar = HWND(self.parts.bar as *mut c_void);
        let bar_rect = window_rect(bar);
        // SAFETY: a COM call on this thread's MTA object.
        match unsafe { button.CurrentBoundingRectangle() } {
            Ok(r) if within(&r, &bar_rect) => {
                self.parts.widgets = Some((r.left - bar_rect.left, r.right - bar_rect.left));
            }
            _ => {
                if let Some(again) = look_up(uia, all, &[(self.parts.bar, self.primary)]).pop() {
                    *self = again;
                }
            }
        }
    }
}

/// Whether a Widgets button whose left edge is `left` (from the taskbar's left edge) sits in
/// the right half of a taskbar `bar_width` wide: beside the notification area, not at the far
/// left as on a centred taskbar.
fn beside_tray(left: i32, bar_width: i32) -> bool {
    left > bar_width / 2
}

/// Whether `r` is a non-empty rectangle within the x span of `bar`.
fn within(r: &RECT, bar: &RECT) -> bool {
    r.right > r.left && r.left >= bar.left && r.right <= bar.right
}

/// Looks each of `bars` (taskbar, primary) over: a secondary taskbar's clock, the leftmost
/// `SystemTrayIcon` element, and the `WidgetsButton` element.
fn look_up(
    uia: &IUIAutomation,
    all: &IUIAutomationCondition,
    bars: &[(isize, bool)],
) -> Vec<Found> {
    bars.iter()
        .map(|&(bar, primary)| {
            let hwnd = HWND(bar as *mut c_void);
            let bar_rect = window_rect(hwnd);
            let mut clock: Option<i32> = None;
            let mut button = None;
            // SAFETY: COM calls on this thread's MTA objects.
            unsafe {
                if let Ok(found) = uia
                    .ElementFromHandle(hwnd)
                    .and_then(|root| root.FindAll(TreeScope_Descendants, all))
                {
                    for el in
                        (0..found.Length().unwrap_or(0)).filter_map(|i| found.GetElement(i).ok())
                    {
                        let Ok(id) = el.CurrentAutomationId() else {
                            continue;
                        };
                        let wanted = (id == "SystemTrayIcon" && !primary)
                            || (id == "WidgetsButton" && button.is_none());
                        let Some(r) = wanted
                            .then(|| el.CurrentBoundingRectangle().ok())
                            .flatten()
                            .filter(|r| within(r, &bar_rect))
                        else {
                            continue;
                        };
                        if id == "WidgetsButton" {
                            button = Some((el, r));
                        } else {
                            clock = Some(clock.map_or(r.left, |c| c.min(r.left)));
                        }
                    }
                }
            }
            Found {
                parts: Parts {
                    bar,
                    clock: clock.map(|x| x - bar_rect.left),
                    widgets: button
                        .as_ref()
                        .map(|(_, r)| (r.left - bar_rect.left, r.right - bar_rect.left)),
                },
                primary,
                clock_missing: !primary && clock.is_none(),
                button: button.map(|(el, _)| el),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_DESTROY;

    const NOT_TASKBAR: &str = "#32769";

    #[test]
    fn show_of_a_taskbar_its_tray_or_its_child_triggers() {
        for (class, parent) in [
            ("Shell_TrayWnd", NOT_TASKBAR),
            ("Shell_SecondaryTrayWnd", NOT_TASKBAR),
            ("TrayNotifyWnd", "Shell_TrayWnd"),
            (
                "Windows.UI.Composition.DesktopWindowContentBridge",
                "Shell_TrayWnd",
            ),
            ("WorkerW", "Shell_SecondaryTrayWnd"),
        ] {
            assert!(
                is_layout_trigger(EVENT_OBJECT_SHOW, true, class, parent),
                "{class} in {parent}"
            );
        }
    }

    #[test]
    fn show_of_other_windows_or_parts_does_not_trigger() {
        assert!(!is_layout_trigger(
            EVENT_OBJECT_SHOW,
            true,
            "CabinetWClass",
            NOT_TASKBAR
        ));
        assert!(!is_layout_trigger(
            EVENT_OBJECT_SHOW,
            true,
            "MSTaskListWClass",
            "MSTaskSwWClass"
        ));
        assert!(!is_layout_trigger(
            EVENT_OBJECT_SHOW,
            false,
            "Shell_TrayWnd",
            NOT_TASKBAR
        ));
    }

    #[test]
    fn location_change_of_a_taskbar_or_its_tray_triggers() {
        let moved = |class, window| {
            is_layout_trigger(EVENT_OBJECT_LOCATIONCHANGE, window, class, NOT_TASKBAR)
        };
        assert!(moved("Shell_TrayWnd", true));
        assert!(moved("Shell_SecondaryTrayWnd", true));
        assert!(moved("TrayNotifyWnd", true));
        assert!(!moved("ReBarWindow32", true));
        assert!(!moved("Shell_TrayWnd", false));
    }

    #[test]
    fn reorder_of_a_taskbar_triggers_whatever_the_object() {
        assert!(is_layout_trigger(
            EVENT_OBJECT_REORDER,
            false,
            "Shell_TrayWnd",
            NOT_TASKBAR
        ));
        assert!(is_layout_trigger(
            EVENT_OBJECT_REORDER,
            false,
            "Shell_SecondaryTrayWnd",
            NOT_TASKBAR
        ));
        assert!(!is_layout_trigger(
            EVENT_OBJECT_REORDER,
            false,
            "ReBarWindow32",
            "Shell_TrayWnd"
        ));
    }

    #[test]
    fn location_change_of_the_weather_or_task_buttons_triggers() {
        for class in [
            "DynamicContent2",
            "ShellFeedsTaskbarViewWindow",
            "ReBarWindow32",
        ] {
            assert!(
                is_layout_trigger(EVENT_OBJECT_LOCATIONCHANGE, true, class, "Shell_TrayWnd"),
                "{class}"
            );
            assert!(is_layout_trigger(
                EVENT_OBJECT_LOCATIONCHANGE,
                true,
                class,
                "Shell_SecondaryTrayWnd"
            ));
            // Only as a taskbar's direct child, and only for the whole window.
            assert!(!is_layout_trigger(
                EVENT_OBJECT_LOCATIONCHANGE,
                true,
                class,
                NOT_TASKBAR
            ));
            assert!(!is_layout_trigger(
                EVENT_OBJECT_LOCATIONCHANGE,
                false,
                class,
                "Shell_TrayWnd"
            ));
        }
        // A Windows 10 secondary taskbar's clock widens with its text (9:59 to 10:00).
        assert!(is_layout_trigger(
            EVENT_OBJECT_LOCATIONCHANGE,
            true,
            "ClockButton",
            "Shell_SecondaryTrayWnd"
        ));
        // Windows 10 19045's search highlights icon sits inside the search box.
        assert!(!is_layout_trigger(
            EVENT_OBJECT_LOCATIONCHANGE,
            true,
            "DynamicContent1",
            "TrayDummySearchControl"
        ));
    }

    #[test]
    fn hide_of_a_taskbar_child_triggers() {
        // The weather turned off, say: the widget moves back next to the notification area.
        assert!(is_layout_trigger(
            EVENT_OBJECT_HIDE,
            true,
            "DynamicContent2",
            "Shell_TrayWnd"
        ));
        assert!(!is_layout_trigger(
            EVENT_OBJECT_HIDE,
            false,
            "DynamicContent2",
            "Shell_TrayWnd"
        ));
    }

    #[test]
    fn other_events_do_not_trigger() {
        assert!(!is_layout_trigger(
            EVENT_OBJECT_HIDE,
            true,
            "Shell_TrayWnd",
            NOT_TASKBAR
        ));
        assert!(!is_layout_trigger(
            EVENT_OBJECT_DESTROY,
            true,
            "DynamicContent2",
            "Shell_TrayWnd"
        ));
    }

    // Windows 10 19045 at 96 DPI: the notification area starts at x 2287, the weather button
    // spans 2101..2279, and the widget is 102 px wide plus its 4 px gap.
    const TRAY: i32 = 2287;
    const STRIP: i32 = 102 + 4;
    const WEATHER: (i32, i32) = (2101, 2279);

    #[test]
    fn widget_steps_past_the_weather_button() {
        assert_eq!(step_past(TRAY, STRIP, &[WEATHER]), 2101);
    }

    #[test]
    fn widget_stays_next_to_the_tray_without_obstacles() {
        assert_eq!(step_past(TRAY, STRIP, &[]), TRAY);
        // Far left of where the widget goes (a centred Windows 11 Widgets button), or right
        // of the anchor.
        assert_eq!(step_past(TRAY, STRIP, &[(0, 180)]), TRAY);
        assert_eq!(step_past(TRAY, STRIP, &[(TRAY, 2560)]), TRAY);
        // An empty rectangle is no obstacle.
        assert_eq!(step_past(TRAY, STRIP, &[(2200, 2200)]), TRAY);
    }

    #[test]
    fn widget_steps_past_an_obstacle_reaching_into_its_strip() {
        // Ends 1 px into the strip (which starts at 2287 - 106 = 2181).
        assert_eq!(step_past(TRAY, STRIP, &[(2000, 2182)]), 2000);
        // Ends right where the strip starts: nothing to step past.
        assert_eq!(step_past(TRAY, STRIP, &[(2000, 2181)]), TRAY);
    }

    #[test]
    fn widget_steps_past_obstacles_side_by_side() {
        // A second obstacle that only gets in the way once the widget stepped past the first.
        let near = (2120, 2279);
        let next = (1990, 2060);
        assert_eq!(step_past(TRAY, STRIP, &[next, near]), 1990);
        // One the widget fits beside after the first step stays where it is.
        let clear = (1900, 2014);
        assert_eq!(step_past(TRAY, STRIP, &[clear, near]), 2120);
    }

    #[test]
    fn task_buttons_end_where_the_widget_starts() {
        // Client x: the task buttons start at 382 and end at 2079; the widget starts at 1995.
        assert_eq!(narrowed_width(382, 2079 - 382, 1995), Some(1995 - 382));
        // Ending before the widget, or right at it: left alone.
        assert_eq!(narrowed_width(382, 1500, 1995), None);
        assert_eq!(narrowed_width(382, 1995 - 382, 1995), None);
        // Starting at or right of the widget (a taskbar too narrow for both): left alone.
        assert_eq!(narrowed_width(1995, 300, 1995), None);
        assert_eq!(narrowed_width(2000, 300, 1995), None);
    }

    #[test]
    fn widgets_button_beside_the_tray_is_read_again() {
        // Left-aligned taskbar 2560 px wide: the button sits next to the notification area.
        assert!(beside_tray(2101, 2560));
        // Centred: it sits at the far left.
        assert!(!beside_tray(12, 2560));
        assert!(!beside_tray(1280, 2560));
    }

    #[test]
    fn no_widget_shown_on_a_taskbar_falls_back_to_tray() {
        assert!(needs_tray(true, false));
        assert!(!needs_tray(true, true));
        // No taskbar at all (Explorer not running): tray icons cannot show either, and
        // `TaskbarCreated` starts the check again.
        assert!(!needs_tray(false, false));
    }

    #[test]
    fn hit_maps_x_to_device() {
        let key = |k: &str| DeviceKey(k.into());
        let devices = [(key("A"), 0, 57), (key("B"), 57, 120)];
        let at = |x| device_at(&devices, x).map(|d| d.0.clone());
        assert_eq!(at(0), Some(key("A")));
        assert_eq!(at(56), Some(key("A")));
        assert_eq!(at(57), Some(key("B")));
        assert_eq!(at(119), Some(key("B")));
        assert_eq!(at(120), None);
        assert_eq!(at(-1), None);
        assert_eq!(device_at(&[], 5), None);
    }

    #[test]
    fn monitor_choice_selects_taskbars() {
        let bars = [(1, true), (2, false), (3, false)];
        let device = |bar: i32| format!(r"\\.\DISPLAY{bar}");
        let pick = |choice| select(&bars, &choice, device, |_| true);
        assert_eq!(pick(MonitorChoice::All), bars);
        assert_eq!(pick(MonitorChoice::Main), [(1, true)]);
        assert_eq!(
            pick(MonitorChoice::Named(r"\\.\DISPLAY3".into())),
            [(3, false)]
        );
        // Disconnected, or without a taskbar: every taskbar rather than none.
        assert_eq!(pick(MonitorChoice::Named(r"\\.\DISPLAY9".into())), bars);
        // No primary taskbar (Explorer starting): the secondary ones.
        assert_eq!(
            select(&bars[1..], &MonitorChoice::Main, device, |_| true),
            bars[1..]
        );
    }

    #[test]
    fn unanchored_choice_shows_on_every_taskbar_until_anchored() {
        let bars = [(1, true), (2, false), (3, false)];
        let device = |bar: i32| format!(r"\\.\DISPLAY{bar}");
        let display3 = MonitorChoice::Named(r"\\.\DISPLAY3".into());
        // Display 3's clock is not known yet (UI Automation answers later): every
        // taskbar, so the primary's widget shows at once.
        assert_eq!(select(&bars, &display3, device, |bar| bar == 1), bars);
        // Known: Display 3 only.
        assert_eq!(
            select(&bars, &display3, device, |bar| bar != 2),
            [(3, false)]
        );
        // The primary's notification area is not laid out yet: every taskbar.
        assert_eq!(
            select(&bars, &MonitorChoice::Main, device, |bar| bar != 1),
            bars
        );
    }
}
