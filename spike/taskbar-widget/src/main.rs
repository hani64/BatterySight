//! Throwaway spike (Task 2a): a taskbar widget drawn as a layered child window of `Shell_TrayWnd`
//! (and of each `Shell_SecondaryTrayWnd`), plus a tray icon, a stallable worker thread and a
//! message-only window that takes an 8 000 msg/s burst. Not part of the workspace, no tests.
//!
//! Results and the technique to port: docs/spike/taskbar-results.md.
//!
//! Usage (one exe, several roles):
//!   spike run [child|overlay]     run the widget (default: child)
//!   spike cmd <stall|burst|stall-ui|detach|report|recreate|quit|place:<x>|place-off>
//!   spike probe                   dump monitors, taskbars, their child windows and our widgets
//!   spike trycreate               CreateWindowExW / SetParent matrix of styles and parents
//!   spike measure <secs> [cmd] [delay_ms]
//!                                 time SendMessageTimeout(WM_NULL) round trips to Shell_TrayWnd,
//!                                 optionally posting <cmd> to the running spike after delay_ms
//!   spike hover-test <cmd|none> [touch|click] <tag>
//!                                 does the taskbar still react to mouse input while <cmd> runs?
//!   spike autohide <get|on|off>   ABM_GETSTATE / ABM_SETSTATE
//!   spike activate-temp <secs>    make a small window of ours foreground (lets auto-hide hide)
//!   spike theme-broadcast         WM_SETTINGCHANGE "ImmersiveColorSet" to all top-level windows
//!   spike cursor [x y]            print or move the cursor (injected input, physical pixels)
//!   spike shot x y w h out.bmp    screen capture in physical pixels

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, w};

const HOST_CLASS: PCWSTR = w!("BatterySightSpikeHost");
const WIDGET_CLASS: PCWSTR = w!("BatterySightSpikeWidget");
const SINK_CLASS: PCWSTR = w!("BatterySightSpikeSink");

const CMD_STALL: u32 = WM_APP + 1;
const CMD_BURST: u32 = WM_APP + 2;
const CMD_STALL_UI: u32 = WM_APP + 3;
const CMD_QUIT: u32 = WM_APP + 4;
const CMD_RECREATE: u32 = WM_APP + 5;
const CMD_DETACH: u32 = WM_APP + 6;
const CMD_REPORT: u32 = WM_APP + 7;
/// wParam = screen x for the primary widget's left edge, lParam 1 = set, 0 = back to the anchor.
const CMD_PLACE: u32 = WM_APP + 8;
const WM_TRAY: u32 = WM_APP + 20;
const SINK_MSG: u32 = WM_APP + 30;
const SINK_DONE: u32 = WM_APP + 31;

const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 250;
const STALL_SECS: u64 = 10;
const BURST_RATE: u64 = 8_000;
const BURST_SECS: u64 = 10;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static SINK: AtomicIsize = AtomicIsize::new(0);
static SINK_RECEIVED: AtomicU64 = AtomicU64::new(0);
static SINK_MAX_LAT_US: AtomicU64 = AtomicU64::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn log(msg: impl AsRef<str>) {
    let t = START.elapsed().as_millis();
    let tid = unsafe { GetCurrentThreadId() };
    let line = format!("[{t:>7} ms t{tid}] {}", msg.as_ref());
    println!("{line}");
    let _ = std::io::stdout().flush();
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

fn qpc() -> i64 {
    let mut v = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

fn qpc_freq() -> i64 {
    let mut v = 0;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut v);
    }
    v
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn window_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(h, &mut r);
    }
    r
}

fn client_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(h, &mut r);
    }
    r
}

fn fmt_rect(r: &RECT) -> String {
    format!("({},{})-({},{}) {}x{}", r.left, r.top, r.right, r.bottom, r.right - r.left, r.bottom - r.top)
}

fn awareness(ctx: DPI_AWARENESS_CONTEXT) -> String {
    unsafe {
        if AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool() {
            return "PerMonitorV2".into();
        }
        match GetAwarenessFromDpiAwarenessContext(ctx).0 {
            0 => "Unaware".into(),
            1 => "System".into(),
            2 => "PerMonitor(v1)".into(),
            v => format!("invalid({v})"),
        }
    }
}

fn light_theme() -> bool {
    let mut v: u32 = 0;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS && v != 0
}

/// Primary taskbar first, then every secondary taskbar.
fn taskbars() -> Vec<(HWND, bool)> {
    let mut out = Vec::new();
    unsafe {
        if let Ok(p) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) {
            out.push((p, true));
        }
        let mut after: Option<HWND> = None;
        while let Ok(s) = FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), PCWSTR::null()) {
            out.push((s, false));
            after = Some(s);
        }
    }
    out
}

/// Screen x of the left edge of whatever the widget sits left of: the notification area
/// (`TrayNotifyWnd`) on the primary taskbar. A Windows 11 secondary taskbar has no child window for
/// its clock, so its left edge comes from the UI Automation thread (`SECONDARY_CLOCK`).
fn anchor_x(bar: HWND, primary: bool) -> Option<i32> {
    unsafe {
        if let Ok(h) = FindWindowExW(Some(bar), None, w!("TrayNotifyWnd"), PCWSTR::null()) {
            let r = window_rect(h);
            if IsWindowVisible(h).as_bool() && r.right > r.left {
                return Some(r.left);
            }
        }
    }
    if primary {
        return None;
    }
    SECONDARY_CLOCK.lock().unwrap().iter().find(|(b, _)| *b == bar.0 as isize).map(|&(_, x)| x)
}

/// (secondary taskbar HWND, screen x of its clock's left edge), refreshed by `uia_thread`.
static SECONDARY_CLOCK: Mutex<Vec<(isize, i32)>> = Mutex::new(Vec::new());

