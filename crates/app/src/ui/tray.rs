//! Tray-icon fallback: one notification-area icon per device, or one placeholder icon when
//! there is none, so the menu stays reachable.
//!
//! Runs on the UI thread. `Shell_NotifyIconW` is one of the bounded Explorer calls allowed
//! on that thread (measured at up to 32 ms during an Explorer restart).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON, SM_CXSMICON};

use super::render;
use super::{DeviceView, WM_APP_TRAY};

/// Tooltip of the placeholder icon.
const NO_DEVICES: &str = "BatterySight — no devices";

/// The icons this process shows, in view order; icon `i` has id `i + 1`.
pub struct Tray {
    shell: NotifyArea,
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
            shell: NotifyArea { host },
            icons: Vec::new(),
        }
    }

    /// Makes the icons match `views` (see [`sync`]).
    pub fn show(&mut self, views: &[DeviceView], light: bool) {
        sync(&mut self.icons, &mut self.shell, views, light);
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
        remove_all(&mut self.icons, &mut self.shell);
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

/// What [`sync`] asks of the notification area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Add the icon (and set its version).
    Add,
    Modify,
    Delete,
}

/// Drawing and showing icons; [`NotifyArea`] in the app.
trait Shell {
    fn render(&mut self, view: Option<&DeviceView>, light: bool) -> Option<HICON>;
    fn notify(&mut self, action: Action, id: u32, content: Option<(HICON, &str)>) -> bool;
    fn destroy(&mut self, icon: HICON);
}

/// Makes `icons` match `views`: changed icons are redrawn, missing ones added, surplus ones
/// removed.
fn sync(icons: &mut Vec<Icon>, shell: &mut impl Shell, views: &[DeviceView], light: bool) {
    let wanted: Vec<Option<&DeviceView>> = if views.is_empty() {
        vec![None]
    } else {
        views.iter().map(Some).collect()
    };
    while icons.len() > wanted.len() {
        let id = icons.len() as u32;
        if let Some(icon) = icons.pop() {
            if icon.added {
                shell.notify(Action::Delete, id, None);
            }
            shell.destroy(icon.handle);
        }
    }
    for (i, view) in wanted.into_iter().enumerate() {
        let id = i as u32 + 1;
        let current = icons.get(i);
        let same = current.is_some_and(|c| c.view.as_ref() == view && c.light == light);
        if same && current.is_some_and(|c| c.added) {
            continue;
        }
        if !same {
            // Icon `i` must sit at index `i`: one that cannot be drawn ends the pass, so no
            // later icon takes its place (the next change tries again).
            let Some(handle) = shell.render(view, light) else {
                break;
            };
            let icon = Icon {
                view: view.cloned(),
                light,
                handle,
                added: current.is_some_and(|c| c.added),
            };
            match icons.get_mut(i) {
                Some(old) => shell.destroy(std::mem::replace(old, icon).handle),
                None => icons.push(icon),
            }
        }
        let Some(icon) = icons.get_mut(i) else {
            break;
        };
        let tip = icon
            .view
            .as_ref()
            .map_or(NO_DEVICES, |v| v.tooltip.as_str());
        let action = if icon.added {
            Action::Modify
        } else {
            Action::Add
        };
        icon.added = shell.notify(action, id, Some((icon.handle, tip)));
    }
}

fn remove_all(icons: &mut Vec<Icon>, shell: &mut impl Shell) {
    for (i, icon) in std::mem::take(icons).into_iter().enumerate() {
        if icon.added {
            shell.notify(Action::Delete, i as u32 + 1, None);
        }
        shell.destroy(icon.handle);
    }
}

/// The real notification area, with icons for `host`.
struct NotifyArea {
    host: HWND,
}

impl Shell for NotifyArea {
    fn render(&mut self, view: Option<&DeviceView>, light: bool) -> Option<HICON> {
        render::tray_icon(view, icon_size(), light)
    }

