# Contributing to BatterySight

Thanks for helping. Before opening an issue, search the existing ones; a bug report without the details the form asks for may be closed. Security problems go through [SECURITY.md](SECURITY.md), not a public issue.

## Scope

- Windows 10 and 11, x64. Device logic lives in `crates/core` and must not depend on Windows APIs; Windows I/O lives in `crates/win`, the UI in `crates/app`.
- A read-only battery app: no device settings (DPI, lighting, EQ, sidetone).
- No telemetry and no network access.
- One portable exe; no installer, no runtime, no plugins.

## Settled decisions

These were decided on purpose. Issues and PRs that reopen them may be closed with a link here.

### Never keep a device awake

BatterySight listens for the battery reports devices send on their own and asks a device only on the events in the README's [When BatterySight asks a device](README.md#when-batterysight-asks-a-device). No periodic queries to an idle device, no "retry every device" paths, no query when the menu opens. Only the scheduler decides when anything is sent; engines declare what they can do.

### Devices are data, protocols are engines

A device is a catalog entry (`crates/core/catalog/<vendor>.toml`); a protocol is an engine (`crates/core/src/engines/`). No per-vendor drivers that pick their own timing, and no runtime-loaded scripts or device definitions.

### Every device entry carries a status and a source

Devices nobody has verified are not added, not even behind an opt-in.

### No keystroke contents

Nothing reads what is typed or how the mouse moves; activity detection, where needed, looks at report headers only.

### Taskbar widget or tray icons

The widget sits inside the taskbar; where it cannot, the app uses tray icons. No floating overlay.

## Adding or fixing a device

Catalog entries take `name`, `vid`, `pids` (USB product ids) and/or `wpids` (wireless ids a receiver reports), `kind`, `engine`, `status`, `source` and optional `quirks`; the comment at the top of [`logitech.toml`](crates/core/catalog/logitech.toml) explains the ids.

| `status` | Meaning | What `source` must hold |
|---|---|---|
| `hardware-tested` | Tested on the hardware by the project or a named tester | Who tested what, and when |
| `source-verified` | Protocol confirmed by an independent public source, not yet tested on hardware | The source and file/line (Linux HID drivers, Solaar, HeadsetControl, OpenRazer, rivalcfg, vendor docs) |
| `community-tested` | Reported tested on hardware by someone outside the project, not independently confirmed | A link to that report |

A change to an engine's behaviour needs a replay fixture in `crates/core/tests/fixtures/<engine>/` and a test that replays it (see `crates/core/tests/hidpp_replay.rs`).

## Capturing device traffic

1. Set the environment variable `BATTERYSIGHT_TRACE=1`, start BatterySight, reproduce the problem, then exit.
2. The trace is `%LOCALAPPDATA%\BatterySight\sends.log`.
3. Before sharing it, check it holds no user name or local path. Device ids (unit ids, serial numbers, interface paths, audio container ids) can stay: they help debugging.

If `BATTERYSIGHT_TRACE` is set for your whole user account, `cargo test` appends to the same `sends.log`; unset it while running tests.

## Pull requests

- One change per PR. PRs are squash-merged into a linear history.
- Tests for anything that can regress; README changes for anything a user sees, in the same PR.
- Dependencies come from crates.io only; cargo-deny checks their sources ([`deny.toml`](deny.toml)).
- AI-assisted changes follow the same rules: you must understand every line you submit and answer for it. Do not add AI co-author trailers (`Co-authored-by: Claude …`, `… Copilot …`); CI rejects them.
- No personal data in the tree: no user names or local paths. CI rejects personal paths.

## Checks

CI runs these on Windows; run them before pushing:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo doc --workspace --no-deps --locked
```

CI runs `cargo doc` with `RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links"`; in PowerShell set `$env:RUSTDOCFLAGS` to that value first. With [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) installed, `cargo deny check bans sources advisories` runs the dependency checks too. The toolchain is pinned in `rust-toolchain.toml`; see the README's [Building](README.md#building) section for MSVC and GNU setups.
