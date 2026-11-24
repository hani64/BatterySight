//! Tray-icon fallback: one notification-area icon per device, or one placeholder icon when
//! there is none, so the menu stays reachable.
//!
//! Runs on the UI thread. `Shell_NotifyIconW` is one of the bounded Explorer calls allowed
//! on that thread (measured at up to 32 ms during an Explorer restart).

use batterysight_core::model::DeviceKey;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
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

    /// The id of the icon that shows the device `key`, if the notification area has it.
    pub fn icon_id(&self, key: &DeviceKey) -> Option<u32> {
        icon_id(&self.icons, key)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

/// What [`sync`] asks of the notification area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// Add the icon (and set its version).
    Add,
    Modify,
    /// Change the tooltip only (the icon is unchanged).
    Tip,
    Delete,
}

/// Drawing and showing icons; [`NotifyArea`] in the app.
pub(super) trait Shell {
    fn render(&mut self, view: Option<&DeviceView>, light: bool) -> Option<HICON>;
    fn notify(&mut self, action: Action, id: u32, content: Option<(HICON, &str)>) -> bool;
    /// Shows a warning balloon (a toast on Windows 10 and 11) from icon `id`.
    fn balloon(&mut self, id: u32, title: &str, text: &str) -> bool;
    fn destroy(&mut self, icon: HICON);
}

/// Makes `icons` match `views`: changed icons are redrawn, missing ones added, surplus ones
/// removed; an icon whose tooltip alone changed gets the new tooltip only.
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
        let redraw =
            !current.is_some_and(|c| c.light == light && looks_alike(c.view.as_ref(), view));
        if redraw {
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
        if !redraw {
            icon.view = view.cloned();
        }
        let tip = icon
            .view
            .as_ref()
            .map_or(NO_DEVICES, |v| v.tooltip.as_str());
        let action = match (icon.added, redraw) {
            (false, _) => Action::Add,
            (true, true) => Action::Modify,
            (true, false) => Action::Tip,
        };
        icon.added = shell.notify(action, id, Some((icon.handle, tip)));
    }
}

/// Whether two icons' views draw alike (their tooltips aside); both `None` is the
/// placeholder.
fn looks_alike(a: Option<&DeviceView>, b: Option<&DeviceView>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.looks_like(b),
        (None, None) => true,
        _ => false,
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

fn icon_id(icons: &[Icon], key: &DeviceKey) -> Option<u32> {
    let at = icons
        .iter()
        .position(|i| i.added && i.view.as_ref().is_some_and(|v| &v.key == key))?;
    Some(at as u32 + 1)
}

/// The real notification area, with icons for `host`.
pub(super) struct NotifyArea {
    pub(super) host: HWND,
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
            Action::Tip => self.shell_notify_tip(id, content.map_or("", |(_, tip)| tip)),
            Action::Delete => self.shell_notify(NIM_DELETE, id, None),
        }
    }

    fn balloon(&mut self, id: u32, title: &str, text: &str) -> bool {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.host,
            uID: id,
            // NIF_SHOWTIP again: a call without it turns the icon's tooltip off.
            uFlags: NIF_INFO | NIF_SHOWTIP,
            dwInfoFlags: NIIF_WARNING,
            ..Default::default()
        };
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, text);
        // SAFETY: `data` outlives the call.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
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
            set_tip(&mut data.szTip, tip);
        }
        if action == NIM_SETVERSION {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        }
        // SAFETY: `data` outlives the call.
        unsafe { Shell_NotifyIconW(action, &data) }.as_bool()
    }

    /// Changes icon `id`'s tooltip only.
    fn shell_notify_tip(&self, id: u32, tip: &str) -> bool {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.host,
            uID: id,
            // NIF_SHOWTIP again: a call without it turns the icon's tooltip off.
            uFlags: NIF_TIP | NIF_SHOWTIP,
            ..Default::default()
        };
        set_tip(&mut data.szTip, tip);
        // SAFETY: `data` outlives the call.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
    }
}

/// Copies `tip` into `szTip`, cut to fit ([`fit_tip`]).
fn set_tip(dst: &mut [u16], tip: &str) {
    let room = dst.len().saturating_sub(1);
    copy_wide(dst, &fit_tip(tip, room));
}

/// Copies `s` into the fixed buffer `dst`, truncated to keep its terminating NUL.
fn copy_wide(dst: &mut [u16], s: &str) {
    let room = dst.len().saturating_sub(1);
    for (d, c) in dst[..room].iter_mut().zip(s.encode_utf16()) {
        *d = c;
    }
}

