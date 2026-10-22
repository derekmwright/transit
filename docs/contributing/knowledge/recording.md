# Recording

A lost take is a merge-blocking defect. Treat every change here as high risk.

- **Capture:** `crates/playback/src/record.rs` opens the input stream with the most channels the
  device offers and hands frames to the plan bridge (`plan_bridge.rs`) for monitoring and takes.
- **Record-start latch:** `crates/playback/src/record/start.rs`. A start is a prepared operation
  with a `Ticket` (engine epoch plus a non-wrapping sequence). The audio side marks it started when
  the applied action runs, not when the UI asks, so the take's start frame is the real one. A
  delayed or cancelled ticket can never start a later take. `start/mapping.rs` maps the start to
  the session position.
- **Placement:** takes are placed from the latched device frame and the clock fit, not from the
  time the user pressed the key (`clocks.md`).
- **Input selection:** monitoring and recording must resolve an input path the same way
  (`crates/engine/src/io.rs`; known issue list for gaps).
- **Data safety:** take data is the user's work. Never add a path that can discard captured frames
  on stop, device loss or a full buffer without surfacing it; cap allocations, but never by dropping
  a take silently.
- **MIDI recording:** `crates/playback/src/midi_capture.rs` is a preallocated lock-free ring (64Ki
  events, a reserve kept for note-offs) the audio thread fills with each admitted live event and its
  device frame; the ring is shared by all tracks, and a full ring drops the event and counts it
  (surfaced as events lost). The same record-start latch and `start/mapping.rs` map frames to the
  session, so a MIDI take lines up with an audio take. `crates/engine/src/midi_take.rs` assembles
  takes at Stop: pairs note-ons with note-offs, converts to ticks, merges an overdub into the one
  clip under the pass, duplicates the playlist when several clips are in the way, and makes one
  playlist per loop pass. There is no MIDI take file yet (known issue K11).

Tests: `crates/engine/tests/recording.rs`, `crates/playback/tests/monitor_contract.rs`,
`crates/playback/tests/publication_contract.rs`.