/// UI Automation queries go to Explorer's UI thread. They run here, on a thread that owns no
/// windows, never on the thread that owns the taskbar child (Explorer waits on that thread).
fn uia_thread() {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(u) => u,
            Err(e) => {
                log(format!("UIA: CoCreateInstance failed: {e}"));
                return;
            }
        };
        let Ok(all_cond) = uia.CreateTrueCondition() else { return };
        let mut dumped = false;
        let mut last: Vec<(isize, i32)> = Vec::new();
        loop {
            let mut found = Vec::new();
            for (bar, primary) in taskbars() {
                if primary {
                    continue;
                }
                let br = window_rect(bar);
                let t0 = Instant::now();
                let Ok(root) = uia.ElementFromHandle(bar) else { continue };
                let Ok(all) = root.FindAll(TreeScope_Descendants, &all_cond) else { continue };
                let n = all.Length().unwrap_or(0);
                let mut clock_left: Option<i32> = None;
                for i in 0..n {
                    let Ok(el) = all.GetElement(i) else { continue };
                    let id = el.CurrentAutomationId().map(|b| b.to_string()).unwrap_or_default();
                    let r = el.CurrentBoundingRectangle().unwrap_or_default();
                    if !dumped {
                        let cls = el.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
                        log(format!("UIA {}: id={id:?} class={cls:?} rect {}", class_name(bar), fmt_rect(&r)));
                    }
                    if id == "SystemTrayIcon" && r.right > r.left && r.left >= br.left && r.right <= br.right {
                        clock_left = Some(clock_left.map_or(r.left, |c| c.min(r.left)));
                    }
                }
                if let Some(x) = clock_left {
                    found.push((bar.0 as isize, x));
                }
                if found != last || !dumped {
                    log(format!(
                        "UIA: {} descendants of secondary taskbar {:?}, clock left = {clock_left:?} ({} ms)",
                        n,
                        bar.0,
                        t0.elapsed().as_millis()
                    ));
                }
            }
            dumped = true;
            if found != last {
                last = found.clone();
            }
            *SECONDARY_CLOCK.lock().unwrap() = found;
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

// ---------------------------------------------------------------- rendering

const SEGMENTS: [(&str, bool); 4] = [("\u{1F3A7}", true), (" 88%   ", false), ("\u{1F5B1}", true), (" 34%", false)];

/// Renders "🎧 88%  🖱 34%" into a premultiplied 32-bit DIB selected into a memory DC.
struct Bitmap {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

fn render(dpi: u32, height: i32, light: bool) -> Option<Bitmap> {
    let px = |v: i32| v * dpi as i32 / 96;
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = |face: PCWSTR| {
            CreateFontW(
                -px(12),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                face,
            )
        };
        let text_font = font(w!("Segoe UI"));
        let emoji_font = font(w!("Segoe UI Emoji"));
        let pad = px(4);
        let mut sizes = Vec::new();
        let mut width = pad * 2;
        for (s, emoji) in SEGMENTS {
            let ws: Vec<u16> = s.encode_utf16().collect();
            SelectObject(dc, if emoji { emoji_font } else { text_font }.into());
            let mut sz = SIZE::default();
            let _ = GetTextExtentPoint32W(dc, &ws, &mut sz);
            width += sz.cx;
            sizes.push((ws, emoji, sz));
        }
        let h = height.max(1);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bmp.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
        let mut x = pad;
        for (ws, emoji, sz) in &sizes {
            SelectObject(dc, if *emoji { emoji_font } else { text_font }.into());
            let _ = TextOutW(dc, x, (h - sz.cy) / 2, ws);
            x += sz.cx;
        }
        let _ = GdiFlush();
        SelectObject(dc, text_font.into()); // deselect before deleting the fonts below
        let _ = DeleteObject(emoji_font.into());
        let _ = DeleteObject(text_font.into());
        // White-on-black grayscale coverage becomes alpha; colour is premultiplied.
        let fg: u32 = if light { 0x00 } else { 0xFF };
        let px_slice = std::slice::from_raw_parts_mut(bits as *mut u8, (width * h * 4) as usize);
        for p in px_slice.chunks_exact_mut(4) {
            let a = p[0].max(p[1]).max(p[2]) as u32;
            let c = (fg * a / 255) as u8;
            p[0] = c;
            p[1] = c;
            p[2] = c;
            p[3] = a as u8;
        }
        Some(Bitmap { dc, bmp, old, w: width, h })
    }
}

// ---------------------------------------------------------------- widget state (UI thread only)

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Child,
    Overlay,
}

struct Widget {
    hwnd: HWND,
    bar: HWND,
    primary: bool,
    dpi: u32,
    height: i32,
    light: bool,
    size: SIZE,
    pos: Option<(i32, i32)>,
    raises: u32,
    moves: u32,
}

struct State {
    mode: Mode,
    host: HWND,
    widgets: Vec<Widget>,
    tray_added: bool,
    light: bool,
    worker: mpsc::Sender<()>,
    recreate_started: Option<Instant>,
    /// Test override: screen x of the primary widget's left edge (alpha check over a task button).
    place_x: Option<i32>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// None while the state is borrowed: Win32 calls that talk to Explorer (SetWindowPos on its child,
/// Shell_NotifyIcon, UpdateLayeredWindow...) dispatch incoming sent messages, so `host_proc` can be
/// re-entered from inside `tick`.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(PCWSTR::null()).map(|m| HINSTANCE(m.0)).unwrap_or_default() }
}

fn create_widget(mode: Mode, bar: HWND, primary: bool) -> Option<Widget> {
    unsafe {
        let before = awareness(GetThreadDpiAwarenessContext());
        let proc_before = awareness(GetDpiAwarenessContextForProcess(HANDLE::default()));
        let r = match mode {
            Mode::Child => CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE,
                WIDGET_CLASS,
                w!("BatterySight spike widget"),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                Some(bar),
                None,
                Some(hinstance()),
                None,
            ),
            Mode::Overlay => CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                WIDGET_CLASS,
                w!("BatterySight spike widget"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance()),
                None,
            ),
        };
        let hwnd = match r {
            Ok(h) => h,
            Err(e) => {
                log(format!("CreateWindowExW({mode:?}) on {:?} failed: {e}", bar.0));
                return None;
            }
        };
        let mut explorer_pid = 0u32;
        let explorer_tid = GetWindowThreadProcessId(bar, Some(&mut explorer_pid));
        log(format!(
            "widget {:?} created: mode={mode:?} primary={primary} bar={:?} ({}) explorer pid={explorer_pid} tid={explorer_tid}; \
             DPI awareness thread {before} -> {}, process {proc_before} -> {}, window {}; bar DPI {}",
            hwnd.0,
            bar.0,
            class_name(bar),
            awareness(GetThreadDpiAwarenessContext()),
            awareness(GetDpiAwarenessContextForProcess(HANDLE::default())),
            awareness(GetWindowDpiAwarenessContext(hwnd)),
            GetDpiForWindow(bar),
        ));
        Some(Widget {
            hwnd,
            bar,
            primary,
            dpi: 0,
            height: 0,
            light: false,
            size: SIZE::default(),
            pos: None,
            raises: 0,
            moves: 0,
        })
    }
}

