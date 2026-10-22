# Routing

Design: [ADR 0015](../../adr/0015-io-routing-abstraction.md). Read it before changing anything here.

## Model

- **Per machine:** an `IoMap` describes the device's channels and the named input and output paths
  a user built (settings, not the session). Code: `crates/playback/src/devices.rs`,
  `crates/ui-studio/src/audio_devices.rs` and `io_settings.rs`.
- **Per session:** tracks, buses and sends name their destinations by string, such as
  `Route::Hardware(name)` (`crates/model/src/mixer.rs`). The session carries names only, so a
  session moves between machines and each machine resolves the names against its own map.
- **Unresolved routes:** a name the machine cannot resolve is kept in the file, renders silent,
  and is shown to the user as unresolved. Never rewrite or drop it on load.
- **Name collisions:** a path and a bus may not share a name; the command refuses.

## Route graph

`crates/model/src/route_graph.rs` models edges (output, send, input, key). It serves loop refusal
(L22), delay compensation, and the plan layering. A new kind of edge must be added to all three,
with a test that loads a loop from a file.

## Where to change what

- Resolving names to channels: `crates/engine/src/io.rs` and `crates/mix/src/sink.rs`.
- Commands that edit routes: `crates/engine/src/cmd/track.rs`, `setup_more.rs`.
- Input path resolution is shared by monitoring and recording; do not duplicate it.

## Hardware inserts

An insert may carry a hardware pair: a send path (output) and a return path (input), by name
(`crates/mix/src/hardware.rs`, ADR 0015 section 5). The slot writes the send and reads the return from
the live capture ring, which already delivers it one round trip late, so there is no FIFO in the slot.
Its latency is the measured round trip (Measure in Audio Devices > I/O > Hardware inserts plays a click
train and finds it on the return), stored per machine with device, rate and buffer; delay compensation
uses it. The send sums unaligned by design: the measured trip spans from the sink write. States
(`HardwareState`): Ready, Unmeasured (runs, no latency), NoInput, MissingSend, MissingReturn (dry) and
Disabled (dry, with a reason). Conflicts: an ordinary route to a path an insert sends on is refused, and
so is an insert whose send path is already a route target; opening a session runs the same check and
disables offenders. Offline paths print the insert dry. Gaps: K18 to K22.
Tests: `crates/mix/tests/hardware_insert.rs`, `crates/engine/tests/hardware_insert.rs`.

Known gaps: `../../known-issues/README.md`.