    fn notify(&mut self, action: Action, id: u32, content: Option<(HICON, &str)>) -> bool {
        match action {
            Action::Add => {
                self.shell_notify(NIM_ADD, id, content)
                    && self.shell_notify(NIM_SETVERSION, id, None)
            }
            Action::Modify => self.shell_notify(NIM_MODIFY, id, content),
            Action::Delete => self.shell_notify(NIM_DELETE, id, None),
        }
    }

    fn destroy(&mut self, icon: HICON) {
        // SAFETY: the icon was created by `render::tray_icon` and is no longer shown.
        let _ = unsafe { DestroyIcon(icon) };
    }
}

impl NotifyArea {
    fn shell_notify(
        &self,
        action: NOTIFY_ICON_MESSAGE,
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

/// Small-icon size at the system DPI, the DPI the notification area uses.
fn icon_size() -> i32 {
    // SAFETY: no pointers involved.
    unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    /// Records calls; icons are numbered handles; `fail` lists views that cannot be drawn.
    #[derive(Default)]
    struct FakeShell {
        fail: Vec<String>,
        next: usize,
        calls: Vec<(Action, u32, Option<String>)>,
        destroyed: Vec<usize>,
    }

    impl Shell for FakeShell {
        fn render(&mut self, view: Option<&DeviceView>, _light: bool) -> Option<HICON> {
            if view.is_some_and(|v| self.fail.contains(&v.text)) {
                return None;
            }
            self.next += 1;
            Some(HICON(self.next as *mut c_void))
        }

        fn notify(&mut self, action: Action, id: u32, content: Option<(HICON, &str)>) -> bool {
            self.calls
                .push((action, id, content.map(|(_, tip)| tip.to_owned())));
            true
        }

        fn destroy(&mut self, icon: HICON) {
            self.destroyed.push(icon.0 as usize);
        }
    }

    fn view(text: &str) -> DeviceView {
        DeviceView {
            glyph: '\u{E962}',
            text: text.into(),
            dimmed: false,
            charging: false,
            low: false,
            tooltip: format!("M — {text}"),
        }
    }

    #[test]
    fn adds_one_icon_per_view_then_removes_surplus() {
        let mut shell = FakeShell::default();
        let mut icons = Vec::new();
        sync(&mut icons, &mut shell, &[view("10%"), view("20%")], false);
        sync(&mut icons, &mut shell, &[view("10%")], false);
        assert_eq!(
            shell.calls,
            [
                (Action::Add, 1, Some("M — 10%".into())),
                (Action::Add, 2, Some("M — 20%".into())),
                (Action::Delete, 2, None),
            ]
        );
        assert_eq!(shell.destroyed, [2]);
        assert_eq!(icons.len(), 1);
    }

    #[test]
    fn no_views_show_the_placeholder() {
        let mut shell = FakeShell::default();
        let mut icons = Vec::new();
        sync(&mut icons, &mut shell, &[], false);
        assert_eq!(shell.calls, [(Action::Add, 1, Some(NO_DEVICES.into()))]);
    }

    #[test]
    fn a_view_that_cannot_be_drawn_ends_the_pass() {
        let mut shell = FakeShell {
            fail: vec!["10%".into()],
            ..FakeShell::default()
        };
        let mut icons = Vec::new();
        sync(&mut icons, &mut shell, &[view("10%"), view("20%")], false);
        assert!(icons.is_empty());
        assert!(shell.calls.is_empty());
        // Next pass, drawable again: both icons, at their own ids.
        shell.fail.clear();
        sync(&mut icons, &mut shell, &[view("10%"), view("20%")], false);
        assert_eq!(shell.calls.iter().map(|c| c.1).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn changed_view_is_redrawn_and_modified() {
        let mut shell = FakeShell::default();
        let mut icons = Vec::new();
        sync(&mut icons, &mut shell, &[view("10%")], false);
        sync(&mut icons, &mut shell, &[view("10%")], false);
        sync(&mut icons, &mut shell, &[view("11%")], false);
        assert_eq!(
            shell.calls,
            [
                (Action::Add, 1, Some("M — 10%".into())),
                (Action::Modify, 1, Some("M — 11%".into())),
            ]
        );
        assert_eq!(shell.destroyed, [1]);
    }
}