/// Paints with UpdateLayeredWindow (per-pixel alpha). Position is set separately with SetWindowPos.
fn paint(w: &mut Widget, light: bool, overlay_pos: Option<POINT>) -> bool {
    let dpi = unsafe { GetDpiForWindow(w.bar) }.max(96);
    let height = client_rect(w.bar).bottom;
    let Some(bm) = render(dpi, height, light) else {
        log("render failed");
        return false;
    };
    let size = SIZE { cx: bm.w, cy: bm.h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    let r = unsafe {
        UpdateLayeredWindow(
            w.hwnd,
            None,
            overlay_pos.as_ref().map(|p| p as *const POINT),
            Some(&size),
            Some(bm.dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };
    match r {
        Ok(()) => {
            log(format!(
                "painted {:?}: {}x{} dpi={dpi} light={light} (bar client height {height})",
                w.hwnd.0, size.cx, size.cy
            ));
            w.dpi = dpi;
            w.height = height;
            w.light = light;
            w.size = size;
            w.pos = None;
            true
        }
        Err(e) => {
            log(format!("UpdateLayeredWindow({:?}) failed: {e}", w.hwnd.0));
            false
        }
    }
}

fn position(w: &mut Widget, mode: Mode, place_x: Option<i32>) {
    unsafe {
        let Some(anchor) = anchor_x(w.bar, w.primary) else {
            if IsWindowVisible(w.hwnd).as_bool() || (GetWindowLongW(w.hwnd, GWL_STYLE) as u32 & WS_VISIBLE.0) != 0 {
                let _ = ShowWindow(w.hwnd, SW_HIDE);
                log(format!("widget {:?}: no anchor on {}, hidden", w.hwnd.0, class_name(w.bar)));
            }
            w.pos = None;
            return;
        };
        let gap = (4 * w.dpi as i32) / 96;
        let bar = window_rect(w.bar);
        match mode {
            Mode::Child => {
                let mut p = POINT { x: anchor, y: bar.top };
                let _ = ScreenToClient(w.bar, &mut p);
                let left = match place_x {
                    Some(x) => {
                        let mut q = POINT { x, y: bar.top };
                        let _ = ScreenToClient(w.bar, &mut q);
                        q.x
                    }
                    None => p.x - w.size.cx - gap,
                };
                let covered = GetWindow(w.hwnd, GW_HWNDPREV).is_ok();
                let shown = (GetWindowLongW(w.hwnd, GWL_STYLE) as u32 & WS_VISIBLE.0) != 0;
                if w.pos == Some((left, 0)) && !covered && shown {
                    return;
                }
                if covered {
                    w.raises += 1;
                }
                if w.pos != Some((left, 0)) {
                    w.moves += 1;
                    log(format!(
                        "position {:?}: client x={left} (anchor screen x={anchor}, bar {}) covered={covered} shown={shown}",
                        w.hwnd.0,
                        fmt_rect(&bar)
                    ));
                }
                w.pos = Some((left, 0));
                if let Err(e) =
                    SetWindowPos(w.hwnd, Some(HWND_TOP), left, 0, w.size.cx, w.size.cy, SWP_NOACTIVATE | SWP_SHOWWINDOW)
                {
                    log(format!("SetWindowPos failed: {e}"));
                }
            }
            Mode::Overlay => {
                let x = anchor - w.size.cx - gap;
                let y = bar.top;
                if w.pos != Some((x, y)) {
                    w.moves += 1;
                    log(format!("overlay position {:?}: screen ({x},{y}), bar {}", w.hwnd.0, fmt_rect(&bar)));
                }
                w.pos = Some((x, y));
                let _ = SetWindowPos(
                    w.hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    w.size.cx,
                    w.size.cy,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    }
}

fn tick(s: &mut State) {
    unsafe {
        let light = light_theme();
        if light != s.light {
            log(format!("theme change seen by registry poll: light={light}"));
            s.light = light;
        }
        // Drop widgets whose window or taskbar went away (Explorer restart).
        s.widgets.retain(|w| {
            let alive = IsWindow(Some(w.hwnd)).as_bool() && IsWindow(Some(w.bar)).as_bool();
            if !alive {
                log(format!(
                    "widget {:?} gone (widget alive={}, bar alive={})",
                    w.hwnd.0,
                    IsWindow(Some(w.hwnd)).as_bool(),
                    IsWindow(Some(w.bar)).as_bool()
                ));
                if IsWindow(Some(w.hwnd)).as_bool() {
                    let _ = DestroyWindow(w.hwnd);
                }
            }
            alive
        });
        for (bar, primary) in taskbars() {
            if s.widgets.iter().any(|w| w.bar == bar) {
                continue;
            }
            if primary && FindWindowExW(Some(bar), None, w!("TrayNotifyWnd"), PCWSTR::null()).is_err() {
                continue; // taskbar not laid out yet
            }
            if let Some(w) = create_widget(s.mode, bar, primary) {
                s.widgets.push(w);
            }
        }
        let mode = s.mode;
        let place_x = s.place_x;
        for w in &mut s.widgets {
            let dpi = GetDpiForWindow(w.bar).max(96);
            let height = client_rect(w.bar).bottom;
            if w.size.cx == 0 || dpi != w.dpi || height != w.height || s.light != w.light {
                let overlay = (mode == Mode::Overlay).then(|| POINT { x: window_rect(w.bar).left, y: window_rect(w.bar).top });
                paint(w, s.light, overlay);
            }
            if w.size.cx > 0 {
                position(w, mode, if w.primary { place_x } else { None });
            }
        }
        if let Some(t) = s.recreate_started {
            if s.widgets.iter().any(|w| w.primary && w.pos.is_some()) {
                log(format!("widget back on the taskbar {} ms after TaskbarCreated", t.elapsed().as_millis()));
                s.recreate_started = None;
            }
        }
        if !s.tray_added {
            s.tray_added = add_tray(s.host);
        }
    }
}

fn add_tray(host: HWND) -> bool {
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: host,
            uID: 1,
            uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE,
            uCallbackMessage: WM_TRAY,
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            ..Default::default()
        };
        for (d, c) in nid.szTip.iter_mut().zip("BatterySight spike: right-click for tests".encode_utf16()) {
            *d = c;
        }
        let added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let versioned = added && Shell_NotifyIconW(NIM_SETVERSION, &nid).as_bool();
        log(format!("tray icon NIM_ADD={added} NIM_SETVERSION(4)={versioned}"));
        added
    }
}

fn remove_tray(host: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: host, uID: 1, ..Default::default() };
        let ok = Shell_NotifyIconW(NIM_DELETE, &nid).as_bool();
        log(format!("tray icon NIM_DELETE={ok}"));
    }
}

fn destroy_widgets(s: &mut State) {
    for w in s.widgets.drain(..) {
        unsafe {
            if IsWindow(Some(w.hwnd)).as_bool() {
                let _ = DestroyWindow(w.hwnd);
            }
        }
    }
}

fn report(s: &State) {
    unsafe {
        for w in &s.widgets {
            let r = window_rect(w.hwnd);
            let prev = GetWindow(w.hwnd, GW_HWNDPREV).map(class_name).unwrap_or_else(|_| "-".into());
            log(format!(
                "report {:?}: primary={} rect {} visible={} pos={:?} raises={} moves={} above-sibling={prev} parent={}",
                w.hwnd.0,
                w.primary,
                fmt_rect(&r),
                IsWindowVisible(w.hwnd).as_bool(),
                w.pos,
                w.raises,
                w.moves,
                GetParent(w.hwnd).map(class_name).unwrap_or_else(|_| "-".into())
            ));
        }
    }
}

/// Raymond Chen: a cross-process child attaches the creating thread's input queue to the
/// parent's thread. Detaching succeeds only if the two queues were attached.
fn detach_probe(s: &State) {
    unsafe {
        let me = GetCurrentThreadId();
        for w in s.widgets.iter().filter(|w| w.primary) {
            let tid = GetWindowThreadProcessId(w.bar, None);
            let detached = AttachThreadInput(me, tid, false).as_bool();
            let err = GetLastError();
            log(format!("AttachThreadInput(ui {me}, explorer {tid}, FALSE) = {detached} (last error {err:?})"));
        }
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let tb_created = TASKBAR_CREATED.load(Ordering::Relaxed);
        let ours = (msg == tb_created && msg != 0) || (WM_APP..WM_APP + 0x100).contains(&msg);
        if ours && STATE.with(|s| s.try_borrow_mut().is_err()) {
            // Re-entered from a cross-process call inside tick(); handle it after tick returns.
            log(format!("re-entrant message 0x{msg:X} while state is busy: re-posted"));
            let _ = PostMessageW(Some(hwnd), msg, wp, lp);
            return LRESULT(0);
        }
        if msg == tb_created && msg != 0 {
            log("TaskbarCreated received");
            with_state(|s| {
                for w in &s.widgets {
                    log(format!(
                        "  old widget {:?}: IsWindow={} old bar IsWindow={}",
                        w.hwnd.0,
                        IsWindow(Some(w.hwnd)).as_bool(),
                        IsWindow(Some(w.bar)).as_bool()
                    ));
                }
                destroy_widgets(s);
                s.tray_added = add_tray(s.host);
                s.recreate_started = Some(Instant::now());
                tick(s);
            });
            return LRESULT(0);
        }
        match msg {
            WM_TIMER if wp.0 == TIMER_TICK => {
                with_state(tick);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                let area = if lp.0 != 0 { PCWSTR(lp.0 as *const u16).to_string().unwrap_or_default() } else { String::new() };
                if area == "ImmersiveColorSet" {
                    log(format!("WM_SETTINGCHANGE ImmersiveColorSet; registry light={}", light_theme()));
                    with_state(tick);
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                log("WM_DISPLAYCHANGE");
                with_state(tick);
                LRESULT(0)
            }
            CMD_STALL => {
                log("cmd: stall worker thread");
                with_state(|s| s.worker.send(()).ok());
                LRESULT(0)
            }
            CMD_BURST => {
                log("cmd: burst");
                std::thread::spawn(burst);
                LRESULT(0)
            }
            CMD_STALL_UI => {
                log(format!("cmd: stall UI/widget thread for {STALL_SECS} s"));
                std::thread::sleep(Duration::from_secs(STALL_SECS));
                log("UI/widget thread stall over");
                LRESULT(0)
            }
            CMD_DETACH => {
                with_state(|s| detach_probe(s));
                LRESULT(0)
            }
            CMD_PLACE => {
                with_state(|s| {
                    s.place_x = (lp.0 != 0).then_some(wp.0 as i32);
                    log(format!("cmd: place primary widget at {:?}", s.place_x));
                    tick(s);
                });
                LRESULT(0)
            }
            CMD_REPORT => {
                with_state(|s| report(s));
                LRESULT(0)
            }
            CMD_RECREATE => {
                with_state(|s| {
                    destroy_widgets(s);
                    tick(s);
                });
                LRESULT(0)
            }
            CMD_QUIT => {
                log("cmd: quit");
                with_state(|s| {
                    destroy_widgets(s);
                    remove_tray(s.host);
                });
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_TRAY => {
                let event = (lp.0 & 0xFFFF) as u32;
                if event == WM_CONTEXTMENU || event == WM_RBUTTONUP {
                    tray_menu(hwnd);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn tray_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, CMD_STALL as usize, w!("Stall worker 10 s"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_BURST as usize, w!("Burst 8000 msg/s for 10 s"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_STALL_UI as usize, w!("Stall widget thread 10 s"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, None, hwnd, None).0 as u32;
        let _ = DestroyMenu(menu);
        if cmd != 0 {
            let _ = PostMessageW(Some(hwnd), cmd, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DPICHANGED_BEFOREPARENT | WM_DPICHANGED_AFTERPARENT => {
                log(format!("widget {:?}: WM_DPICHANGED_{}PARENT", hwnd.0, if msg == WM_DPICHANGED_BEFOREPARENT { "BEFORE" } else { "AFTER" }));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_LBUTTONUP => {
                log(format!("widget {:?}: click", hwnd.0));
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---------------------------------------------------------------- other threads

fn worker(rx: mpsc::Receiver<()>) {
    log("worker thread ready");
    for () in rx {
        log(format!("worker: sleeping {STALL_SECS} s"));
        std::thread::sleep(Duration::from_secs(STALL_SECS));
        log("worker: awake");
    }
}

unsafe extern "system" fn sink_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        SINK_MSG => {
            SINK_RECEIVED.fetch_add(1, Ordering::Relaxed);
            let lat_us = ((qpc() - lp.0 as i64) * 1_000_000 / qpc_freq()).max(0) as u64;
            SINK_MAX_LAT_US.fetch_max(lat_us, Ordering::Relaxed);
            LRESULT(0)
        }
        SINK_DONE => {
            let received = SINK_RECEIVED.swap(0, Ordering::Relaxed);
            let lat = SINK_MAX_LAT_US.swap(0, Ordering::Relaxed);
            log(format!(
                "sink: burst done; posted={} post failures={} received={received} max post->receive latency={} us",
                wp.0, lp.0, lat
            ));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn sink_thread() {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(sink_proc),
            hInstance: hinstance(),
            lpszClassName: SINK_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            SINK_CLASS,
            w!("sink"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance()),
            None,
        )
        .expect("message-only window");
        SINK.store(hwnd.0 as isize, Ordering::SeqCst);
        log(format!("message-only sink window {:?} ready", hwnd.0));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

fn burst() {
    let sink = HWND(SINK.load(Ordering::SeqCst) as *mut c_void);
    log(format!("burst: posting {BURST_RATE} msg/s for {BURST_SECS} s to sink {:?}", sink.0));
    let start = Instant::now();
    let (mut posted, mut failed) = (0u64, 0u64);
    loop {
        let el = start.elapsed();
        if el >= Duration::from_secs(BURST_SECS) {
            break;
        }
        let target = el.as_micros() as u64 * BURST_RATE / 1_000_000;
        while posted < target {
            if unsafe { PostMessageW(Some(sink), SINK_MSG, WPARAM(0), LPARAM(qpc() as isize)) }.is_err() {
                failed += 1;
            }
            posted += 1;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let secs = start.elapsed().as_secs_f64();
    log(format!("burst: done, {posted} posted in {secs:.2} s ({:.0}/s), {failed} failed", posted as f64 / secs));
    unsafe {
        let _ = PostMessageW(Some(sink), SINK_DONE, WPARAM(posted as usize), LPARAM(failed as isize));
    }
}

// ---------------------------------------------------------------- roles

fn run(mode: Mode) {
    let path = std::env::temp_dir().join("taskbar-spike.log");
    *LOG.lock().unwrap() = std::fs::File::create(&path).ok();
    log(format!("spike run mode={mode:?}, log {}", path.display()));
    unsafe {
        log(format!(
            "process DPI awareness at start: {}",
            awareness(GetDpiAwarenessContextForProcess(HANDLE::default()))
        ));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || worker(rx));
        std::thread::spawn(sink_thread);
        std::thread::spawn(uia_thread);

        let inst = hinstance();
        let host_wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(host_proc),
            hInstance: inst,
            lpszClassName: HOST_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&host_wc);
        let widget_wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(widget_proc),
            hInstance: inst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: WIDGET_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&widget_wc);
        // Hidden top-level window: message-only windows do not get the TaskbarCreated broadcast.
        let host = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            HOST_CLASS,
            w!("BatterySight spike host"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(inst),
            None,
        )
        .expect("host window");
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_CREATED.store(taskbar_created, Ordering::Relaxed);
        log(format!("host {:?}, TaskbarCreated = 0x{taskbar_created:X}", host.0));
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                mode,
                host,
                widgets: Vec::new(),
                tray_added: false,
                light: light_theme(),
                worker: tx,
                recreate_started: None,
                place_x: None,
            })
        });
        with_state(|s| {
            s.tray_added = add_tray(host);
            tick(s);
        });
        SetTimer(Some(host), TIMER_TICK, TICK_MS, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        log("exit");
    }
}

/// `name` is a command, or `place:<screen x>` / `place-off`.
fn post_cmd(name: &str) -> bool {
    let (mut wp, mut lp) = (0usize, 0isize);
    let msg = match name {
        "stall" => CMD_STALL,
        "burst" => CMD_BURST,
        "stall-ui" => CMD_STALL_UI,
        "quit" => CMD_QUIT,
        "recreate" => CMD_RECREATE,
        "detach" => CMD_DETACH,
        "report" => CMD_REPORT,
        "place-off" => CMD_PLACE,
        _ if name.starts_with("place:") => {
            wp = name[6..].parse::<i32>().unwrap_or(0) as usize;
            lp = 1;
            CMD_PLACE
        }
        _ => {
            eprintln!("unknown cmd {name}");
            return false;
        }
    };
    unsafe {
        match FindWindowW(HOST_CLASS, PCWSTR::null()) {
            Ok(h) => PostMessageW(Some(h), msg, WPARAM(wp), LPARAM(lp)).is_ok(),
            Err(_) => {
                eprintln!("spike not running");
                false
            }
        }
    }
}

unsafe extern "system" fn monitor_cb(m: HMONITOR, _: HDC, _: *mut RECT, _: LPARAM) -> BOOL {
    unsafe {
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(m, &mut mi);
        let (mut dx, mut dy) = (0, 0);
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        println!(
            "monitor {:?}: rect {} work {} primary={} dpi={dx} ({}%)",
            m.0,
            fmt_rect(&mi.rcMonitor),
            fmt_rect(&mi.rcWork),
            mi.dwFlags & 1 != 0,
            dx * 100 / 96
        );
    }
    true.into()
}

unsafe extern "system" fn child_cb(h: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        let root = HWND(lp.0 as *mut c_void);
        let mut depth = 0;
        let mut p = h;
        while let Ok(q) = GetParent(p) {
            if q == root {
                break;
            }
            depth += 1;
            p = q;
        }
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(h, Some(&mut pid));
        println!(
            "  {}{:?} {} rect {} visible={} style=0x{:08X} ex=0x{:08X} pid={pid} tid={tid}",
            "  ".repeat(depth),
            h.0,
            class_name(h),
            fmt_rect(&window_rect(h)),
            IsWindowVisible(h).as_bool(),
            GetWindowLongW(h, GWL_STYLE) as u32,
            GetWindowLongW(h, GWL_EXSTYLE) as u32
        );
    }
    true.into()
}

fn autohide_state() -> u32 {
    unsafe {
        let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
        SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32
    }
}

/// Tries several style / parent combinations and reports which CreateWindowExW / SetParent calls work.
fn try_create() {
    unsafe {
        let inst = hinstance();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(widget_proc),
            hInstance: inst,
            lpszClassName: WIDGET_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let own = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WIDGET_CLASS,
            w!("own top-level"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(inst),
            None,
        )
        .unwrap_or_default();
        println!("own top-level window: {:?}", own.0);
        let mut parents: Vec<(String, HWND)> = vec![("own top-level".into(), own)];
        for (bar, _) in taskbars() {
            parents.push((class_name(bar), bar));
            if let Ok(b) = FindWindowExW(Some(bar), None, w!("Windows.UI.Composition.DesktopWindowContentBridge"), PCWSTR::null()) {
                parents.push((format!("{} > DesktopWindowContentBridge", class_name(bar)), b));
            }
            if let Ok(t) = FindWindowExW(Some(bar), None, w!("TrayNotifyWnd"), PCWSTR::null()) {
                parents.push((format!("{} > TrayNotifyWnd", class_name(bar)), t));
            }
        }
        let combos: [(&str, WINDOW_EX_STYLE, WINDOW_STYLE); 4] = [
            ("LAYERED|NOACTIVATE, CHILD|CLIPSIBLINGS", WS_EX_LAYERED | WS_EX_NOACTIVATE, WS_CHILD | WS_CLIPSIBLINGS),
            ("LAYERED, CHILD", WS_EX_LAYERED, WS_CHILD),
            ("0, CHILD", WINDOW_EX_STYLE(0), WS_CHILD),
            ("NOREDIRECTIONBITMAP, CHILD", WS_EX_NOREDIRECTIONBITMAP, WS_CHILD),
        ];
        for (pname, parent) in &parents {
            for (cname, ex, st) in combos {
                SetLastError(WIN32_ERROR(0));
                let r = CreateWindowExW(ex, WIDGET_CLASS, w!("t"), st, 0, 0, 10, 10, Some(*parent), None, Some(inst), None);
                let err = GetLastError();
                match r {
                    Ok(h) => {
                        println!("CreateWindowExW [{cname}] parent {pname}: OK {:?}", h.0);
                        let _ = DestroyWindow(h);
                    }
                    Err(e) => println!("CreateWindowExW [{cname}] parent {pname}: FAILED {e} (GetLastError {err:?})"),
                }
            }
            // Create as our own child, then re-parent cross-process.
            if *parent != own {
                let h = CreateWindowExW(WS_EX_LAYERED, WIDGET_CLASS, w!("t"), WS_CHILD, 0, 0, 10, 10, Some(own), None, Some(inst), None);
                if let Ok(h) = h {
                    SetLastError(WIN32_ERROR(0));
                    let r = SetParent(h, Some(*parent));
                    let err = GetLastError();
                    println!(
                        "SetParent(child of own -> {pname}): {:?} (GetLastError {err:?}); parent now {}",
                        r.map(|p| p.0),
                        GetParent(h).map(class_name).unwrap_or_else(|_| "-".into())
                    );
                    let _ = DestroyWindow(h);
                }
                let h = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOOLWINDOW, WIDGET_CLASS, w!("t"), WS_POPUP, 0, 0, 10, 10, None, None, Some(inst), None);
                if let Ok(h) = h {
                    SetWindowLongW(h, GWL_STYLE, (WS_CHILD | WS_CLIPSIBLINGS).0 as i32);
                    SetLastError(WIN32_ERROR(0));
                    let r = SetParent(h, Some(*parent));
                    let err = GetLastError();
                    println!(
                        "SetParent(popup restyled WS_CHILD -> {pname}): {:?} (GetLastError {err:?}); parent now {}",
                        r.map(|p| p.0),
                        GetParent(h).map(class_name).unwrap_or_else(|_| "-".into())
                    );
                    let _ = DestroyWindow(h);
                }
            }
        }
        let _ = DestroyWindow(own);
    }
}

fn probe() {
    unsafe {
        println!("probe: process awareness {}", awareness(GetDpiAwarenessContextForProcess(HANDLE::default())));
        let _ = EnumDisplayMonitors(None, None, Some(monitor_cb), LPARAM(0));
        println!("autohide state (ABM_GETSTATE) = {} (ABS_AUTOHIDE=1)", autohide_state());
        println!("SystemUsesLightTheme = {}", light_theme());
        for (bar, primary) in taskbars() {
            let mut pid = 0;
            let tid = GetWindowThreadProcessId(bar, Some(&mut pid));
            println!(
                "taskbar {:?} {} primary={primary} rect {} client {} dpi={} visible={} ex=0x{:08X} pid={pid} tid={tid}",
                bar.0,
                class_name(bar),
                fmt_rect(&window_rect(bar)),
                fmt_rect(&client_rect(bar)),
                GetDpiForWindow(bar),
                IsWindowVisible(bar).as_bool(),
                GetWindowLongW(bar, GWL_EXSTYLE) as u32
            );
            let _ = EnumChildWindows(Some(bar), Some(child_cb), LPARAM(bar.0 as isize));
            if let Some(a) = anchor_x(bar, primary) {
                println!("  anchor screen x = {a}");
            }
            if let Ok(wd) = FindWindowExW(Some(bar), None, WIDGET_CLASS, PCWSTR::null()) {
                let r = window_rect(wd);
                let br = window_rect(bar);
                println!(
                    "  OUR WIDGET {:?}: rect {} inside bar={} gap to anchor={:?} above-sibling={} awareness={}",
                    wd.0,
                    fmt_rect(&r),
                    r.left >= br.left && r.right <= br.right && r.top >= br.top && r.bottom <= br.bottom,
                    anchor_x(bar, primary).map(|a| a - r.right),
                    GetWindow(wd, GW_HWNDPREV).map(class_name).unwrap_or_else(|_| "-".into()),
                    awareness(GetWindowDpiAwarenessContext(wd))
                );
            }
        }
        let mut after: Option<HWND> = None;
        while let Ok(o) = FindWindowExW(None, after, WIDGET_CLASS, PCWSTR::null()) {
            println!("top-level widget {:?} rect {}", o.0, fmt_rect(&window_rect(o)));
            after = Some(o);
        }
    }
}

fn measure(secs: f64, cmd: Option<String>, delay_ms: u64) {
    let bar = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }.expect("Shell_TrayWnd");
    if let Some(c) = cmd.clone() {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            println!("measure: posting cmd {c} at t={delay_ms} ms -> {}", post_cmd(&c));
        });
    }
    let start = Instant::now();
    let mut samples: Vec<(f64, f64, bool, bool)> = Vec::new();
    while start.elapsed().as_secs_f64() < secs {
        let t = start.elapsed().as_secs_f64();
        let t0 = Instant::now();
        let mut res = 0usize;
        let ok = unsafe { SendMessageTimeoutW(bar, WM_NULL, WPARAM(0), LPARAM(0), SMTO_NORMAL, 5000, Some(&mut res)) }.0 != 0;
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        let hung = unsafe { IsHungAppWindow(bar) }.as_bool();
        samples.push((t, dt, ok, hung));
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut d: Vec<f64> = samples.iter().map(|s| s.1).collect();
    d.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| d[((d.len() - 1) as f64 * p) as usize];
    println!(
        "measure: {} samples over {secs} s (cmd {:?}): WM_NULL round trip median {:.3} ms, p99 {:.3} ms, max {:.3} ms; \
         >50 ms: {}; failed/timeouts: {}; IsHungAppWindow true: {}",
        d.len(),
        cmd,
        pct(0.5),
        pct(0.99),
        d.last().copied().unwrap_or(0.0),
        samples.iter().filter(|s| s.1 > 50.0).count(),
        samples.iter().filter(|s| !s.2).count(),
        samples.iter().filter(|s| s.3).count()
    );
    let buckets = secs.ceil() as usize;
    let line: Vec<String> = (0..buckets)
        .map(|b| {
            let m = samples.iter().filter(|s| s.0 as usize == b).map(|s| s.1).fold(0.0, f64::max);
            format!("{m:.2}")
        })
        .collect();
    println!("measure: max round trip per second (ms): {}", line.join(" "));
}

fn set_autohide(on: bool) {
    unsafe {
        let bar = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).unwrap_or_default();
        let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, hWnd: bar, ..Default::default() };
        let cur = SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32;
        let new = if on { cur | ABS_AUTOHIDE } else { cur & !ABS_AUTOHIDE };
        abd.lParam = LPARAM(new as isize);
        SHAppBarMessage(ABM_SETSTATE, &mut abd);
        println!("autohide: state {cur} -> {new}; now {}", autohide_state());
    }
}

/// Screen pixels (BGRA, top-down) in physical coordinates.
fn capture(x: i32, y: i32, w: i32, h: i32) -> Vec<u8> {
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).expect("dib");
        let old = SelectObject(dc, bmp.into());
        BitBlt(dc, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT).expect("BitBlt");
        let _ = GdiFlush();
        let data = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        data
    }
}

fn save_bmp(w: i32, h: i32, data: &[u8], out: &str) {
    let mut f = std::fs::File::create(out).expect("create");
    let file_size = 14 + 40 + data.len() as u32;
    let mut hdr = Vec::new();
    hdr.extend_from_slice(b"BM");
    hdr.extend_from_slice(&file_size.to_le_bytes());
    hdr.extend_from_slice(&0u32.to_le_bytes());
    hdr.extend_from_slice(&54u32.to_le_bytes());
    hdr.extend_from_slice(&40u32.to_le_bytes());
    hdr.extend_from_slice(&w.to_le_bytes());
    hdr.extend_from_slice(&(-h).to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes());
    hdr.extend_from_slice(&32u16.to_le_bytes());
    hdr.extend_from_slice(&[0u8; 24]);
    f.write_all(&hdr).unwrap();
    f.write_all(data).unwrap();
}

fn shot(x: i32, y: i32, w: i32, h: i32, out: &str) {
    save_bmp(w, h, &capture(x, y, w, h), out);
    println!("shot: {w}x{h} at ({x},{y}) -> {out}");
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as f64).sum::<f64>() / a.len().max(1) as f64
}

/// Does Explorer still process mouse input while the widget-owning thread is blocked?
/// Moves the cursor onto an opaque pixel of our widget (so a mouse message for our thread enters the
/// shared input queue), then onto a task button, and compares the button's pixels with an
/// un-hovered capture: a hover highlight means Explorer processed the input.
/// `cmd`: spike command to post first (stall-ui, stall, burst) or None. `touch`: pass over our widget
/// first. Restores the cursor.
fn hover_test(cmd: Option<&str>, touch: bool, click: bool, tag: &str) {
    let stall = cmd.is_some();
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants};
    unsafe {
        let bar = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).expect("taskbar");
        let br = window_rect(bar);
        let widget = FindWindowExW(Some(bar), None, WIDGET_CLASS, PCWSTR::null()).expect("spike widget on primary taskbar");
        let wr = window_rect(widget);
        // Brightest captured pixel inside the widget = an opaque text pixel (dark theme).
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let px = capture(wr.left, wr.top, ww, wh);
        let (mut best, mut bi) = (0u32, 0usize);
        for (i, p) in px.chunks_exact(4).enumerate() {
            let v = p[0] as u32 + p[1] as u32 + p[2] as u32;
            if v > best {
                best = v;
                bi = i;
            }
        }
        let opaque = POINT { x: wr.left + (bi as i32 % ww), y: wr.top + (bi as i32 / ww) };
        // Last task button via UI Automation (the first is often the active app, already highlighted).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).expect("uia");
        let all = uia.ElementFromHandle(bar).unwrap().FindAll(TreeScope_Descendants, &uia.CreateTrueCondition().unwrap()).unwrap();
        let mut button = RECT::default();
        for i in 0..all.Length().unwrap_or(0) {
            let el = all.GetElement(i).unwrap();
            let cls = el.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
            let r = el.CurrentBoundingRectangle().unwrap_or_default();
            if cls == "Taskbar.TaskListButtonAutomationPeer" && r.right > r.left && r.top >= br.top {
                button = r;
            }
        }
        assert!(button.right > button.left, "no task button found");
        let (bw, bh) = (button.right - button.left, button.bottom - button.top);
        let center = POINT { x: (button.left + button.right) / 2, y: (button.top + button.bottom) / 2 };
        let empty = POINT { x: br.left + 300, y: (br.top + br.bottom) / 2 };
        let mut saved = POINT::default();
        let _ = GetCursorPos(&mut saved);
        println!(
            "hover-test {tag}: cmd={cmd:?} touch={touch} click={click}; widget {} opaque pixel {:?}; task button {}; cursor saved at ({},{})",
            fmt_rect(&wr),
            (opaque.x, opaque.y),
            fmt_rect(&button),
            saved.x,
            saved.y
        );
        let dir = std::env::temp_dir().join("spikeshots");
        let _ = std::fs::create_dir_all(&dir);
        let save = |name: &str, d: &[u8]| save_bmp(bw, bh, d, dir.join(format!("{tag}-{name}.bmp")).to_str().unwrap());
        move_mouse(empty);
        std::thread::sleep(Duration::from_millis(800));
        let base = capture(button.left, button.top, bw, bh);
        save("0-base", &base);
        let t0 = Instant::now();
        if stall {
            assert!(post_cmd(cmd.unwrap()));
            std::thread::sleep(Duration::from_millis(300));
        }
        if touch || click {
            move_mouse(opaque);
            std::thread::sleep(Duration::from_millis(300));
        }
        if click {
            // A queued click for the stalled thread, as when the user clicks the widget.
            mouse_click();
            std::thread::sleep(Duration::from_millis(300));
        }
        move_mouse(center);
        std::thread::sleep(Duration::from_millis(1000));
        let during = capture(button.left, button.top, bw, bh);
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        save("1-hover", &during);
        let mut res = 0usize;
        let s0 = Instant::now();
        let ok = SendMessageTimeoutW(bar, WM_NULL, WPARAM(0), LPARAM(0), SMTO_NORMAL, 5000, Some(&mut res)).0 != 0;
        let null_ms = s0.elapsed().as_secs_f64() * 1000.0;
        let hung = IsHungAppWindow(bar).as_bool();
        println!(
            "  t={:.1}s hover capture: diff vs base {:.2}; cursor on button={} ({},{}); WM_NULL ok={ok} {null_ms:.2} ms; IsHungAppWindow={hung}",
            t0.elapsed().as_secs_f64(),
            mean_abs_diff(&base, &during),
            at == center,
            at.x,
            at.y
        );
        if stall {
            // Poll until the hover highlight appears (or 15 s pass), to time when input resumes.
            let mut seen = None;
            while t0.elapsed() < Duration::from_secs(15) {
                let _ = GetCursorPos(&mut at);
                if at != center {
                    println!("  cursor moved by someone else at t={:.1}s; polling stopped", t0.elapsed().as_secs_f64());
                    break;
                }
                let now = capture(button.left, button.top, bw, bh);
                if mean_abs_diff(&base, &now) > 1.0 {
                    seen = Some(t0.elapsed().as_secs_f64());
                    save("2-later", &now);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            println!("  hover highlight first seen at t={seen:?} s ({cmd:?} posted at t=0, lasts {STALL_SECS} s)");
            while t0.elapsed() < Duration::from_secs(STALL_SECS + 1) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        move_mouse(empty);
        std::thread::sleep(Duration::from_millis(300));
        move_mouse(saved);
        println!("  cursor restored");
    }
}

/// Injected left click at the current cursor position.
fn mouse_click() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, SendInput,
    };
    let ev = |f| INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: f, ..Default::default() } } };
    unsafe {
        SendInput(&[ev(MOUSEEVENTF_LEFTDOWN), ev(MOUSEEVENTF_LEFTUP)], size_of::<INPUT>() as i32);
    }
}

