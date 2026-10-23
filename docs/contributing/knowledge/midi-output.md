# MIDI output

A MIDI or instrument track can send its MIDI to a hardware or virtual port instead of its hosted
instrument (#61). Input side: `midi.md`; clocks: `clocks.md`; device layer:
[ADR 0012](../../adr/0012-midi-device-layer.md), decision 5.

## Model and command

- `Track::midi_output` (`crates/model/src/midi_output.rs`): `Internal` (the default, not written to
  the file) or `External { port, channel, offset_ms }`. The port is stored by backend, name and
  ordinal plus the backend's stable id when it has one (WinMM interface path). Channel is
  `as_recorded` or 1 to 16. The offset is clamped to ±500 ms when read, since files are hostile.
- `midi.output` (`crates/engine/src/cmd/midi_output.rs`): one undo step for any number of tracks;
  omitted fields keep their values. UI: the "Out" button at the end of a MIDI strip's input row
  (`crates/ui-studio/src/midi_output.rs`).

## Routing rule: instead of, not in addition to

An external destination replaces the hosted instrument for that track. Clip notes and controllers
go to the port; the instrument receives only note-offs, so switching destination mid-note cannot
leave a voice stuck. Live input on an armed or monitored track is echoed to the port and does not
reach the instrument, but it is still captured for a MIDI recording, so a MIDI track with no
instrument can record. Track mute and solo silence the port (note-offs still pass). Track
automation lanes do not generate MIDI CCs today, so there is nothing of theirs to forward; clip
controllers (CC, pitch bend, pressure, program) are sent.

## Threads

| Thread | Does | Never |
|---|---|---|
| Audio callback (`player/audio/external.rs`) | Per block, visits the clip events of external tracks (`soundcraft_mix::external_midi_events`) and pushes stamped copies into a preallocated SPSC ring | Calls into `transit-midi-io`, allocates, locks, blocks |
| MIDI ingress (`player/ingress.rs`) | Pushes live input of echo routes into a second ring as soon as it pops it | Waits on the output thread |
| MIDI output (`player/midi_out.rs`, `transit-midi-io` sends) | Owns every `OutputHandle`; sorts events by deadline; sends; tracks held notes; polls ports | Runs on an audio thread (counted in `audio_thread_sends`, tested) |
| Main | Sends the destination table on each `Player::service`, the panic on Stop and All Notes Off; reads the status | Sends MIDI |

Ring full: the event is dropped and counted; the last 256 slots are kept for releases.

## Timing model

For each block about to render, the audio thread takes the block's first render frame, maps it
through the converter phase to an output-device frame (`Phase::lead`), and maps that frame to host
time H through the device clock map (`ClockMap::frame_to_host`, ADR 0017). While no map is valid,
it falls back to the host time at callback entry plus the frames between, which is early by the
device's output latency. Each event gets `H(block) + offset_in_block × ns_per_session_sample` at
the current speed. The output thread sends it at `H + destination offset`.

- The audio is rendered ahead of presentation, so events usually arrive at the output thread
  before their instant. A negative offset larger than that lead cannot be honored: the event is
  sent at once and counted late.
- The output thread sleeps until 0.3 ms before the next deadline (high-resolution sleep, at most
  1 ms at a time, which also bounds echo polling) and yields for the rest. Expected thread jitter
  is well under 1 ms on an idle machine; the OS MIDI driver and USB add their own. A send more than
  2 ms late is counted (`late`, `max_late_ns`, in the Out button's tooltip). Hardware accuracy is
  not established by the fake-backend tests; it needs Derek's hardware check.
- Echo ignores the offset and goes out at once. Its latency is the ingress worker's poll plus the
  output thread's poll (two 1 ms loops; the ingress park can round to the system timer tick).
- With no external destination the thread parks between port polls (4 per second, for the
  picker's list) instead of waking every millisecond.
- The output thread runs at normal priority; promoting it is a follow-up if jitter under load
  matters.

## Safety

- **Stop** (the audio thread's transition, plus a direct panic from `Player::stop`): scheduled
  playback is dropped; every held note gets a note-off, held pedals go up, then Sustain off
  (CC 64 = 0) and All Notes Off (CC 123) on every channel the port was used on.
- **Loop wrap and locate** (#47's rule): at the jump instant (plus each destination's offset) the
  notes playback holds end; events scheduled at or after it are dropped (a locate discards its
  rendered-ahead block); the new lap or position starts again every note that sounds across it
  (`chase`). The same chase runs at play start.
- **Destination change** (port or channel, or back to internal): the track's held notes end on the
  old port, then Sustain off and All Notes Off on the channels that track used.
- **Echo route revoked** (disarm, monitor off, a MIDI input or channel filter change, the input
  controller unplugged): the key-up can no longer be echoed, so the output thread ends every note
  and pedal the echo holds for that track (note-offs, Sustain off), as revoking the source does for
  an instrument. `Native::service` sends `release_echo` for each revoked echo route; the
  destination's `echo` flag (armed or monitored) also releases when it goes false, and while it is
  false a late echo may only end notes, never start them.
- **Device loss**: any `PortWatcher` poll (the output thread polls every 250 ms) flags the handle;
  the port is closed and marked missing, held state is forgotten and sends are counted as unsent.
  When the port appears again it reopens and first gets Sustain off and All Notes Off on its used
  channels. A used port that is missing or failed to open is retried on every poll, so a port an
  old output thread still held (stuck in a driver at a device reopen) opens once it lets go.
- **Exit**: dropping the Player stops the thread, which releases everything on every open port; the
  drop waits up to 500 ms for that and never joins a stuck driver.

## Persistence

Saved by port name and id. A session opened where the port is absent shows "device missing" on
the strip's Out button (red, with a warning icon and tooltip) and sends nothing; it connects when
the port appears. A port of another backend (a session from another OS) stays missing. The track
header indicator is a follow-up (`TODO(#61)` in `midi_output.rs`): the header lives in
`arrange.rs`.

## Not in scope

MIDI clock and MTC out, transport messages, SysEx, program change management.

Tests: `crates/playback/src/player/midi_out/tests.rs` (fake backend: order and offset, thread
identity, Stop, wrap, destination change, loss and reconnect, missing at load, echo, exit),
`crates/playback/src/player/device/external_tests.rs` (through the Player: a looped clip to the
port, no allocation in the callback, live echo without an instrument), `crates/mix/src/external_midi.rs`,
`crates/engine/tests/midi_output.rs`, `crates/model/src/midi_output.rs`, `crates/ui-studio/src/midi_output.rs`.
