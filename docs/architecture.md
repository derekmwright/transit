# Architecture

transit is a Cargo workspace of small crates with enforced layering (`cargo xtask layers`). Nothing
below the UI knows about egui, so the interface can be replaced. Detail lives in
[`contributing/knowledge/`](contributing/knowledge/README.md); decisions in [`adr/`](adr/README.md).

```
L0  time  rt  audio-io  midi-io  midi  video      (standalone or primitives)
L1  dsp  clap-host  vst3-host  au-host  asio-panel (DSP and plug-in hosts)
L2  model                                          (the session document)
L3  mix                                            (the plan renderer)
L4  engine  playback                               (commands, undo, I/O · devices, Player)
L5  automation                                     (control channel, MCP)
L6  ui-studio (transit)   ui-egui (legacy)         (the user interface)
    apps: transit (desktop), plus the upstream soundcraft apps
```

Layer rules: [`layering.md`](contributing/knowledge/layering.md).

## The document

`model::Session` is plain serde data: tracks with playlists of clips, mixer state, tempo and meter
maps, memory locations, groups, buses, and edit state. Decoded audio lives in a `SourcePool` of
`Arc`s that is not serialized. Sessions save as `.scraft` JSON plus an `Audio Files/` folder. Format
rules: [`session-format.md`](contributing/knowledge/session-format.md).

## Commands and undo

Every user action is a registered command in `crates/engine/src/cmd/`; `Engine::execute` runs it and
records one undo step. See [`commands.md`](contributing/knowledge/commands.md).

## Audio

The plan renderer serves realtime playback, bounces and freezes. (The old `MixEngine::render` path is gone.) The control
side compiles a session into a preallocated `PreparedPlan`; the audio callback adopts it at a block
boundary and renders without allocating or locking. Plug-in delay compensation aligns every path to
the sinks. Details: [`plan-renderer.md`](contributing/knowledge/plan-renderer.md),
[`audio-thread.md`](contributing/knowledge/audio-thread.md).

- Devices and timestamps: [`clocks.md`](contributing/knowledge/clocks.md).
- Output and input paths, the route graph: [`routing.md`](contributing/knowledge/routing.md);
  sidechain keys: [`sidechain.md`](contributing/knowledge/sidechain.md).
- Live MIDI: [`midi.md`](contributing/knowledge/midi.md). Recording: [`recording.md`](contributing/knowledge/recording.md).
- Hosted plug-ins (`clap-host`, `vst3-host`, `au-host`) are scanned, created and destroyed off the
  audio thread; only `process` runs on it. Unsafe code: [`unsafe.md`](contributing/knowledge/unsafe.md).
- `video` holds our own H.264, ProRes and Motion JPEG decoders and the ISO-BMFF demuxer.

## UI

`crates/ui-studio` is transit's UI: one shortcut table, Lucide icons, theme tokens, command-driven.
See [`ui.md`](contributing/knowledge/ui.md).

## Agent control

The desktop app exposes a JSON-lines control channel over TCP (engine commands, inspection, synthetic
input, screenshots), and `soundcraft-cli mcp` wraps a headless engine or a running app as an MCP
server. See [`control-protocol.md`](control-protocol.md) and [`mcp.md`](mcp.md).
