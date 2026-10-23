# Clock domains

Contract: [ADR 0017 clock annex](../../adr/0017-clock-contract-annex.md). Code:
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

`AudioConfig.backend` (`Auto | Cpal | NativeWasapi | NativeAsio`, saved with the audio settings)
picks the implementation: `NativeWasapi` on the WASAPI system, `NativeAsio` on the ASIO system
(`Backend::native_on`; `crates/playback/src/backend/native.rs`, `factory_for`); `Auto` is cpal until
ADR 0016 slice (f). A native choice takes effect only in a build with its feature (`native-wasapi`,
or `native-asio` on 64-bit Windows; `transit`, `soundcraft-playback`, `audio-bench`); if its open
fails before `start()`, the Player opens cpal with the same choices and keeps "Native WASAPI failed:
<reason>; using the cpal backend" (or "Native ASIO failed: ...") in `Player::backend_note`, shown on
the status line and in Audio Devices. An input opened on demand goes through the backend that serves
the output. The load readout's host text is `WASAPI* shared`/`WASAPI* exclusive` or `ASIO*` while a
native backend serves the device and `WASAPI`/`ASIO` for cpal; the saved I/O map is keyed by the
system name (`WASAPI`, `ASIO`) and the device name (an ASIO driver's registry name) in both cases.
Native WASAPI stamps are `BackendEstimate` (device position plus padding plus `GetStreamLatency`);
native ASIO stamps are `BackendEstimate` from the driver's sample position plus its `getLatencies`
figures, which by sample position match the measured loop within a few frames on the Scarlett 2i2
(K31); measuring the loop refines that and catches a loop in another phase (the Calibration line's
Measure in Audio Devices, or `audio-bench --backend asio --loopback --settings`). Calibrations of native ASIO are keyed by mode
`asio` (`devices::calibration_mode`) and hold the whole loop; details in [asio-backend.md](asio-backend.md#loopback-calibration-20-k31).

## Native capture, duplex and what `Measured` means (ADR 0016 slice d)

The native backend opens capture as its own endpoint, client, thread and position counter
(`transit-audio-wasapi`, `capture_loop.rs`); only QPC is shared with the render stream. Each stream
captures its `(QPC, Instant)` pair at its own `start()` against the Player's one `HostOrigin`, so the
stamps of the two streams are comparable in host nanoseconds, and each has its own `ClockKey`. A
capture packet's frame time is the QPC `GetBuffer` reports for its first frame; its callback time is
the QPC read when the packet is taken (on the Scarlett in shared mode that is about 18 ms after its
first frame; the capture client's own audio clock reads zero on real devices and is not used). A `DATA_DISCONTINUITY` or `TIMESTAMP_ERROR` packet is
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
it does not separate the DAC delay from the ADC delay.

The calibration is stored per machine in `ui.json` as `audio.calibrations`
(`LoopbackCalibration`: `device`, `input`, `mode`, `rate`, `period`, `round_trip_frames`,
`spread_frames`, `measured_at`) and applies to a stream pair only when the key
`(output device, paired input, mode, rate, requested period)` is exactly equal
(`AudioConfig::calibration_for`); any mismatch leaves the pair uncalibrated (`BackendEstimate`, and
recordings placed from it are approximate as before). `Player::open_with` sets
`OpenRequest.calibration` on both streams of a duplex open. Audio Devices shows a "Paired input"
selector (native backend only; it writes `input_pairing`) and a "Calibration" line for the chosen
pair, and the MIDI timing readout says "calibrated (00b1x ms)" for a `Measured` map.

### Calibrating a device pair

1. Cable an output to an input of the same interface (see `docs/benchmarks/README.md`), close the
   app, and run `audio-bench --backend wasapi --loopback --device <out> --in-device <in> --settings
   <ui.json>` at the rate and `--period` the app uses. `--in-device` is the same string the app's
   Paired input holds; without it the key's input is empty and matches an automatic pairing.
2. Per click the bench takes the capture stream's `backend_frame_ns` of the detected click minus the
   render stream's `backend_frame_ns` of the emitted click (each plus its frame's offset in its
   block). `round_trip_frames` is the median of those, rounded; the uncertainty is `p95 - min` of the
   same figures plus one frame of position granularity, rounded up. It refuses a run where fewer than
   half the clicks (and fewer than three) were found.
3. `--settings` merges the record into `audio.calibrations`, replacing an entry with the same key and
   keeping everything else. The app rewrites its `audio` block from memory when it saves, so it must
   be closed while the bench runs; it reads the file at its next start.

Render padding jitters, so the fit of a `Measured` stream must tolerate it:
`device_clock/tests.rs` fits a calibrated render stream (bound 0.5 ms) whose presentation time
jitters by +-1 ms, at random, alternating and one-late-block patterns, and stays under
`MAX_RESIDUAL_NS` (2 ms); the calibration offset is constant, so its bound does not enter the
residual. The `Measured` residual bound is therefore unchanged. Presentation jitter of 2 ms or more
(a padding that swings by a whole period at periods above about 4 ms) is still refused as a
discontinuity; measure it on the hardware before widening the bound.

Tests: `crates/playback/src/device_clock/tests.rs`, `crates/playback/tests/backend_time_contract.rs`.

## Capture gaps

A native WASAPI capture packet flagged `DATA_DISCONTINUITY` has frames missing before it (the first packet after Start carries the flag without a loss and is ignored); the loop sizes the gap from the clock-position jump (QPC delta if unreadable), clamps it to one second, and delivers that many zero frames first (`crates/audio-wasapi/src/capture_loop.rs`, K16).
