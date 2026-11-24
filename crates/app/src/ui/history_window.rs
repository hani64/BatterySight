//! The battery history window: a normal top-level window (not a taskbar child) with the
//! devices in a list, 24 h / 7 d / 30 d buttons, the chart of the device chosen and its
//! time-left estimate. One at a time: opening it again brings it to the front; it closes
//! with the app.
//!
//! It runs on its own thread with its own message loop, so nothing it does — painting,
//! or a modal loop such as holding a caption button, which dispatches no timers — can hold
//! up the UI thread, whose widgets share Explorer's input queue and beat the watchdog. The
//! UI thread only sends it data ([`open`], [`update`]): a copy of the history when it
//! opens, then the samples recorded since, through a channel and a posted message. It
//! never waits on the window's thread, except for a bounded wait at exit ([`close`]). The
//! window's procedure works on its own copy only: it never touches the UI state and never
//! reads the disk. Its state is borrowed only for short stretches that call nothing that
//! dispatches messages; calls that do (creating, showing, destroying, `SetWindowPos`,
//! filling the list) run with it released.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use batterysight_core::history::{self, Sample};
use batterysight_core::model::{DeviceKey, DeviceState};
use batterysight_core::settings::Settings;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateCompatibleBitmap, CreateCompatibleDC,
    CreateFontIndirectW, DeleteDC, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, GetSysColor,
    GetSysColorBrush, HDC, HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    TextOutW,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_SETCHECK, BN_CLICKED, BS_AUTORADIOBUTTON, BS_PUSHLIKE, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos, GetDlgItem,
    GetMessageW, HMENU, IDCANCEL, IsDialogMessageW, IsIconic, LB_ADDSTRING, LB_GETCURSEL,
    LB_RESETCONTENT, LB_SETCURSEL, LBN_SELCHANGE, LBS_NOINTEGRALHEIGHT, LBS_NOTIFY, MINMAXINFO,
    MSG, NONCLIENTMETRICSW, PostMessageW, PostQuitMessage, SPI_GETNONCLIENTMETRICS, SW_RESTORE,
    SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetWindowPos,
    ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_PAINT, WM_SETFONT, WM_SIZE, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_GROUP, WS_OVERLAPPEDWINDOW,
    WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, w};

use super::{HISTORY_CLASS, chart, hinstance, unix_now_ms};

const HOUR: Duration = Duration::from_secs(3600);
/// The time windows offered, left to right; the first is shown at first.
const RANGES: [(PCWSTR, Duration); 3] = [
    (w!("24 h"), HOUR.saturating_mul(24)),
    (w!("7 d"), HOUR.saturating_mul(24 * 7)),
    (w!("30 d"), HOUR.saturating_mul(24 * 30)),
];
const ID_LIST: i32 = 100;
/// The first range button; the others follow.
const ID_RANGE: i32 = 101;
/// `BST_CHECKED` (defined by the `windows` crate's Controls feature only).
const BST_CHECKED: usize = 1;
/// Updates from the UI thread are queued (posted to the window by [`WindowThread::send`]).
const WM_APP_UPDATES: u32 = WM_APP + 1;
/// How long [`close`] waits for the window's thread to end.
const CLOSE_WAIT: Duration = Duration::from_secs(1);

const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_CLIPCHILDREN.0);
/// A control parent: the keyboard moves between the list and the buttons.
const EX_STYLE: WINDOW_EX_STYLE = WS_EX_CONTROLPARENT;
/// Client size at first, and the smallest window size, at 96 DPI.
const START_SIZE: (i32, i32) = (760, 420);
const MIN_SIZE: (i32, i32) = (560, 320);

thread_local! {
    /// The window's thread, as the UI thread knows it; used on the UI thread only.
    static THREAD: RefCell<Option<WindowThread>> = const { RefCell::new(None) };
    /// The window's state, on the window's own thread.
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

/// A device in the window's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub key: DeviceKey,
    pub name: String,
}

