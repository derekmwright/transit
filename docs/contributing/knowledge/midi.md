# MIDI input

Decision: [ADR 0012](../../adr/0012-midi-device-layer.md); clocks: `clocks.md`; sending to a
hardware port: `midi-output.md`.

## Path

1. **Device layer** (`crates/midi-io`): the WinMM backend (via midir), a ring buffer
   (`ring.rs`), and a fake backend (`fake.rs`) for tests. Callbacks stamp packets in backend time.
2. **Ingress** (`crates/playback/src/player/ingress.rs`): maps packets to host time and delivers
   them to the audio side through the action lane. It must never block.
3. **Delivery** (`crates/playback/src/player/audio/midi.rs`): an inbox holds future events in device
   time until their render block exists. `RELEASE_RESERVE` keeps room so note-offs and pedal-ups are
   admitted even when the inbox is nearly full: dropping a release causes a stuck note.
4. **Mix** (`crates/mix/src/events.rs`): events become instrument events with sample offsets.

## Conventions

- Sustain pedal is CC 64; pedal edges must not be reordered before the note events they follow
  (timing-mode switches are guarded in `midi.rs`).
- Tombstones: `crates/playback/src/player/sources.rs` validates a source (port plus generation) again
  just before dispatch. A local tombstone records a revoke so a stale read of the shared admission
  flag cannot resurrect a revoked source; a late event from it is discarded.
- Step entry taps the same stream from the UI side (`crates/ui-studio/src/keys/step.rs`).

## Invariants

Stuck notes and dropped note-offs are merge-blocking defects. Timing may be flagged approximate
(`clocks.md`) but must never be silently wrong. The device callback may allocate or lock inside the
OS library (known issue K4); our code on that path may not.

Tests: `crates/midi-io/src/tests.rs`, `crates/playback/src/player/ingress/tests.rs`,
`crates/playback/src/player/audio/midi/tests.rs`, `crates/engine/tests/midi_input.rs`.
