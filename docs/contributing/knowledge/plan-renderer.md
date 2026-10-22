# Plan renderer

The renderer for realtime playback, bounces and freezes. The old `MixEngine::render` path and the `legacy-render` feature are gone; this is the only renderer.

## Flow

1. **Compile** (`crates/mix/src/compile.rs`): turns a `Session` into a `PreparedPlan`
   (`crates/mix/src/plan.rs`): layers of strips, buses, sinks, key taps, and the tables the callback
   needs, all preallocated. Compilation allocates and runs off the audio thread.
2. **Adopt:** the callback swaps in the new plan at a block boundary (`audio-thread.md`).
3. **Render** (`crates/mix/src/active.rs`, `render_active`): fills the sinks for one block with no
   allocation.

## Structure

- **Layers:** strips are ordered by dependency (routing and key edges). Strips in one layer are
  independent and may run in parallel on the render pool.
- **Strips and buses:** clips, trim, inserts, pre-fader sends, fader and mute, post-fader sends,
  pan, then buses and master. `crates/mix/src/lib.rs` has the chain.
- **Sinks** (`crates/mix/src/sink.rs`): where a path's mix goes (device output channels, bounce
  file). **Keys** (`crates/mix/src/key.rs`): sidechain taps; see `sidechain.md`.
- **Events** (`crates/mix/src/events.rs`): instrument events with sample-accurate offsets.

## Plug-in delay compensation (PDC)

Every path to a sink is aligned to the longest one, plus a shared device latency L so that
hardware outputs line up with software paths. Tests: `crates/mix/tests/path_pdc.rs`,
`master_insert_latency.rs`. A latent insert anywhere changes the plan's delays, so a change that
touches latency needs these.

## Loops

A routing loop is refused by rule L22 in the route graph (`crates/model/src/route_graph.rs`): edits
that would create one fail (`would_create_cycle_all`), and a loop in a loaded file is cut at load
(`RouteScratch::solve`). See `routing.md`.

## Parity and goldens

`crates/mix/tests/plan_parity.rs` renders sessions and compares with
`crates/mix/tests/golden/plan_parity`; offline against realtime is covered only in part: `output_sinks.rs` (`only_main_falls_back_and_a_path_table_without_routes_adds_nothing`) compares the plan's main mix bit for bit with `render_range` on a session with a send, a bus and a latent insert, and `path_pdc.rs` (`a_two_aux_lookahead_cascade_lines_up_with_the_dry_path`) checks the offline arrival within one sample. A general offline-equals-realtime parity test is planned, not written.
Update a golden only with a stated audible reason (`../evidence/tests-as-evidence.md`).

Design: [ADR 0015](../../adr/0015-io-routing-abstraction.md) sections 6 and 7.
