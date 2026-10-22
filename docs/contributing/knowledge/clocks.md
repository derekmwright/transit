# Clock domains

Contract: [ADR 0012 clock annex](../../adr/0012-clock-contract-annex.md). Code:
`crates/playback/src/device_clock.rs` and `device_clock/`.

| Coordinate | Meaning |
|---|---|
| Host time H | Monotonic process time in integer nanoseconds from a runtime origin. |
| Backend time B | A timestamp from one identified audio or MIDI backend clock domain. |
| Device frames D (output), I (input) | Frame ordinal of one stream; separate for capture. |
| Render frames R | The engine's monotonic work ordinal, which can run ahead of presentation. |
| Session samples S | Position in the musical session. Changes on play, seek, loop, rate change. |

Never mix coordinates in one integer. Equal nominal sample rates do not prove equal clock rates.

## The clock fit and quality

Each stream's callbacks give `Observation`s with a `Stamp` of a `Quality`: `Invalid`, `Arrival`
(host arrival only, unknown dispatch bias), `BackendEstimate`, or `Measured`
(`crates/playback/src/device_clock.rs`). A bounded fit maps B (or arrival) to D with an error
bound. Code must carry the quality along and degrade, not assume numbers are exact.

Stamps come from the audio backend trait (`crates/audio-backend`): every device block carries a
`Stamp` in origin-relative nanoseconds with its `Quality`. The cpal adapter
(`crates/playback/src/backend/cpal.rs`) maps cpal timestamps to stamps; the clock service consumes
only stamps.

## Why MIDI timing can be "approximate"

A MIDI event is timestamped in its own backend domain (WinMM). Placing it at a device frame needs
the fit; when the fit is missing or only `Arrival`-grade, the event is flagged approximate
(`approximate` in `crates/playback/src/player/audio/midi.rs`) and delivered as soon as possible
rather than at a computed frame. Real backend timestamps can be wrong (ASIO timestamps that move
backwards, missing validity flags), so approximate is a normal state, not an error.

## Latency

Reported latency is nominal unless measured. Do not claim certified latency from backend numbers;
see `../evidence/hardware-and-ci.md`. Device backend work: [ADR 0016](../../adr/0016-native-wasapi-backend.md).

## Which backend serves the device

`AudioConfig.backend` (`Auto | Cpal | NativeWasapi`, saved with the audio settings) picks the
implementation on the WASAPI system; `Auto` is cpal until ADR 0016 slice (f). `NativeWasapi` takes
effect only in a build with the `native-wasapi` feature (`transit`, `soundcraft-playback`,
`audio-bench`); if its open fails before `start()`, the Player opens cpal with the same choices and
keeps "Native WASAPI failed: <reason>; using the cpal backend" (`Player::backend_note`), shown on the
status line and in Audio Devices. The load readout's host text is `WASAPI*` while the native
backend serves the device and `WASAPI` for cpal; the saved I/O map is keyed by the system name
(`WASAPI`) in both cases. Native stream stamps are `BackendEstimate` (device position plus padding
plus `GetStreamLatency`); measure them with `audio-bench --backend wasapi` before trusting them.

## Native capture, duplex and what `Measured` means (ADR 0016 slice d)

The native backend opens capture as its own endpoint, client, thread and position counter
(`transit-audio-wasapi`, `capture_loop.rs`); only QPC is shared with the render stream. Each stream
captures its `(QPC, Instant)` pair at its own `start()` against the Player's one `HostOrigin`, so the
stamps of the two streams are comparable in host nanoseconds, and each has its own `ClockKey`. A
capture packet's frame time is the QPC `GetBuffer` reports for its first frame; its callback time is
the `IAudioClock2` position read right after. A `DATA_DISCONTINUITY` or `TIMESTAMP_ERROR` packet is
delivered with an invalid stamp (the fit restarts) and counted; a `SILENT` packet is delivered as
zeros. Stamps never step backward and host time strictly increases (`render_loop::Held`).

A render and a capture endpoint are one duplex device when the user paired them
(`AudioConfig.input_pairing`) or they report the same `PKEY_Device_ContainerId`
(`pairing.rs`); the Player then opens the input with the output. The recorder maps the output's
host time to input frames through the input stream's own fit.

`OpenRequest.calibration` (`Calibration { round_trip_ns, uncertainty_ns }`) is what turns the native
stamps to `Quality::Measured`. The round trip is the delay the stamps do not account for, measured in
the stamps' own time (an output frame's presentation time to the input frame time of the same click
over a loopback cable). The input stream carries all of it (its frame time moves earlier by the round
trip), so the output's presentation time stays the reference, and both streams report the
`uncertainty_ns` bound. `Measured` therefore claims the pair is tied together to within that bound;
it does not separate the DAC delay from the ADC delay. Nothing stores or supplies a calibration yet:
the measurement (`audio-bench --backend wasapi --loopback`), its per-machine storage and the "calibrated"
label are the remaining part of slice (d).

Tests: `crates/playback/src/device_clock/tests.rs`, `crates/playback/tests/backend_time_contract.rs`.
