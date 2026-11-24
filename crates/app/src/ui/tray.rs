//! Tray-icon fallback: one notification-area icon per device, or one placeholder icon when
//! there is none, so the menu stays reachable.
//!
//! Runs on the UI thread. `Shell_NotifyIconW` is one of the bounded Explorer calls the spike
//! allows on that thread (up to 32 ms during an Explorer restart).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON, SM_CXSMICON};

use super::render;
use super::{DeviceView, WM_APP_TRAY};

/// Tooltip of the placeholder icon.
const NO_DEVICES: &str = "BatterySight — no devices";

/// The icons this process shows, in view order; icon `i` has id `i + 1`.
pub struct Tray {
    host: HWND,
    icons: Vec<Icon>,
}

struct Icon {
    /// What the icon shows; `None` for the placeholder.
    view: Option<DeviceView>,
    light: bool,
    handle: HICON,
    /// Whether the current Explorer has the icon.
    added: bool,
}

impl Tray {
    /// No icons yet; [`Tray::show`] adds them.
    pub fn new(host: HWND) -> Tray {
        Tray {
            host,
            icons: Vec::new(),
        }
    }

    /// Makes the icons match `views`: changed icons are redrawn, missing ones added,
    /// surplus ones removed.
    pub fn show(&mut self, views: &[DeviceView], light: bool) {
        let wanted: Vec<Option<&DeviceView>> = if views.is_empty() {
            vec![None]
        } else {
            views.iter().map(Some).collect()
        };
        while self.icons.len() > wanted.len() {
            let id = self.icons.len() as u32;
            if let Some(icon) = self.icons.pop() {
                if icon.added {
                    self.notify(NIM_DELETE, id, None);
                }
                destroy(icon.handle);
            }
        }
        for (i, view) in wanted.into_iter().enumerate() {
            let id = i as u32 + 1;
            let current = self.icons.get(i);
            let same = current.is_some_and(|c| c.view.as_ref() == view && c.light == light);
            if same && current.is_some_and(|c| c.added) {
                continue;
            }
            if !same {
                let Some(handle) = render::tray_icon(view, icon_size(), light) else {
                    continue;
                };
                let icon = Icon {
                    view: view.cloned(),
                    light,
                    handle,
                    added: current.is_some_and(|c| c.added),
                };
                match self.icons.get_mut(i) {
                    Some(old) => destroy(std::mem::replace(old, icon).handle),
                    None => self.icons.push(icon),
                }
            }
            let icon = &self.icons[i];
            let tip = icon
                .view
                .as_ref()
                .map_or(NO_DEVICES, |v| v.tooltip.as_str());
            let added = if icon.added {
                self.notify(NIM_MODIFY, id, Some((icon.handle, tip)))
            } else {
                self.notify(NIM_ADD, id, Some((icon.handle, tip)))
                    && self.notify(NIM_SETVERSION, id, None)
            };
            self.icons[i].added = added;
        }
    }

    /// After `TaskbarCreated`: the new Explorer has none of our icons.
    pub fn readd(&mut self, views: &[DeviceView], light: bool) {
        for icon in &mut self.icons {
            icon.added = false;
        }
        self.show(views, light);
    }

    /// Removes every icon.
    pub fn remove(&mut self) {
        for (i, icon) in std::mem::take(&mut self.icons).into_iter().enumerate() {
            if icon.added {
                self.notify(NIM_DELETE, i as u32 + 1, None);
            }
            destroy(icon.handle);
        }
    }

    fn notify(
        &self,
        action: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE,
        id: u32,
        content: Option<(HICON, &str)>,
    ) -> bool {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.host,
            uID: id,
            ..Default::default()
        };
        if let Some((icon, tip)) = content {
            data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_MESSAGE;
            data.uCallbackMessage = WM_APP_TRAY;
            data.hIcon = icon;
            // Truncated to the buffer, keeping the terminating NUL.
            let room = data.szTip.len() - 1;
            for (d, c) in data.szTip[..room].iter_mut().zip(tip.encode_utf16()) {
                *d = c;
            }
        }
        if action == NIM_SETVERSION {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        }
        // SAFETY: `data` outlives the call.
        unsafe { Shell_NotifyIconW(action, &data) }.as_bool()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Small-icon size at the system DPI, the DPI the notification area uses.
fn icon_size() -> i32 {
    // SAFETY: no pointers involved.
    unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16)
}

fn destroy(icon: HICON) {
    // SAFETY: the icon was created by `render::tray_icon` and is no longer shown.
    let _ = unsafe { DestroyIcon(icon) };
}