/// `tip` in at most `room` UTF-16 units: a tooltip too long has the device's name cut (it
/// ends in "…"), so its level after the name's " · " and the line below, with the time
/// left, stay.
fn fit_tip(tip: &str, room: usize) -> String {
    if tip.encode_utf16().count() <= room {
        return tip.to_owned();
    }
    let first = tip.find('\n').unwrap_or(tip.len());
    let Some(name_end) = tip[..first].rfind(" · ") else {
        return tip.to_owned();
    };
    let (name, kept) = tip.split_at(name_end);
    // The name's room: everything but what is kept and the "…".
    let Some(name_room) = room.checked_sub(kept.encode_utf16().count() + 1) else {
        return tip.to_owned();
    };
    let mut out = String::with_capacity(tip.len());
    let mut used = 0;
    for c in name.chars() {
        used += c.len_utf16();
        if used > name_room {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out.push_str(kept);
    out
}

/// Small-icon size at the system DPI, the DPI the notification area uses.
fn icon_size() -> i32 {
    // SAFETY: no pointers involved.
    unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::ffi::c_void;

    use batterysight_core::model::{
        Charge, DeviceKind, DeviceState, Level, Presence, Source, Verification, Via,
    };
    use batterysight_core::settings::Settings;

    /// Records calls; icons are numbered handles; `fail` lists views that cannot be drawn;
    /// the notification area refuses every call for the icon ids in `refused`.
    #[derive(Default)]
    pub(in crate::ui) struct FakeShell {
        pub(in crate::ui) fail: Vec<String>,
        pub(in crate::ui) refused: Vec<u32>,
        pub(in crate::ui) next: usize,
        pub(in crate::ui) calls: Vec<(Action, u32, Option<String>)>,
        /// Balloons shown: icon id and text.
        pub(in crate::ui) balloons: Vec<(u32, String)>,
        pub(in crate::ui) destroyed: Vec<usize>,
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
            !self.refused.contains(&id)
        }

        fn balloon(&mut self, id: u32, _title: &str, text: &str) -> bool {
            if self.refused.contains(&id) {
                return false;
            }
            self.balloons.push((id, text.to_owned()));
            true
        }

        fn destroy(&mut self, icon: HICON) {
            self.destroyed.push(icon.0 as usize);
        }
    }

    fn view(text: &str) -> DeviceView {
        keyed("M", text)
    }

    /// A view of device `key` showing `text`.
    pub(in crate::ui) fn keyed(key: &str, text: &str) -> DeviceView {
        DeviceView {
            key: DeviceKey(key.into()),
            glyph: '\u{E962}',
            text: text.into(),
            dimmed: false,
            charging: false,
            full: false,
            low: false,
            caption: None,
            tooltip: format!("{key} — {text}"),
        }
    }

    #[test]
    fn icon_id_finds_the_shown_icon_of_a_device() {
        let mut shell = FakeShell::default();
        let mut icons = Vec::new();
        sync(
            &mut icons,
            &mut shell,
            &[keyed("A", "10%"), keyed("B", "20%")],
            false,
        );
        assert_eq!(icon_id(&icons, &DeviceKey("B".into())), Some(2));
        assert_eq!(icon_id(&icons, &DeviceKey("C".into())), None);
        // An icon the notification area does not have cannot carry a balloon.
        icons[0].added = false;
        assert_eq!(icon_id(&icons, &DeviceKey("A".into())), None);
    }

    #[test]
    fn all_hidden_shows_placeholder() {
        let state = |key: &str| DeviceState {
            key: DeviceKey(key.into()),
            name: key.into(),
            kind: DeviceKind::Mouse,
            via: Via::Receiver,
            level: Some(Level::Percent(50)),
            millivolts: None,
            charge: Charge::Discharging,
            presence: Presence::Online,
            updated_ms: 0,
            source: Source::Query,
            verification: Verification::HardwareTested,
        };
        let states = [state("A"), state("B")];
        let mut settings = Settings::default();
        settings.set_hidden(&DeviceKey("A".into()), true);
        settings.set_hidden(&DeviceKey("B".into()), true);
        let views = super::super::views(&states, &settings, None);
        // The widget draws its own placeholder for no views (`render::widget_strip`).
        assert!(views.is_empty());
        let mut shell = FakeShell::default();
        let mut icons = Vec::new();
        sync(&mut icons, &mut shell, &views, false);
        assert_eq!(shell.calls, [(Action::Add, 1, Some(NO_DEVICES.into()))]);
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

    #[test]
    fn a_new_tooltip_alone_changes_only_the_tip() {
        let mut shell = FakeShell::default();
        let mut icons = Vec::new();
        sync(&mut icons, &mut shell, &[view("10%")], false);
        let aged = DeviceView {
            tooltip: "M — 10%, later".into(),
            ..view("10%")
        };
        sync(&mut icons, &mut shell, std::slice::from_ref(&aged), false);
        assert_eq!(
            shell.calls,
            [
                (Action::Add, 1, Some("M — 10%".into())),
                (Action::Tip, 1, Some("M — 10%, later".into())),
            ]
        );
        // Not drawn again.
        assert_eq!(shell.next, 1);
        assert!(shell.destroyed.is_empty());
        assert_eq!(icons[0].view.as_ref(), Some(&aged));
        // Unchanged since: nothing to do.
        sync(&mut icons, &mut shell, &[aged], false);
        assert_eq!(shell.calls.len(), 2);
    }

    #[test]
    fn a_long_name_is_cut_to_keep_the_level_and_time_left() {
        let tip = |name: &str| format!("{name} · 80%\n3 min ago · about 9 h left");
        let rest = " · 80%\n3 min ago · about 9 h left".encode_utf16().count();
        // Fits: unchanged.
        assert_eq!(fit_tip(&tip("G305"), 127), tip("G305"));
        let long = "N".repeat(200);
        let fitted = fit_tip(&tip(&long), 127);
        assert_eq!(fitted.encode_utf16().count(), 127);
        assert_eq!(fitted, tip(&format!("{}…", "N".repeat(127 - rest - 1))));
        // A character outside the BMP (two units) is not split.
        let emoji = "😀".repeat(100);
        let fitted = fit_tip(&tip(&emoji), 127);
        assert!(fitted.encode_utf16().count() <= 127);
        assert!(fitted.starts_with("😀"));
        assert!(fitted.ends_with("… · 80%\n3 min ago · about 9 h left"));
    }
}
