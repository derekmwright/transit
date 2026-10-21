# transit: instructions for agents

transit is a desktop DAW (Rust, egui) built on the SoundCraft engine. Read `README.md` and
`CONTRIBUTING.md` first. Decisions live in ADRs, which are maintained by the project owner; do not
add or change one without being asked.

## Layering
Crates form layers, and a crate may depend only on lower layers (L0 time/audio-io/midi, L1 dsp,
L2 model, L3 mix, L4 engine/playback, L5 automation, L6 UI, then the apps). Nothing below the UI
depends on egui, eframe, winit or rfd. `cargo xtask layers` enforces this; keep it green.

## Never crash
A crash loses someone's recording. This outranks feature work.
- No panics in non-test code: no `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`,
  `unimplemented!`. Return `Result` and use `?`. Treat input-derived numbers (files, MIDI, plug-in
  data) as hostile: use `get()`, checked arithmetic, and cap allocations.
- The audio callback never allocates, takes a lock, blocks, or panics. Hand data to it through
  lock-free structures.
- `unsafe` only in `crates/rt`, plus SDK-interface code in the host crates (CLAP, VST3, ASIO), where
  every block carries a quoted `// SAFETY:` review and a safe fallback exists. Tests cover it.
- Every crash fix lands with a regression test.

## Before a pull request
```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask layers
```
- Every bundled asset needs a row in `ATTRIBUTION.md` (`cargo xtask assets` checks it).
- Use American English in code, comments, UI strings and docs.
- Keep diffs small and in the style of the surrounding code.
