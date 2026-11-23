// Embeds the application manifest: Windows 7-11 supportedOS (WS_EX_LAYERED child windows need
// the Windows 8+ entry), PerMonitorV2 DPI awareness, Common Controls v6, asInvoker.
fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("BatterySight.TaskbarSpike"))
            .expect("unable to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
