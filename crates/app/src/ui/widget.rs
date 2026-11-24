//! The in-taskbar widget: a layered child window of `Shell_TrayWnd` and of every
//! `Shell_SecondaryTrayWnd`, drawn with `UpdateLayeredWindow` (technique and measurements:
//! docs/spike/taskbar-results.md).
//!
//! Positioning is event-driven. A WinEvent hook on Explorer's process reports when a taskbar
//! or its notification area (`TrayNotifyWnd`) moves, resizes or appears, and the UI thread
//! lays the widgets out again. Auto-hide needs nothing: a child moves with its parent. A
//! secondary taskbar has no window for its clock, so its position comes from UI Automation
//! on a windowless MTA thread ([`uia_thread`]), queried again only on `TaskbarCreated`,
//! `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE`, a new secondary taskbar or one that changed width.
//!
//! Explorer-directed calls made on the UI thread are only those the spike measured:
//! `CreateWindowExW` / `DestroyWindow` of the child, `SetWindowPos` on it and
//! `UpdateLayeredWindow`. Everything else here reads window-manager state locally.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, ScreenToClient};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, IUIAutomationCondition, SetWinEventHook,
    TreeScope_Descendants, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_SHOW, FindWindowExW, FindWindowW, GW_HWNDPREV, GWL_STYLE, GetClassNameW,
    GetClientRect, GetWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, HWND_TOP,
    IsWindow, IsWindowVisible, MA_NOACTIVATE, OBJID_WINDOW, SWP_HIDEWINDOW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA,
    UpdateLayeredWindow, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CONTEXTMENU,
    WM_DPICHANGED_AFTERPARENT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use super::{DeviceView, WIDGET_CLASS, WM_APP_CLOCKS, WM_APP_MENU, hinstance, post, render};

/// How often UI Automation looks again for a secondary taskbar's clock that is missing
/// (Explorer starting), and for how many rounds.
const CLOCK_RETRY: Duration = Duration::from_millis(500);
const CLOCK_RETRIES: u32 = 20;

/// Windows refused to create the widget window: this Windows build gets tray icons.
#[derive(Debug)]
pub struct CreateFailed;

/// The widgets on every taskbar, the WinEvent hooks and the UI Automation thread.
pub struct Widgets {
    widgets: Vec<Widget>,
    /// (secondary taskbar, its clock's left edge minus the taskbar's left edge), from UIA.
    clocks: Vec<(isize, i32)>,
    hooks: Vec<HWINEVENTHOOK>,
    uia: Option<Uia>,
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
    /// Hooks Explorer and creates a widget on every taskbar there is (none while Explorer is
    /// not running; `TaskbarCreated` brings them).
    pub fn new(views: &[DeviceView], light: bool) -> Result<Widgets, CreateFailed> {
        let mut widgets = Widgets {
            widgets: Vec::new(),
            clocks: Vec::new(),
            hooks: Vec::new(),
            uia: None,
        };
        widgets.hook_explorer();
        widgets.layout(views, light)?;
        Ok(widgets)
    }

    /// Repaints every widget at the next [`Widgets::layout`] (the views changed).
    pub fn invalidate(&mut self) {
        for w in &mut self.widgets {
            w.painted = None;
        }
    }

    /// Creates widgets on new taskbars, drops those whose taskbar is gone, repaints those
    /// whose DPI, height, theme or views changed, and moves them next to their anchor.
    pub fn layout(&mut self, views: &[DeviceView], light: bool) -> Result<(), CreateFailed> {
        self.widgets.retain(|w| {
            // SAFETY: plain handle checks.
            let alive =
                unsafe { IsWindow(Some(w.hwnd)).as_bool() && IsWindow(Some(w.bar)).as_bool() };
            if !alive {
                destroy(w.hwnd);
            }
            alive
        });
        let mut ask_clocks = false;
        for (bar, primary) in taskbars() {
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
        self.hook_explorer();
        self.layout(views, light)?;
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
        self.layout(views, light)?;
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
        if taskbars().iter().all(|&(_, primary)| primary) {
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

    /// Watches Explorer's windows for moves, resizes and showing (see [`on_win_event`]).
    fn hook_explorer(&mut self) {
        self.unhook();
        // SAFETY: literal class name; plain handle query.
        let pid = unsafe {
            let Ok(bar) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) else {
                return;
            };
            let mut pid = 0;
            GetWindowThreadProcessId(bar, Some(&mut pid));
            pid
        };
        if pid == 0 {
            return;
        }
        for event in [EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW] {
            // SAFETY: `on_win_event` matches WINEVENTPROC; out-of-context, so it runs on this
            // thread while it retrieves messages.
            let hook = unsafe {
                SetWinEventHook(
                    event,
                    event,
                    None,
                    Some(on_win_event),
                    pid,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if !hook.is_invalid() {
                self.hooks.push(hook);
            }
        }
    }

    fn unhook(&mut self) {
        for hook in self.hooks.drain(..) {
            // SAFETY: the hook was installed by this thread.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
    }
}

impl Drop for Widgets {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// A taskbar or its notification area moved, resized or appeared: lay out again (posted, so
/// it never runs inside whatever call this event interrupted).
unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW.0 || child != CHILDID_SELF as i32 || hwnd.is_invalid() {
        return;
    }
    let mut buf = [0u16; 32];
    // SAFETY: `buf` outlives the call.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    let class = &buf[..usize::try_from(n).unwrap_or(0)];
    let watched = ["Shell_TrayWnd", "Shell_SecondaryTrayWnd", "TrayNotifyWnd"];
    if watched
        .iter()
        .any(|name| name.encode_utf16().eq(class.iter().copied()))
    {
        super::schedule_layout();
    }
}

/// The window procedure of the widget. Clicks open the menu on the host window; nothing
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

/// Renders the views and hands the bitmap to the window (per-pixel alpha). The position is
/// set separately; a changed width moves the window at the next [`position`].
fn paint(w: &mut Widget, views: &[DeviceView], dpi: u32, height: i32, light: bool) {
    let Some(painter) = render::widget_strip(views, dpi, height, light) else {
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
