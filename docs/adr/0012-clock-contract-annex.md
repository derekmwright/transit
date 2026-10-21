# ADR 0012 annex: clock domains and timestamp quality

- Date: 2026-10-18. Status: proposed for joint review.
- Parent: [ADR 0012](0012-midi-device-layer.md), gates G4 and G6.
- Implementation status (2026-10-19 09:58 EDT): the original `f9078c7` jitter
  failure was corrected by retaining bounded observation spans. Full CI and independent
  re-review passed; clock/loop policy integrated through `06a936b`. **G4 fake-device
  passed at component level.** Player/take placement migration and hardware timing
  remain open. A live silent probe found WASAPI capture anchor failures, output
  non-convergence on a second open, and Focusrite ASIO backend timestamps that move
  backwards while host arrivals increase. Both ASIO opens ended without maps.
  Numerical
  backend estimates retain unknown offset uncertainty; these results do not certify
  hardware accuracy or demonstrate audio sample loss.
- Joint review: the pinned backend sources were reviewed on 2026-10-18;
  the clarifications below incorporate that review, with error bounds and early
  clock safety tests retained as described under G4.
- Applies to current CPAL 0.16.0 and the engine audited at e6acafa/d469e8f.

## What the device API actually provides

CPAL's timestamp origin is unspecified and its source depends on the backend.
Its type alone does not prove that two streams, a MIDI backend, and Rust Instant
share an epoch or resolution. [CPAL 0.16 StreamInstant](https://docs.rs/cpal/0.16.0/cpal/struct.StreamInstant.html).

The pinned implementation imposes further limits:

- WASAPI `host/wasapi/stream.rs:529–582` obtains a QPC time from IAudioClock,
  discards its device-position result, and estimates output presentation by adding
  `frames_available / sample_rate`. This is not a measured DAC position/latency.
  In a shared render buffer, queued padding is the relevant first-frame wait;
  adding the available space can create a systematic offset. Preserve padding
  when the backend exposes it, and test/calibrate rather than assuming this value.
- WASAPI capture uses the first frame's QPC timestamp from GetBuffer. The Windows
  API also reports discontinuity and timestamp-error flags. A backend adapter must
  retain those quality signals instead of treating every numeric timestamp as valid.
  [IAudioCaptureClient::GetBuffer](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer).
- ASIO `host/asio/stream.rs:637–651, 898–948` converts the callback's system time,
  then estimates playback/capture time by adding/subtracting one driver buffer.
  These helpers do not use the driver's reported input/output latencies or expose
  a device sample-position anchor to playback. A nominal buffer is not a complete
  interface latency measurement.
  The CPAL adapter also does not check system-time validity. A hardware-free probe
  of the original 0.16 StreamInstant arithmetic found that subtracting 128 frames at
  48 kHz from time zero returns a malformed future timestamp, about 4.2923 seconds
  after the callback, rather than panicking. This refutes the specific zero-time
  panic hypothesis, but confirms the need to validate timestamps before fitting.
  A real driver returning invalid zero time has not been demonstrated.
  The product's current fork pin `31b5dd4` repairs that signed arithmetic, verified
  by public API regression tests. It does not repair missing backend validity flags
  or establish an ADC/DAC offset.

The backend descriptions above come from source inspection. The live metadata
probe recorded separately now supplies device counterexamples to clock availability,
not a physical latency measurement. G4 supplies the observation seam and synthetic
checks; hardware timing still requires valid anchors and independent calibration.

## Distinct coordinates

| Coordinate | Owner and meaning | Discontinuity |
|---|---|---|
| Host time H | Monotonic process time, integer nanoseconds relative to an explicit runtime origin | New runtime origin |
| Backend time B | Timestamp from an identified audio/MIDI backend clock domain | Backend reset, wrap, invalid observation |
| Output frames D | Monotonic frame ordinal for the output stream; actual sample rate is estimated separately | Device generation changes |
| Input frames I | Separate capture-stream ordinal, even with the same nominal rate | Capture generation changes |
| Render frames R | Monotonic engine work ordinal, including processing ahead of presentation | Engine/format restart; same-rate plan edits preserve R and carry a separate plan generation |
| Session samples S | Position in the musical session, at the session sample rate | Play, stop, seek, loop wrap, rate change |

Equal nominal rates do not prove equal clock rates. Split devices require an
estimated input/output rate ratio and a controlled asynchronous resampler; a
periodic discard of queued audio is not drift correction. SRC filter delay and
route/plugin delay remain named components of the timing model.

## Observation and publication contract

A device observation carries direction, device generation, backend clock-domain
id, first-frame ordinal, frame count, nominal rate, callback timestamp,
capture/presentation timestamp when available, and quality/uncertainty. Quality
is one of measured hardware position, backend estimate, callback-arrival fallback,
or invalid. Unknown latency is represented as unknown, never zero.

At callback entry the runtime also observes its host clock. A pair of timestamps
is a calibration observation with uncertainty, not an equality: callback dispatch
delay, driver timestamp resolution and sample-position semantics matter. Use
multiple observations to estimate offset/rate and reject discontinuities. A
backend that cannot provide a sufficiently precise relation must expose degraded
accuracy; adding a buffer cannot recover timestamp information already lost.

The audio thread owns current output-frame advancement and adopts precomputed
calibration/transport changes at explicit boundaries. A non-realtime runtime owner
collects bounded observation records and prepares revised clock estimates. No
callback waits for the worker. Overflow, invalid observations, excessive residuals,
and stale estimates are counted. Published estimates have a generation and a
validity horizon. Reader retries must be bounded; keep the last valid snapshot or
return unavailable instead of spinning on a writer.

Publish residual jitter of the fitted relation separately from absolute offset
uncertainty. A callback-entry fit can improve relative monitoring placement without
establishing an accurate ADC/DAC offset. State precision per backend and direction;
the worst component resolution is a lower limit on achievable precision, not a
valid upper bound on combined error. End-to-end bounds must also include bias,
calibration error, timestamp granularity and delivery delay. Audio settings should
show quality, both uncertainty measures, and whether loopback calibration exists.

## Mapping into the session

Session mapping is a bounded history of segments, not one global offset. A segment
identifies a device/transport generation, a half-open output-frame interval,
the corresponding session sample/fraction, session/device rate ratio, play state,
loop iteration and the render plan's measured delay convention. Split segments at
seek, loop wrap, start/stop and speed changes, including several boundaries inside
one callback. Integer/rational accumulation retains fractional phase across blocks.

The compiler/runtime must agree exactly once where each delay is accounted for:
device output latency, SRC delay, main/route compensation, and input calibration.
An impulse test is the authority for this convention. Do not subtract plugin
latency both in the render plan and again during take placement. Each recorded
take stores its placement calibration and generation so a later device change
cannot silently reinterpret it.

Record arm and Play become one runtime transaction with a chosen timeline boundary;
the UI's call order is not the recording clock. Capture blocks carry their first
frame's time and sequence, allowing data before the chosen boundary to be trimmed
and later frames to be placed independently of writer scheduling. Stop closes an
admission generation and waits off the audio thread for admitted capture work to
finish publishing, then drains through that final sequence. Device loss must not
require another callback to acknowledge an already idle producer.

## MIDI admission and late events

Ingress preserves backend arrival time, clock-domain id, port/device generation,
channel, note identity and stable sequence. This is the time the backend observed
the event, not a claim about the physical keypress. Recording uses this mapped
time; monitoring uses the same time plus an explicit lookahead budget.

Express lookahead in frames or time, not "one callback" when callback sizes vary.
It bounds scheduling jitter only while arrival delay and clock uncertainty fit
the declared budget. Each event has an explicit result:

- Within a future/current processing interval: scheduled at its exact offset.
- Too late: render at the earliest legal offset, increment lateness counters and
  retain the original arrival time for recording; releases remain protected.
- Stale generation, invalid mapping or full queue: apply a documented recovery
  policy and count it; do not reinterpret an old event under a new epoch.

Seek, restart, port disconnect and stale note generations trigger a bounded
release protocol agreed with F10. Equal-time event ordering is stable and must
preserve sustain/note-release semantics. Fixed latency does not imply zero jitter
for arbitrarily delayed OS delivery.

WinMM's [MIM_DATA arrival timestamp](https://learn.microsoft.com/en-us/windows/win32/multimedia/mim-data)
uses milliseconds from midiInStart. Preserve
that granularity and the MIDI port's origin instead of advertising sample-accurate
keypress capture merely because the output uses sample offsets. Verify the chosen
MIDI backend's actual timestamp path before selecting a precision target.

## Runtime ownership proposal (G6)

Introduce a non-UI `AudioRuntime` in playback (or a dedicated L4 runtime crate if
dependencies require it), with both front ends calling the same lifecycle methods.
It owns Player, Recorder, audio configuration, clock generations, recording
transactions, and eventual MIDI port runtime. Session edits still go through the
engine. Owning runtime methods that manipulate editors and plugin controllers run
on the required main thread; putting them in a non-UI type does not grant worker
thread affinity.

| Context | Owns / may do | Must not do |
|---|---|---|
| Runtime/main-thread facade | Anchored processing holder, plugin controllers/editors, prepared plans, commands and typed acknowledgements | Device/writer joins; depend on UI repaint for event timing |
| Native lifecycle owner | Device transitions, stream close/join, recording finalization; borrow the anchored holder | Own/finally drop plugins; wait for main while holding its locks |
| Output callback | Render state, frame advancement, bounded event admission, telemetry | Device open/close, controller calls, allocation, waiting |
| Input callback | Capture sequence and bounded frame publication | File I/O, waiting for output/writer |
| Recording worker | WAV writes, final drain, peak summaries | Infer placement from its wake time |
| Clock worker | Calibration estimates and quality checks | Mutate active render state |
| MIDI ingress/scheduler | Timestamped bytes and bounded scheduled output | Own musical transport independently |

The proposed native/main split, pumped quit, state authority and WebAudio adapter are
specified in the runtime-owner contract (ADR 0012, gate G6). Main
continues to anchor processing owners until explicit quiescence; lifecycle requests
wake the facade independently of periodic repaint. This is a proposed G6 decision,
not an implemented migration or G12 hardware result.

## G4 test evidence required

The fake device must drive the same observation, calibration and event-admission
code used by the real adapter. A separate ideal-clock calculator is not sufficient.
Keep the adapter/fake in playback or its successor runtime crate; audio-io remains
file codecs. Expose a `fake-device` feature when cross-crate tests need the seam.
Tests inject known ground truth without sleeps or hardware:

1. Distinct input/output callback partitions, rates, backend epochs and arrival
   delays; an impulse and MIDI note map to the expected sample within the declared
   precision, independent of callback partitioning.
2. Loop wrap inside a block, several wraps in one block, seek, stop, fractional
   rate conversion, and speed changes; no old segment is used after its generation.
3. Restart A→B→A with stale observations/events in flight; invalidate old mappings,
   release held notes, and prohibit stale capture from entering a new take.
4. Delayed and reordered observations, timestamp errors, clock drift, queue full,
   missing callbacks, zero frames, and arithmetic extremes; bounded recovery and
   correct counters, with no callback allocation or unbounded retry.
5. Late events under a fixed lookahead budget, boundary events exactly at block
   end, and release events during overload. Recorded arrival time stays unchanged.

An ASIO/WASAPI backend test must additionally demonstrate which observations are
measured versus estimated. Hardware loopback then validates residual placement at
64/128/256 frames and 44.1/48/96 kHz. Neither fake-device tests nor a backend estimate
alone establishes hardware sample accuracy.

Before MIDI depends on the map: cases 1–3 and the clock-only safety cases from 4
(invalid/stale observations, zero frames, arithmetic limits, bounded publication)
must pass. MIDI event overload/release cases in 4–5 land with G5. A fake event can
exercise timestamp placement before the full note/port identity model exists;
it must still use the production mapping path.
