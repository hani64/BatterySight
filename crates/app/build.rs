//! Embeds the application manifest: supportedOS Windows 7 to 11 (a layered child window of
//! the taskbar needs the Windows 8+ entries), PerMonitorV2 DPI awareness, Common Controls v6
//! and asInvoker, all defaults of `new_manifest`. See docs/spike/taskbar-results.md.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("BatterySight"))
            .expect("unable to embed the application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
