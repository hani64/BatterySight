# Instructions for coding agents

[CONTRIBUTING.md](CONTRIBUTING.md) is the source of truth; this file is its short form.

## Layout

- `crates/core`: device logic (catalog, engines, hub, scheduler, history, settings). Portable: no Windows APIs.
- `crates/win`: Windows I/O (HID, audio sessions, power, notifications, files, trace).
- `crates/app`: the exe (taskbar widget, tray icons, menus, `--list`).

## Rules

- Never add a code path that queries an idle device. Only the scheduler decides when anything is sent; queries happen only on the events in README "How it reads batteries".
- No network access, no telemetry, never keystroke or movement contents.
- No blocking work on the UI thread. In `unsafe` code, check handle and COM lifetimes.
- An engine behaviour change needs a replay test (`crates/core/tests/hidpp_replay.rs` for HID++, `vendor_replay.rs` for the others).
- PR titles: `area: lowercase summary` (CONTRIBUTING "Pull requests").
- Catalog entries need a `status` and a `source` (CONTRIBUTING "Adding or fixing a device").
- Never commit user names or local paths. Device ids (unit ids, serial numbers) in captures and fixtures are fine.
- No AI co-author trailers in commit messages.

## Done means

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked` and `cargo doc --workspace --no-deps --locked` (with `RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links"`) pass.
- README updated for anything a user sees.

## Releases (maintainer)

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo build` so `Cargo.lock` picks it up, and commit both to `main`.
2. Tag `vX.Y.Z` (a `-` suffix, such as `v1.2.0-rc.1`, makes a prerelease) and push the tag.
3. The Release workflow checks the tag against the version, builds, and opens a draft release with the exe and `checksums.txt`.
4. Write the notes in the draft and publish it.
