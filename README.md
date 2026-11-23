# BatterySight

BatterySight is a free, open-source (GPLv3) Windows app that shows the battery level of wireless mice, keyboards, headsets and controllers on the taskbar. It replaces BatteryHub, which went offline on 2026-11-22. Unlike BatteryHub, it never keeps a device awake: it listens for what devices push and asks only when a device is provably awake because you are using it. Every supported device entry carries a verification status and a source.

## Privacy

BatterySight collects no telemetry. Its only network call is the update check (arriving in milestone 4).

## Building

```
cargo build --release
```

The binary is written to `target/release/batterysight.exe`.

With the MSVC toolchain (Visual Studio Build Tools installed) nothing else is needed. With the `x86_64-pc-windows-gnu` toolchain, the `windows` crate needs GNU binutils (`dlltool` and `as`): install them, for example with `winget install --scope user BrechtSanders.WinLibs.POSIX.UCRT`, and point rustc at that `dlltool.exe` in your user `~/.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-gnu]
rustflags = ["-C", "dlltool=<path to WinLibs>/mingw64/bin/dlltool.exe"]
```

## License

GPL-3.0-only. See [LICENSE](LICENSE).
