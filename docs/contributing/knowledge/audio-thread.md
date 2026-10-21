# Audio-thread contract

The audio callback (and anything it calls) must never:

- allocate or free (including growing a `Vec`, formatting a string, or dropping a `Box`);
- take a lock, wait on a channel, or block in any other way;
- drop a plug-in instance, a plan, or anything whose destructor may run foreign code;
- panic.

Panics are contained per strip (`crates/mix/tests/strip_panics.rs`); containment is a last resort,
not permission.

## How data reaches the callback

- **Primitives:** `crates/rt` (`handoff`): `Anchor<T>` lives on the control side and owns the value;
  `AudioAccess<T>` is the callback's handle. Retired values are returned to the control side to be
  dropped there. See `crates/rt/src/handoff.rs`.
- **Plans:** the control side compiles a `PreparedPlan` (`crates/mix/src/plan.rs`), all storage
  preallocated; it is published through the queue in `crates/playback/src/publication.rs` and
  adopted by the callback at a block boundary (`crates/playback/src/plan_bridge.rs`). The old plan
  goes back through the same channel for disposal off-thread.
- **Actions** (transport, MIDI, record start) go through the action lane
  (`crates/playback/src/action_lane.rs`), a bounded preallocated queue.
- **Plug-ins** are created, scanned and destroyed off the audio thread; only `process` runs on it.
- Logging from the audio thread uses `on_audio_thread()` and defers the write to the UI thread.

## Worker threads

Pool workers are promoted at thread start (MMCSS, SCHED_FIFO or rtkit, time-constraint policy) by
`crates/rt`. Never register with the OS inside a timed callback.

## Proving it

Zero-allocation tests: `crates/mix/tests/plan_alloc_probe.rs`, `alloc_probe.rs`,
`event_alloc_probe.rs`, using `measure` from `crates/rt/src/testing.rs`. A change that touches the
callback path extends these; see `../evidence/gates.md`.
