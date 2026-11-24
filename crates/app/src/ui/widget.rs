//! The in-taskbar widget: a layered child window of `Shell_TrayWnd` and of every
//! `Shell_SecondaryTrayWnd` that "Show on" chooses, drawn with `UpdateLayeredWindow`.
//!
//! Positioning is event-driven. WinEvent hooks on the taskbars' threads only (not the rest of
//! Explorer) report when a taskbar or its notification area (`TrayNotifyWnd`) moves or
//! resizes, when a taskbar's direct child appears, and when a taskbar's children change
//! z-order (which can cover the widget); the UI thread then lays the widgets out again, which
//! also raises a covered widget. Auto-hide needs nothing: a child moves with its parent. A
//! secondary taskbar has no window for its clock, so its position comes from UI Automation
//! on a windowless MTA thread ([`uia_thread`]), queried again only on `TaskbarCreated`,
//! `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE`, a new secondary taskbar or one that changed width.
//!
//! The only Explorer-directed calls this module makes on the UI thread are `CreateWindowExW` /
//! `DestroyWindow` of the child, `SetWindowPos` on it and `UpdateLayeredWindow`, each
//! measured to stay bounded. Everything else here reads window-manager state locally.

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
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, IUIAutomationCondition, SetWinEventHook,
    TreeScope_Descendants, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, FindWindowExW, FindWindowW, GA_PARENT, GW_HWNDPREV,
    GWL_STYLE, GetAncestor, GetClassNameW, GetClientRect, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, HWND_TOP, IsWindow, IsWindowVisible, MA_NOACTIVATE,
    MONITORINFOF_PRIMARY, OBJID_WINDOW, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA, UpdateLayeredWindow,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CONTEXTMENU, WM_DPICHANGED_AFTERPARENT,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WS_CHILD, WS_CLIPSIBLINGS, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_VISIBLE,
};
use windows::core::{BOOL, PCWSTR, w};

use super::menu::{MonitorInfo, display_number};
use super::{
    DeviceView, WIDGET_CLASS, WM_APP_CLOCKS, WM_APP_HOVER_END, WM_APP_MENU, hinstance, post, render,
};

/// How often UI Automation looks again for a secondary taskbar's clock that is missing
/// (Explorer starting), and for how many rounds.
const CLOCK_RETRY: Duration = Duration::from_millis(500);
const CLOCK_RETRIES: u32 = 20;

/// The cursor left the window (asked for with `TrackMouseEvent`); the `windows` crate
/// defines it only with its `Win32_UI_Controls` feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Windows refused to create a widget window: tray icons, until the widget is tried again.
#[derive(Debug)]
pub struct CreateFailed;

/// The widgets on every taskbar, the WinEvent hooks and the UI Automation thread.
pub struct Widgets {
    widgets: Vec<Widget>,
    /// (secondary taskbar, its clock's left edge minus the taskbar's left edge), from UIA.
    clocks: Vec<(isize, i32)>,
    hooks: Vec<HWINEVENTHOOK>,
    /// (process, thread) of the taskbars the hooks watch.
    hooked: Vec<(u32, u32)>,
    uia: Option<Uia>,
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
    /// Width of the taskbar when its clock was last asked for (secondary taskbars).
    bar_width: i32,
    /// Each shown device's columns of the painted strip: (key, x0, x1).
    devices: Vec<(DeviceKey, i32, i32)>,
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
    requests: Sender<()>,
    results: Receiver<Vec<(isize, i32)>>,
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
            clocks: Vec::new(),
            hooks: Vec::new(),
            hooked: Vec::new(),
            uia: None,
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
        let clocks = &self.clocks;
        let bars = select(&taskbars(), &self.monitors, monitor_device, |bar| {
            anchor_x(bar, clocks, window_rect(bar)).is_some()
        });
        self.widgets.retain(|w| {
            // SAFETY: plain handle checks.
            let alive =
                unsafe { IsWindow(Some(w.hwnd)).as_bool() && IsWindow(Some(w.bar)).as_bool() };
            let keep = alive && bars.iter().any(|&(bar, _)| bar == w.bar);
            if !keep {
                destroy(w.hwnd);
            }
            keep
        });
        let mut ask_clocks = false;
        for (bar, primary) in bars {
            if self.widgets.iter().any(|w| w.bar == bar) {
                continue;
            }
            if primary && tray_notify(bar).is_none() {
                continue; // taskbar not laid out yet
            }
            self.widgets.push(create(bar, primary)?);
            ask_clocks |= !primary;
        }
        for w in &mut self.widgets {
            let bar_rect = window_rect(w.bar);
            if !w.primary && bar_rect.right - bar_rect.left != w.bar_width {
                w.bar_width = bar_rect.right - bar_rect.left;
                ask_clocks = true;
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
                let anchor = anchor_x(w.bar, &self.clocks, bar_rect);
                position(w, anchor, p);
            }
        }
        if ask_clocks {
            self.ask_clocks();
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
        self.clocks.clear();
        self.unhook(); // the taskbars run on new threads; `arrange` hooks them
        self.arrange(views, light)?;
        self.ask_clocks();
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
        self.ask_clocks();
        Ok(())
    }

