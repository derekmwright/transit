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

## Why MIDI timing can be "approximate"

A MIDI event is timestamped in its own backend domain (WinMM). Placing it at a device frame needs
the fit; when the fit is missing or only `Arrival`-grade, the event is flagged approximate
(`approximate` in `crates/playback/src/player/audio/midi.rs`) and delivered as soon as possible
rather than at a computed frame. Real backend timestamps can be wrong (ASIO timestamps that move
backwards, missing validity flags), so approximate is a normal state, not an error.

## Latency

Reported latency is nominal unless measured. Do not claim certified latency from backend numbers;
see `../evidence/hardware-and-ci.md`. Device backend work: [ADR 0016](../../adr/0016-native-wasapi-backend.md).

Tests: `crates/playback/src/device_clock/tests.rs`, `crates/playback/tests/backend_time_contract.rs`.