/// The devices to list: the snapshot's in the user's order, hidden and offline ones
/// included, then devices with history that are not in the snapshot, by key (their names
/// are not kept).
pub fn devices(
    states: &[DeviceState],
    settings: &Settings,
    history: &HashMap<DeviceKey, Vec<Sample>>,
) -> Vec<Device> {
    let mut list: Vec<Device> = settings
        .ordered(states)
        .into_iter()
        .map(|s| Device {
            key: s.key.clone(),
            name: s.name.clone(),
        })
        .collect();
    let mut rest: Vec<&DeviceKey> = history
        .keys()
        .filter(|key| !list.iter().any(|d| d.key == **key))
        .collect();
    rest.sort();
    list.extend(rest.into_iter().map(|key| Device {
        key: key.clone(),
        name: key.0.clone(),
    }));
    list
}

/// What the UI thread sends the window.
enum Update {
    /// Come to the front, with these devices.
    Show(Vec<Device>),
    /// The devices now, and the samples recorded since the last update.
    Data {
        devices: Vec<Device>,
        samples: Vec<Sample>,
    },
    /// The app is ending.
    Close,
}

/// The window's thread, from the UI thread.
struct WindowThread {
    updates: Sender<Update>,
    /// The window, once created (0 before and after), for posting [`WM_APP_UPDATES`].
    hwnd: Arc<AtomicIsize>,
    /// Closed when the thread ends (the window was closed, or never created).
    done: Receiver<()>,
    thread: JoinHandle<()>,
}

impl WindowThread {
    /// Starts the thread, which creates and shows the window with `devices` and `history`.
    fn start(devices: Vec<Device>, history: HashMap<DeviceKey, Vec<Sample>>) -> Option<Self> {
        let (updates, queue) = mpsc::channel();
        let (done_tx, done) = mpsc::channel::<()>();
        let hwnd = Arc::new(AtomicIsize::new(0));
        let slot = Arc::clone(&hwnd);
        let thread = thread::Builder::new()
            .name("batterysight-history".into())
            .spawn(move || {
                run(devices, history, queue, &slot);
                drop(done_tx);
            })
            .ok()?;
        Some(WindowThread {
            updates,
            hwnd,
            done,
            thread,
        })
    }

    fn alive(&self) -> bool {
        matches!(self.done.try_recv(), Err(TryRecvError::Empty))
    }

    /// Queues `update` and posts [`WM_APP_UPDATES`] to the window. Before the window
    /// exists nothing is posted: the thread takes what is queued once it has made it.
    fn send(&self, update: Update) {
        if self.updates.send(update).is_err() {
            return;
        }
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            // SAFETY: no pointers involved; a window just destroyed only makes it fail.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_APP_UPDATES,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
    }
}

/// Runs `f` on the window's thread as the UI thread knows it; `None` when it is borrowed.
fn with_thread<R>(f: impl FnOnce(&mut Option<WindowThread>) -> R) -> Option<R> {
    THREAD.with(|cell| cell.try_borrow_mut().ok().map(|mut t| f(&mut t)))
}

/// Opens the window with `devices` and a copy of `history`, or brings the open one to the
/// front with `devices` (it has the history already). From the UI thread.
pub fn open(devices: Vec<Device>, history: &HashMap<DeviceKey, Vec<Sample>>) {
    with_thread(|thread| match thread.as_ref().filter(|t| t.alive()) {
        Some(t) => t.send(Update::Show(devices)),
        // A thread that ended is dropped, so detached: it is finishing on its own.
        None => *thread = WindowThread::start(devices, history.clone()),
    });
}

/// Whether the window is open (or opening). From the UI thread.
pub fn is_open() -> bool {
    with_thread(|thread| thread.as_ref().is_some_and(WindowThread::alive)).unwrap_or(false)
}

/// Gives the open window the current `devices` and the samples just recorded. From the UI
/// thread.
pub fn update(devices: Vec<Device>, samples: &[Sample]) {
    with_thread(|thread| {
        if let Some(t) = thread.as_ref().filter(|t| t.alive()) {
            t.send(Update::Data {
                devices,
                samples: samples.to_vec(),
            });
        }
    });
}

