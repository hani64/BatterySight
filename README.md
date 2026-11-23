# BatterySight

BatterySight is a free, open-source (GPLv3) Windows app that shows the battery level of wireless mice, keyboards, headsets and controllers on the taskbar. It never keeps a device awake: it listens for what devices push and asks only when a device is provably awake because you are using it. Every supported device entry carries a verification status and a source.

## Privacy

BatterySight collects no telemetry. Its only network call is the update check (arriving in milestone 4).

## Building

```
cargo build --release
```

The binary is written to `target/release/batterysight.exe`.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