/// Real (injected) mouse movement; SetCursorPos alone does not make the XAML taskbar show hover.
fn move_mouse(p: POINT) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
    };
    unsafe {
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: ((p.x - vx) as i64 * 65535 / (vw - 1) as i64) as i32,
                    dy: ((p.y - vy) as i64 * 65535 / (vh - 1) as i64) as i32,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    ..Default::default()
                },
            },
        };
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}

/// Shows a small window of our own in the middle of the primary monitor and makes it foreground for
/// `secs` seconds, so an auto-hide taskbar that was active can slide away; then destroys it.
fn activate_temp(secs: u64) {
    unsafe {
        let inst = hinstance();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(widget_proc),
            hInstance: inst,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: w!("BatterySightSpikeTemp"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let h = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("BatterySightSpikeTemp"),
            w!("spike"),
            WS_POPUP | WS_VISIBLE,
            1180,
            620,
            200,
            100,
            None,
            None,
            Some(inst),
            None,
        )
        .expect("temp window");
        // Foreground lock: attach to the current foreground thread for the switch (test tool only).
        let fg_tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let _ = AttachThreadInput(me, fg_tid, true);
        let fg = SetForegroundWindow(h).as_bool();
        let _ = AttachThreadInput(me, fg_tid, false);
        println!("activate-temp: SetForegroundWindow={fg}; foreground is ours={}", GetForegroundWindow() == h);
        let end = Instant::now() + Duration::from_secs(secs);
        let mut msg = MSG::default();
        while Instant::now() < end {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = DestroyWindow(h);
        println!("activate-temp: done");
    }
}

fn main() {
    LazyLock::force(&START);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = |i: usize| args.get(i).map(String::as_str);
    let num = |i: usize| a(i).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
    match a(0) {
        None | Some("run") => run(if a(1) == Some("overlay") { Mode::Overlay } else { Mode::Child }),
        Some("cmd") => {
            if !post_cmd(a(1).unwrap_or("")) {
                std::process::exit(1);
            }
        }
        Some("hover-test") => {
            hover_test(a(1).filter(|c| *c != "none"), a(2) == Some("touch"), a(2) == Some("click"), a(3).unwrap_or("hover"))
        }
        Some("activate-temp") => activate_temp(a(1).and_then(|s| s.parse().ok()).unwrap_or(5)),
        Some("trycreate") => try_create(),
        Some("probe") => probe(),
        Some("measure") => measure(
            a(1).and_then(|s| s.parse().ok()).unwrap_or(5.0),
            a(2).map(String::from),
            a(3).and_then(|s| s.parse().ok()).unwrap_or(1000),
        ),
        Some("autohide") => match a(1) {
            Some("on") => set_autohide(true),
            Some("off") => set_autohide(false),
            _ => println!("autohide state = {}", autohide_state()),
        },
        Some("theme-broadcast") => unsafe {
            let s = wide("ImmersiveColorSet");
            let mut r = 0usize;
            let ok = SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                WPARAM(0),
                LPARAM(s.as_ptr() as isize),
                SMTO_ABORTIFHUNG,
                5000,
                Some(&mut r),
            );
            println!("broadcast WM_SETTINGCHANGE ImmersiveColorSet -> {}", ok.0);
        },
        Some("cursor") => unsafe {
            if args.len() >= 3 {
                move_mouse(POINT { x: num(1), y: num(2) });
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            println!("cursor {} {}", p.x, p.y);
        },
        Some("shot") => shot(num(1), num(2), num(3), num(4), a(5).unwrap_or("shot.bmp")),
        Some(other) => {
            eprintln!("unknown role {other}");
            std::process::exit(2);
        }
    }
}