/// Closes the window, if open, and waits at most 1 s for its thread to end (past that it
/// ends with the process). From the UI thread, at exit.
pub fn close() {
    let Some(Some(t)) = with_thread(Option::take) else {
        return;
    };
    t.send(Update::Close);
    // The thread closes `done` only as it ends.
    if t.done.recv_timeout(CLOSE_WAIT) == Err(RecvTimeoutError::Disconnected) {
        let _ = t.thread.join();
    }
}

/// The window's thread: creates and shows the window, then runs its message loop until
/// the window is destroyed.
fn run(
    devices: Vec<Device>,
    history: HashMap<DeviceKey, Vec<Sample>>,
    queue: Receiver<Update>,
    slot: &AtomicIsize,
) {
    let Some(hwnd) = create() else {
        return;
    };
    WINDOW.with(|cell| {
        *cell.borrow_mut() = Some(Window {
            font: None,
            devices: Vec::new(),
            selected: None,
            range: 0,
            history,
            queue,
        });
    });
    set_font(hwnd);
    set_devices(hwnd, devices);
    layout(hwnd);
    // SAFETY: our own window and its button.
    unsafe {
        if let Ok(first) = GetDlgItem(Some(hwnd), ID_RANGE) {
            SendMessageW(first, BM_SETCHECK, Some(WPARAM(BST_CHECKED)), None);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
    }
    slot.store(hwnd.0 as isize, Ordering::SeqCst);
    // What was sent before the window existed (and so not posted).
    take_updates(hwnd);
    let mut msg = MSG::default();
    // SAFETY: `msg` outlives each call. GetMessageW returns -1 on error, 0 on WM_QUIT
    // (posted when the window is destroyed).
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` came from GetMessageW; the dialog keys (Tab, arrows, Esc) are
        // handled by IsDialogMessageW.
        unsafe {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    slot.store(0, Ordering::SeqCst);
    // The controls are gone: the font can go.
    WINDOW.with(|cell| cell.borrow_mut().take());
}

/// Carries out the updates queued by the UI thread.
fn take_updates(hwnd: HWND) {
    while let Some(Ok(update)) = with_window(|w| w.queue.try_recv()) {
        match update {
            Update::Show(devices) => {
                set_devices(hwnd, devices);
                invalidate(hwnd);
                // SAFETY: our own window.
                unsafe {
                    if IsIconic(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(hwnd);
                }
            }
            Update::Data { devices, samples } => {
                set_devices(hwnd, devices);
                if !samples.is_empty() {
                    with_window(|w| history::add(&mut w.history, samples, unix_now_ms()));
                    invalidate(hwnd);
                }
            }
            Update::Close => {
                // SAFETY: our own window; WM_DESTROY ends the message loop.
                let _ = unsafe { DestroyWindow(hwnd) };
                return;
            }
        }
    }
}

struct Window {
    /// The message font for the window's DPI, used by the controls and the text.
    font: Option<HFONT>,
    /// In list order.
    devices: Vec<Device>,
    selected: Option<DeviceKey>,
    /// Index into [`RANGES`].
    range: usize,
    history: HashMap<DeviceKey, Vec<Sample>>,
    /// Updates from the UI thread.
    queue: Receiver<Update>,
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(font) = self.font.take() {
            // SAFETY: our font; dropped after the window and its controls are destroyed.
            let _ = unsafe { DeleteObject(font.into()) };
        }
    }
}

/// Runs `f` on the window's state; `None` when it is closed or borrowed.
fn with_window<R>(f: impl FnOnce(&mut Window) -> R) -> Option<R> {
    WINDOW.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn invalidate(hwnd: HWND) {
    // SAFETY: our own window; only queues a paint.
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}

/// `value` at 96 DPI, at `dpi`.
fn scale(value: i32, dpi: u32) -> i32 {
    value * dpi as i32 / 96
}

/// Creates the window and its controls, centred on the monitor with the cursor (where the
/// menu was), sized for that monitor's DPI; hidden.
fn create() -> Option<HWND> {
    let instance = hinstance();
    // SAFETY: literal strings; `cursor`, `info`, `frame` outlive the calls using them.
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let (mut dpi, mut dpi_y) = (96, 96);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: scale(START_SIZE.0, dpi),
            bottom: scale(START_SIZE.1, dpi),
        };
        let _ = AdjustWindowRectExForDpi(&mut frame, STYLE, false, EX_STYLE, dpi);
        let (width, height) = (frame.right - frame.left, frame.bottom - frame.top);
        let (x, y) = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            (
                work.left + ((work.right - work.left - width) / 2).max(0),
                work.top + ((work.bottom - work.top - height) / 2).max(0),
            )
        } else {
            (CW_USEDEFAULT, CW_USEDEFAULT)
        };
        let hwnd = CreateWindowExW(
            EX_STYLE,
            HISTORY_CLASS,
            w!("Battery history — BatterySight"),
            STYLE,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()?;
        let list = WS_TABSTOP
            | WS_GROUP
            | WS_VSCROLL
            | WINDOW_STYLE((LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32);
        let mut made = child(
            hwnd,
            w!("LISTBOX"),
            PCWSTR::null(),
            list,
            WS_EX_CLIENTEDGE,
            ID_LIST,
        );
        for (i, (label, _)) in RANGES.iter().enumerate() {
            let mut style = WINDOW_STYLE((BS_AUTORADIOBUTTON | BS_PUSHLIKE) as u32);
            if i == 0 {
                style |= WS_GROUP | WS_TABSTOP;
            }
            made &= child(
                hwnd,
                w!("BUTTON"),
                *label,
                style,
                WINDOW_EX_STYLE(0),
                ID_RANGE + i as i32,
            );
        }
        if !made {
            let _ = DestroyWindow(hwnd);
            return None;
        }
        Some(hwnd)
    }
}

/// Creates a visible child control of `parent` with `id`; placed by [`layout`].
fn child(
    parent: HWND,
    class: PCWSTR,
    text: PCWSTR,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    id: i32,
) -> bool {
    // SAFETY: literal strings; `parent` is our window.
    unsafe {
        CreateWindowExW(
            ex_style,
            class,
            text,
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(id as isize as *mut c_void)),
            Some(hinstance()),
            None,
        )
    }
    .is_ok()
}

/// Where the controls, the time-left line and the chart go in a `client` area at `dpi`.
struct Areas {
    list: RECT,
    ranges: [RECT; RANGES.len()],
    text: (i32, i32),
    chart: RECT,
}

fn areas(client: RECT, dpi: u32) -> Areas {
    let px = |v| scale(v, dpi);
    let margin = px(12);
    let list = RECT {
        left: margin,
        top: margin,
        right: margin + px(200),
        bottom: (client.bottom - margin).max(margin),
    };
    let left = list.right + margin;
    let ranges = std::array::from_fn(|i| {
        let x = left + i as i32 * px(70);
        RECT {
            left: x,
            top: margin,
            right: x + px(64),
            bottom: margin + px(28),
        }
    });
    let text_top = margin + px(40);
    let chart_top = text_top + px(28);
    Areas {
        list,
        ranges,
        text: (left, text_top),
        chart: RECT {
            left,
            top: chart_top,
            right: (client.right - margin).max(left),
            bottom: (client.bottom - margin).max(chart_top),
        },
    }
}

/// Places the controls for the window's size and DPI.
fn layout(hwnd: HWND) {
    // SAFETY: our own window and its controls; `client` outlives the call.
    unsafe {
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let a = areas(client, GetDpiForWindow(hwnd));
        let places = std::iter::once((ID_LIST, a.list)).chain(
            a.ranges
                .iter()
                .enumerate()
                .map(|(i, r)| (ID_RANGE + i as i32, *r)),
        );
        for (id, r) in places {
            if let Ok(control) = GetDlgItem(Some(hwnd), id) {
                let _ = SetWindowPos(
                    control,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
}

/// Gives the window and its controls the message font for its DPI.
fn set_font(hwnd: HWND) {
    // SAFETY: `metrics` outlives the calls; the old font is deleted only once no control
    // uses it.
    unsafe {
        let mut metrics = NONCLIENTMETRICSW {
            cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let got = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast()),
            0,
            GetDpiForWindow(hwnd),
        );
        if got.is_err() {
            return;
        }
        let font = CreateFontIndirectW(&metrics.lfMessageFont);
        if font.is_invalid() {
            return;
        }
        let Some(old) = with_window(|w| w.font.replace(font)) else {
            let _ = DeleteObject(font.into());
            return;
        };
        for id in std::iter::once(ID_LIST).chain((0..RANGES.len() as i32).map(|i| ID_RANGE + i)) {
            if let Ok(control) = GetDlgItem(Some(hwnd), id) {
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
        if let Some(old) = old {
            let _ = DeleteObject(old.into());
        }
    }
}

/// Lists `devices` if they changed, keeping the device chosen if it is still listed (else
/// choosing the first).
fn set_devices(hwnd: HWND, devices: Vec<Device>) {
    let changed = with_window(|w| {
        if w.devices == devices {
            return None;
        }
        if !w
            .selected
            .as_ref()
            .is_some_and(|key| devices.iter().any(|d| d.key == *key))
        {
            w.selected = devices.first().map(|d| d.key.clone());
        }
        let at = w
            .selected
            .as_ref()
            .and_then(|key| devices.iter().position(|d| d.key == *key));
        let names: Vec<Vec<u16>> = devices.iter().map(|d| wide(&d.name)).collect();
        w.devices = devices;
        Some((names, at))
    });
    let Some(Some((names, at))) = changed else {
        return;
    };
    // SAFETY: our own list box; each name outlives the call that copies it.
    unsafe {
        let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) else {
            return;
        };
        SendMessageW(list, LB_RESETCONTENT, None, None);
        for name in &names {
            SendMessageW(
                list,
                LB_ADDSTRING,
                None,
                Some(LPARAM(name.as_ptr() as isize)),
            );
        }
        if let Some(at) = at {
            SendMessageW(list, LB_SETCURSEL, Some(WPARAM(at)), None);
        }
    }
    invalidate(hwnd);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The window procedure, on the window's thread. It handles input, paint, DPI changes and
/// the UI thread's updates on the window's own state (see the module doc).
pub unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            layout(hwnd);
            invalidate(hwnd);
        }
        WM_GETMINMAXINFO => {
            // SAFETY: WM_GETMINMAXINFO's lparam points to a MINMAXINFO; our own window.
            unsafe {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let info = &mut *(lp.0 as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: scale(MIN_SIZE.0, dpi),
                    y: scale(MIN_SIZE.1, dpi),
                };
            }
        }
        WM_DPICHANGED => {
            set_font(hwnd);
            // SAFETY: WM_DPICHANGED's lparam points to the suggested window rectangle.
            unsafe {
                let r = *(lp.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        WM_COMMAND => command(hwnd, (wp.0 & 0xFFFF) as i32, ((wp.0 >> 16) & 0xFFFF) as u32),
        WM_PAINT => paint(hwnd),
        // Paint covers everything.
        WM_ERASEBKGND => return LRESULT(1),
        WM_APP_UPDATES => take_updates(hwnd),
        // Ends the window's message loop; its state goes with the loop.
        // SAFETY: no pointers involved.
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        // SAFETY: forwarding this window's own message.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

fn command(hwnd: HWND, id: i32, code: u32) {
    if id == ID_LIST && code == LBN_SELCHANGE {
        // SAFETY: our own list box.
        let at = unsafe { GetDlgItem(Some(hwnd), ID_LIST) }
            .map(|list| unsafe { SendMessageW(list, LB_GETCURSEL, None, None) }.0);
        with_window(|w| {
            let device = at
                .ok()
                .and_then(|at| usize::try_from(at).ok())
                .and_then(|at| w.devices.get(at));
            w.selected = device.map(|d| d.key.clone());
        });
        invalidate(hwnd);
    } else if (ID_RANGE..ID_RANGE + RANGES.len() as i32).contains(&id) && code == BN_CLICKED {
        with_window(|w| w.range = (id - ID_RANGE) as usize);
        invalidate(hwnd);
    } else if id == IDCANCEL.0 {
        // Esc.
        // SAFETY: our own window; WM_DESTROY ends the message loop.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

/// Paints the time-left line and the chart through a memory bitmap (no flicker); the
/// controls are clipped out (`WS_CLIPCHILDREN`).
fn paint(hwnd: HWND) {
    // SAFETY: our own window; BeginPaint is paired with EndPaint, the memory DC and bitmap
    // are made, deselected and deleted here.
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let (width, height) = (client.right.max(1), client.bottom.max(1));
        let memory = CreateCompatibleDC(Some(dc));
        let bitmap = CreateCompatibleBitmap(dc, width, height);
        let original = SelectObject(memory, bitmap.into());
        FillRect(memory, &client, GetSysColorBrush(COLOR_WINDOW));
        let a = areas(client, GetDpiForWindow(hwnd));
        WINDOW.with(|cell| {
            if let Ok(state) = cell.try_borrow()
                && let Some(w) = state.as_ref()
            {
                w.draw(memory, &a);
            }
        });
        let _ = BitBlt(dc, 0, 0, width, height, Some(memory), 0, 0, SRCCOPY);
        SelectObject(memory, original);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        let _ = EndPaint(hwnd, &ps);
    }
}

impl Window {
    /// The time-left line and the chart of the device chosen, on `dc`.
    fn draw(&self, dc: HDC, a: &Areas) {
        let samples = self
            .selected
            .as_ref()
            .and_then(|key| self.history.get(key))
            .map_or(&[][..], Vec::as_slice);
        let line = if self.selected.is_some() {
            history::time_left_text(history::time_left(samples, unix_now_ms()))
        } else {
            "No devices yet".to_owned()
        };
        // SAFETY: GDI calls on the caller's memory DC; the font is ours and alive, and is
        // selected out again before returning.
        unsafe {
            let original = self.font.map(|font| SelectObject(dc, font.into()));
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
            let text = wide(&line);
            let _ = TextOutW(dc, a.text.0, a.text.1, &text[..text.len() - 1]);
            chart::paint(dc, a.chart, samples, RANGES[self.range].1, true);
            if let Some(original) = original {
                SelectObject(dc, original);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{
        Charge, DeviceKind, Level, Presence, Source, Verification, Via,
    };

    fn state(key: &str, name: &str, presence: Presence) -> DeviceState {
        DeviceState {
            key: DeviceKey(key.into()),
            name: name.into(),
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: Some(Level::Percent(50)),
            millivolts: None,
            charge: Charge::Discharging,
            presence,
            updated_ms: 0,
            source: Source::Query,
            verification: Verification::HardwareTested,
        }
    }

    fn sample(key: &str) -> Sample {
        Sample {
            at_unix_ms: 1,
            key: DeviceKey(key.into()),
            level: None,
            charge: Charge::Unknown,
            presence: Presence::Online,
            millivolts: None,
            source: Source::Query,
        }
    }

    #[test]
    fn devices_in_user_order_then_history_only_keys() {
        let states = [
            state("a", "G305", Presence::Online),
            state("b", "PRO X", Presence::Offline),
            state("c", "K400", Presence::Asleep),
        ];
        let mut settings = Settings {
            order: vec![DeviceKey("c".into())],
            ..Settings::default()
        };
        // Hidden devices are listed: hiding is for the taskbar only.
        settings.set_hidden(&DeviceKey("a".into()), true);
        let history = HashMap::from([
            (DeviceKey("z".into()), vec![sample("z")]),
            (DeviceKey("a".into()), vec![sample("a")]),
            (DeviceKey("y".into()), vec![sample("y")]),
        ]);
        let names: Vec<String> = devices(&states, &settings, &history)
            .into_iter()
            .map(|d| d.name)
            .collect();
        // Devices not connected now are listed by key.
        assert_eq!(names, ["K400", "G305", "PRO X", "y", "z"]);
    }
}
