# transit: instructions for agents

transit is a desktop DAW (Rust, egui) built on the SoundCraft engine. Decisions live in ADRs, which
are maintained by the project owner; do not add or change one without being asked.

## Read first
1. `README.md` and `CONTRIBUTING.md`.
2. `docs/contributing/knowledge/README.md`: how the engine and UI are built (start with `layering.md`
   and `audio-thread.md`, then the topics for the crates you touch).
3. `docs/contributing/evidence/README.md`: gates, refutation review, the escape record, test rules.
4. `docs/adr/README.md`: the decision index.
5. `docs/known-issues/README.md`: do not rediscover these; add new ones as rows.
6. `docs/building.md`: build and run.

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

## Evidence
- Every escaped defect gets a gate that would have caught it. Never patch around a defect; keep the
  repro and fix the root cause.
- Session files: new fields use `serde(default)`, never `deny_unknown_fields`; old sessions must load.
- A test is evidence only if it fails without the fix.

## Pre-PR checklist
```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask layers
cargo xtask assets
```
- Area gates (allocation probes, parity goldens, fake-device tests): `docs/contributing/evidence/gates.md`.
- Run a refutation review on behavior changes; only critical findings block, the rest are known issues.
- Every bundled asset needs a row in `ATTRIBUTION.md`.
- American English in code, comments, UI strings and docs.
- Keep diffs small and in the style of the surrounding code. CI runs only on tags: report what you ran.
