# Layering

Crates form layers; a crate may depend only on its own layer or lower. The table is `LAYERS` in
`xtask/src/main.rs`, and `cargo xtask layers` enforces it.

| Layer | Crates |
|---|---|
| L0 | `time`, `rt`, `audio-io`, `midi-io`, `midi`, `video` |
| L1 | `dsp`, `clap-host`, `vst3-host`, `au-host`, `asio-panel` |
| L2 | `model` (the session document) |
| L3 | `mix` (the render engine) |
| L4 | `engine` (commands, undo, I/O), `playback` (devices, Player) |
| L5 | `automation` (control channel, MCP) |
| L6 | `ui-studio` (transit's UI), `ui-egui` (legacy, kept for the upstream app) |
| apps | `apps/transit`, plus the upstream `soundcraft*` apps (exempt from the table) |

## Rules the check enforces

- Every crate is registered in `LAYERS`; a new crate needs a row (and a layer you can justify).
- No dependency points upward (dev-dependencies excepted).
- The standalone crates (`STANDALONE`: `time`, `audio-io`, `midi-io`, `midi`, `video`, `dsp`) have no
  workspace dependencies at all.
- Nothing below L6 depends on egui, eframe, winit, rfd, craft-ui or similar (`UI_CRATES`).

## Notes

- `crates/mix` forbids `unsafe` (`unsafe_code = "forbid"`). See `unsafe.md`.
- The `soundcraft-*` package names stay; the short names above drop that prefix.
- `ui-studio` is transit's UI. Do not add new features to `ui-egui`.
