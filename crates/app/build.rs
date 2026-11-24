//! Embeds the application manifest: supportedOS Windows 7 to 11 (a layered child window of
//! the taskbar needs the Windows 8+ entries; without them
//! `CreateWindowExW(WS_EX_LAYERED, …, WS_CHILD)` fails with last error 0; `WS_EX_LAYERED`
//! child windows exist since Windows 8, see
//! https://learn.microsoft.com/windows/win32/winmsg/extended-window-styles), PerMonitorV2 DPI
//! awareness, Common Controls v6 and asInvoker, all defaults of `new_manifest`.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("BatterySight"))
            .expect("unable to embed the application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