    /// Takes the latest clock positions from the UI Automation thread.
    pub fn take_clocks(&mut self) {
        let Some(uia) = &self.uia else { return };
        loop {
            match uia.results.try_recv() {
                Ok(clocks) => self.clocks = clocks,
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
        for w in self.widgets.drain(..) {
            destroy(w.hwnd);
        }
    }

    fn ask_clocks(&mut self) {
        if self.widgets.iter().all(|w| w.primary) {
            return;
        }
        if self.uia.is_none() {
            self.uia = spawn_uia();
        }
        if let Some(uia) = &self.uia
            && uia.requests.send(()).is_err()
        {
            self.uia = None;
        }
    }

    /// Watches the taskbars' threads (only those) for windows that move, resize, appear or
    /// change z-order (see [`on_win_event`]). Re-hooks only when those threads changed, as
    /// after an Explorer restart.
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
/// for the events [`is_layout_trigger`] accepts: on a taskbar's thread, a taskbar or its
/// notification area moves, resizes or appears, a taskbar's direct child appears (it can
/// cover the widget), or a taskbar's children change z-order.
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

/// Whether a WinEvent asks for a layout. `window`: the event is about a whole window
/// (`OBJID_WINDOW`, `CHILDID_SELF`); `class` and `parent_class`: of the event's window and
/// of its parent.
fn is_layout_trigger(event: u32, window: bool, class: &str, parent_class: &str) -> bool {
    let taskbar = |c: &str| TASKBAR_CLASSES.contains(&c);
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => window && (taskbar(class) || class == TRAY_NOTIFY_CLASS),
        // A taskbar shown again (its tray's visibility follows it without an event of its
        // own), its notification area, or any direct child of a taskbar (it may cover us).
        EVENT_OBJECT_SHOW => {
            window && (taskbar(class) || class == TRAY_NOTIFY_CLASS || taskbar(parent_class))
        }
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
    })
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

/// Puts the widget left of `anchor` (screen x), or hides it when there is no anchor.
fn position(w: &mut Widget, anchor: Option<i32>, p: Painted) {
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
        return;
    };
    let gap = 4 * p.dpi as i32 / 96;
    let mut point = POINT {
        x: anchor,
        y: window_rect(w.bar).top,
    };
    // SAFETY: `point` outlives the call.
    let _ = unsafe { ScreenToClient(w.bar, &mut point) };
    let left = point.x - p.size.cx - gap;
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
/// its anchor arrives ([`Widgets::take_clocks`], then a layout).
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
    // SAFETY: literal class name; plain handle queries.
    unsafe {
        let tray = FindWindowExW(Some(bar), None, w!("TrayNotifyWnd"), PCWSTR::null()).ok()?;
        let r = window_rect(tray);
        (IsWindowVisible(tray).as_bool() && r.right > r.left).then_some(r)
    }
}

/// Screen x of what the widget sits left of: the notification area, or on a secondary
/// taskbar (no clock window) the clock UI Automation found.
fn anchor_x(bar: HWND, clocks: &[(isize, i32)], bar_rect: RECT) -> Option<i32> {
    if let Some(r) = tray_notify(bar) {
        return Some(r.left);
    }
    clocks
        .iter()
        .find(|(b, _)| *b == bar.0 as isize)
        .map(|&(_, offset)| bar_rect.left + offset)
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
/// windows, never on the widget's thread. Each request is answered with the clocks of all
/// secondary taskbars; while one is missing (Explorer starting), it looks again every 500 ms
/// for up to 10 s.
fn uia_thread(requests: &Receiver<()>, results: &Sender<Vec<(isize, i32)>>) {
    // SAFETY: initialises COM for this thread only; balanced below.
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        return;
    }
    serve_clocks(requests, results);
    // SAFETY: balances CoInitializeEx; every COM object was dropped in `serve_clocks`.
    unsafe { CoUninitialize() };
}

fn serve_clocks(requests: &Receiver<()>, results: &Sender<Vec<(isize, i32)>>) {
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
    while requests.recv().is_ok() {
        let mut tries = 0;
        loop {
            while requests.try_recv().is_ok() {}
            let (clocks, missing) = secondary_clocks(&uia, &all);
            if results.send(clocks).is_err() {
                return;
            }
            post(WM_APP_CLOCKS);
            if !missing || tries == CLOCK_RETRIES {
                break;
            }
            tries += 1;
            match requests.recv_timeout(CLOCK_RETRY) {
                Ok(()) => tries = 0,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// (secondary taskbar, its clock's left edge minus the taskbar's), and whether some
/// secondary taskbar had no clock. The clock is the leftmost `SystemTrayIcon` element
/// inside the taskbar.
fn secondary_clocks(
    uia: &IUIAutomation,
    all: &IUIAutomationCondition,
) -> (Vec<(isize, i32)>, bool) {
    let mut clocks = Vec::new();
    let mut missing = false;
    for (bar, _) in taskbars().into_iter().filter(|&(_, primary)| !primary) {
        let bar_rect = window_rect(bar);
        // SAFETY: COM calls on this thread's MTA objects.
        let left = unsafe {
            uia.ElementFromHandle(bar)
                .and_then(|root| root.FindAll(TreeScope_Descendants, all))
                .ok()
                .and_then(|found| {
                    (0..found.Length().unwrap_or(0))
                        .filter_map(|i| found.GetElement(i).ok())
                        .filter(|el| {
                            el.CurrentAutomationId()
                                .is_ok_and(|id| id == "SystemTrayIcon")
                        })
                        .filter_map(|el| el.CurrentBoundingRectangle().ok())
                        .filter(|r| {
                            r.right > r.left && r.left >= bar_rect.left && r.right <= bar_rect.right
                        })
                        .map(|r| r.left)
                        .min()
                })
        };
        match left {
            Some(x) => clocks.push((bar.0 as isize, x - bar_rect.left)),
            None => missing = true,
        }
    }
    (clocks, missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_HIDE;

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
    fn other_events_do_not_trigger() {
        assert!(!is_layout_trigger(
            EVENT_OBJECT_HIDE,
            true,
            "Shell_TrayWnd",
            NOT_TASKBAR
        ));
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
