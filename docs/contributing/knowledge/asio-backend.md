# Native ASIO backend

Code: `crates/audio-asio` (`transit-audio-asio`). Decision: the draft native-ASIO amendment to
[ADR 0016](../../adr/0016-native-wasapi-backend.md) (not yet accepted); unsafe policy:
[ADR 0013](../../adr/0013-unsafe-only-in-soundcraft-rt.md) and [`unsafe.md`](unsafe.md). 64-bit Windows
only; the crate is empty elsewhere. No ASIO SDK, C++, bindgen or LLVM in its build.

## Building

Nothing to install: the crate has no SDK, C++, bindgen, libclang or environment variables, and it is
compiled only under `cfg(all(windows, target_pointer_width = "64"))`, so a plain `cargo build` or
`cargo test -p transit-audio-asio` works on Windows and is a no-op elsewhere. Run the bench with
`cargo build --release -p audio-bench --features audio-bench/native-asio`, then
`audio-bench --backend asio --device "<driver registry name>"`. The hardware tests are ignored by
default: `cargo test -p transit-audio-asio -- --ignored hardware_ --nocapture --test-threads 1`, or
one by name (for example `hardware_scarlett_duplex_streams_two_seconds`). The app uses it with the
`transit/native-asio` feature (`soundcraft-playback/native-asio`), as "Native ASIO (experimental)"
under Audio system ASIO; see [In the app](#in-the-app-slice-c). The legacy cpal ASIO feature and its
SDK build are in [docs/building.md](../../building.md).

## The host thread (`src/host.rs`)

One dedicated thread, `transit-asio-host`, owns the driver object and makes every `IASIO` call:

- **Why.** Drivers register as `ThreadingModel=Apartment` in-process COM servers, so the object belongs
  to the STA that created it, and the SDK expects every call on the thread that called `init`. Drivers
  post messages to the `sysRef` window and open panels on the calling thread, so that thread needs a
  window and a message pump.
- **Startup.** `CoInitializeEx(COINIT_APARTMENTTHREADED)`, then a hidden message-only window
  (`HWND_MESSAGE`, system `STATIC` class) whose handle is the `sysRef` for `init`. cpal's `asio-sys`
  passes an uninitialized `sysRef`; we never do.
- **Requests.** Callers queue a closure over `HostState` (list, load and init, query, set rate, panel,
  unload) and set an auto-reset event. Between requests the thread dispatches its window messages and
  sleeps in `MsgWaitForMultipleObjects` on that event and its queue: without a timeout while no driver
  runs, and for at most a quarter of the watchdog's limit while one does ([Watchdog](#the-watchdog-slice-d1)).
- **Priority.** The thread raises itself to `THREAD_PRIORITY_ABOVE_NORMAL` at its start: it serves the
  driver's messages, the watchdog and the stream requests, and never runs audio, so it stays below
  the time-critical and MMCSS threads (a refusal is logged; it then runs at normal priority).
- **Timeouts.** A caller waits at most 10 s (`HOST_TIMEOUT`; 600 s for `controlPanel`, which may be
  modal). A driver that blocks (in `init`, say) makes the request fail with an error, not a hang; later
  requests fail at once while the thread is blocked; a load whose caller gave up is undone when the
  call returns. No `panic!`/`unwrap` on the loop (clippy-denied); a panicking request is an error reply.
- **One driver per process.** Every load claims a process-wide slot; a second load, on any host, is
  `OpenError::InUse` naming the open driver.

## Bindings (`src/abi.rs`, calls in `src/com.rs`)

Hand-written `#[repr(C)]` structures and the 24-slot `IASIO` vtable (`IUnknown`'s three, then the 21
ASIO methods of `iasiodrv.h`, `extern "C"` with `this` first: on x64 and ARM64 that is the C++
`thiscall`). Layouts follow `asio.h` under `#pragma pack(push,4)` (Windows), so `ASIODriverInfo`,
`ASIOBufferInfo`, `ASIOTimeCode`, `AsioTimeInfo` and `ASIOCallbacks` are `packed(4)`; `ASIOSampleRate` is
a `double` (`asiosys.h`), `ASIOSamples`/`ASIOTimeStamp` are `{hi, lo}`. Compile-time `size_of` and
`offset_of` assertions pin every field and vtable slot: `ASIODriverInfo` 172 (`sysRef` at 164),
`ASIOChannelInfo` 52, `ASIOClockSource` 48, `ASIOBufferInfo` 24, `ASIOTimeCode` 84, `AsioTimeInfo` 48,
`ASIOTime` 148, `ASIOCallbacks` 32, vtable 24 pointers (`init` slot 3, `outputReady` slot 23).

Drivers are created with `CoCreateInstanceEx(CLSID, CLSCTX_INPROC_SERVER)` and one `MULTI_QI` whose IID
is the CLSID (the ASIO convention). `Driver` wraps the raw pointer, is neither `Send` nor `Sync`, and
releases its reference on drop. All `unsafe` is in `com.rs` and, since slice (b), `callback.rs` (the
driver's callbacks and the buffers they touch), each block with a `// SAFETY:` comment
naming the invariant and the thread. Tests drive the same wrapper through a fake vtable of Rust
functions (`com::fake`), on the real host thread.

## What slice (a) covers

- `endpoints()`: reads `HKLM\SOFTWARE\ASIO` (name, CLSID, description, `InprocServer32` path and
  whether it exists), then loads, inits, queries and unloads each driver in turn: `getChannels`,
  `getBufferSize`, `canSampleRate` at 44.1/48/88.2/96 kHz, `getSampleRate`, `getLatencies`, and
  `getChannelInfo` for every channel. One `FormatRange` per accepted rate with the buffer range; an
  unsupported sample type (MSB, DSD, mixed) lists no format, and the type stays in the `AsioEndpoint`
  handle. Endpoint id and name are the registry key name (what cpal used and configs saved).
- Failures (no DLL, `CoCreateInstance`, `init` false, a query error) are `log::warn!`ed and kept in
  `AsioBackend::diagnostics()`; `ab::Endpoint` has no diagnostic field.
- `AsioBackend::load(name)` keeps a driver loaded and initialized with no buffers: `channels`,
  `latencies`, `buffer_size`, `sample_rate`, `can_sample_rate`, `set_sample_rate`, `control_panel`,
  `refresh`, and `close` (or drop).
- Errors: `ASE_NotPresent` is `DeviceNotAvailable`; other `ASIOError`s are `Other` with the SDK text and
  code; `init` false is `DeviceNotAvailable` with the driver's `getErrorMessage`; `CoCreateInstance`
  failures are `Other` with the HRESULT and the DLL path.
- 32-bit sample types: the `ASIOSTInt32LSB16/18/20/24` data sits at the **least** significant end of the
  container (WASAPI's 24-in-32 is left-justified); the slice (b) conversion reads and writes them there.

Hardware (2026-10-22, `cargo test -p transit-audio-asio -- --ignored hardware_ --nocapture
--test-threads 1`): Focusrite USB ASIO 2 in / 2 out `ASIOSTInt32LSB`, buffer 16..1024 step 16 (preferred
256), all four rates; Model Mixer ASIO Driver (`getDriverName` "ModelMixer") 12 in / 10 out
`ASIOSTInt24LSB`, buffer fixed at 256, 44.1 and 48 kHz. The other installed drivers (Ableton, Elektron
Overbridge, Maschine, Focusrite Thunderbolt) fail `init` without their hardware and show up as
diagnostics.

## Streaming (slice b)

Code: `src/session.rs` (host thread), `src/callback.rs` (driver thread, `unsafe`), `src/engine.rs`
(the safe work of a buffer switch), `src/convert.rs`, `src/stats.rs`.

- **One session per driver.** ASIO has one buffer set and one callback per driver, made by one
  `createBuffers` call, and `start`/`stop` act on the whole driver. The first `open_render` or
  `open_capture` loads the driver, sets the rate (`setSampleRate` only when the driver runs at another
  one), negotiates the buffer size and creates the buffers for its direction's first
  `format.channels` channels (every channel, from the plans). An open of the other direction disposes
  and creates the buffers again, for both directions in one call.
- **The open-order rule.** Open both streams of a driver before starting either, in either order
  (capture then render, as the Player and the bench do, or render then capture). Once one has
  started, opening the other fails with "the capture stream must be opened before either stream of
  the driver starts ..."; close the running stream and open both again. Both streams run at one rate
  and one buffer size: a second open at another rate fails, and its period is the session's.
- **Start and stop.** The first `start` starts the driver; a direction that has not started yet is
  not called and its outputs play silence (buffers are zeroed when created: drivers hand them over
  uninitialized). `stop` is final for that stream (reopen, not restart); the driver stops when no
  stream of it runs, and the last drop disposes the buffers, frees the callback state and unloads the
  driver (unless a `Loaded` handle holds it). Dropping a stream is a host request that can wait up to
  `HOST_TIMEOUT`: drop on a control thread.
- **No callback after `stop()` returns.** While the other direction keeps the driver running, `stop`
  and a drop wait (up to 2 s, then a warning) until no buffer switch is inside that direction's
  callback: the switch marks a direction in flight before it reads whether the direction runs, the
  control side changes the state before it reads the mark, both `SeqCst`, so one of them sees the
  other. The last stream's `stop` is the driver's `stop`, which the SDK makes synchronous.
- **Orphaned sessions.** A stream dropped while the host thread is stuck in a driver call cannot send
  its detach; its direction is still marked detached, and the host ends a session whose directions
  are all detached between requests and before every open, so a later open is not refused by a
  running driver nobody owns.
- **Tokens.** `createBuffers` hands back a `Buffers` token that only `disposeBuffers` takes (`start`
  turns it into `Running`, `stop` back); both are `#[must_use]`, and a `Buffers` dropped any other way
  is logged and panics in debug builds, so a path that forgets to dispose fails its test.
- **Buffer size.** `Period::Default` is the driver's preferred size; a requested size snaps to the
  nearest one the driver allows (granularity -1: powers of two between min and max; 0: the one fixed
  size; n: steps of n from min; ties go to the larger). A driver that reports no usable size is an
  `OpenError`, not a panic. `Achieved.period` and `buffer_frames` are the granted size, `min_period`
  the driver's minimum, `latency` the direction's own `getLatencies` figure (render: output; capture:
  input).

### The callback thread

The driver calls `bufferSwitchTimeInfo` (or `bufferSwitch`) on a thread it owns, not the STA host
thread; `asioMessage` and `sampleRateDidChange` come on whatever thread the driver uses, sometimes
the host thread inside `createBuffers`. ASIO callbacks carry no user pointer, so the callback table is
one static and the session's state is one process-wide installed `CallbackState` (one driver per
process makes that sufficient; a second install fails). The callbacks:

- never block, allocate or take a lock: the engine and the buffer addresses sit behind one `busy` flag
  that a callback only *tries* to take (a callback that finds it taken does nothing and is counted);
  the control side takes it, spinning, only before the driver starts or after it stops;
- count themselves in (`IN_FLIGHT`) before reading the state pointer, so uninstalling (after
  `disposeBuffers`) clears the pointer and waits for the count to reach zero before freeing the state
  and the user callbacks;
- call only `outputReady` (after the outputs are filled, when the driver answered `ASE_OK` to it after
  `createBuffers`) and `getSamplePosition` (the `bufferSwitch` path) on the driver object, the two
  calls the SDK makes from inside the buffer switch;
- run the user callbacks under `catch_unwind`: a panic is reported once as `StreamError::Other`, the
  callback is not called again and its outputs play silence.

The callback thread joins MMCSS "Pro Audio" at its first buffer switch after each start; see
[MMCSS](#mmcss-slice-d1).

### Driver messages

`asioMessage`: `kAsioSelectorSupported` (1 for the selectors below), `kAsioEngineVersion` 2,
`kAsioSupportsTimeInfo` 1 (so drivers call `bufferSwitchTimeInfo`), `kAsioSupportsTimeCode` 0,
`kAsioBufferSizeChange` 0 (the SDK: "not yet supported ... use kAsioResetRequest instead"),
`kAsioResetRequest` (counted in `resets`; each open stream gets `StreamError::ResetRequested` once, and
the app reopens the device, [Reset handling](#reset-handling-slice-d1)),
`kAsioResyncRequest` (counted in `resets`; the stamps re-anchor at the next callback),
`kAsioLatenciesChanged` (the host thread refetches `getLatencies` for the stamps), `kAsioOverload`
(counted in `overloads`). `sampleRateDidChange` to a rate other than the session's is a reset
request. An error that arrives on a driver thread is delivered to the owner's error callback under the
`busy` flag by whichever thread gets it next.

### Stamps and their quality

- **Backend domain:** the driver's sample position (`samplePosition` of the time info when its
  `kSamplePositionValid` flag is set, else `getSamplePosition` on the `bufferSwitch` path), in
  nanoseconds at the nominal rate, anchored once per driver start to the QPC read at the first
  callback with a valid position. Both directions share the anchor, so their frame times are
  comparable (what the loopback calibration subtracts). A position that goes back re-anchors; one that
  does not advance by exactly one buffer is counted in `position_jumps`. After either, and after a
  resync request, the next stamp of each running direction is `Quality::Invalid`, so the Player's
  clock fit restarts there (as after a WASAPI discontinuity) instead of bridging the step.
- **Host time:** the QPC read at callback entry, in the stream's `HostOrigin` through the
  `(QPC, Instant)` pair the stream captured at its own `start()` (`origin_pair`/`relative_ns`, as in
  `transit-audio-wasapi`).
- **Frame time:** render presents its first frame `getLatencies` output frames after the buffer switch;
  capture captured its first frame `getLatencies` input frames before it (the SDK's latencies count from
  the buffer switch and include the buffer).
- **Quality:** `BackendEstimate` with a valid position, `Measured` with `OpenRequest.calibration`
  (applied as on WASAPI: the capture frame time moves earlier by the round trip, both streams claim the
  bound), `Arrival` when the driver gives no valid position. The driver's `systemTime` is not used for
  stamps: it is a clock of the driver's choosing (cpal documents `timeGetTime`), and nothing shows it
  finer than a millisecond. The smallest step between consecutive callbacks' `systemTime`, an upper
  bound on its grain, is kept as a diagnostic (logged at debug level when a stream closes): 4 ms on the
  Model 12 (5.8 ms buffers) and 0.17 ms on the Scarlett at 64 frames (the spacing of its bursts).

### Sample formats

`convert.rs` turns the driver's planar per-channel buffers into the trait's interleaved `f32` chunks
and back: `ASIOSTInt16LSB`, packed `ASIOSTInt24LSB`, `ASIOSTInt32LSB`, `ASIOSTFloat32LSB`,
`ASIOSTFloat64LSB` and `ASIOSTInt32LSB16/18/20/24` (valid bits at the least significant end,
sign-extended; bits above them are ignored on read). The integer scaling is
`transit-audio-wasapi`'s (and through it cpal's): mirrored rather than shared, because that module is
crate-private and works on interleaved bytes. MSB and DSD types stay unsupported.

### Counters

`Stream::stats()` per direction: `wakes` (buffer switches served), `wake_late_us` (arrival minus the
previous arrival plus one buffer), `service_us` (entry to the end of the work, `outputReady`
included), `faults` (panicked callbacks), and the driver-wide `overloads`, `busy_skips` (switches
that found the engine still busy, from the previous switch or an error being delivered, and did
nothing), `position_jumps`, `resets` and `timeouts` (the watchdog's stalls). `skipped_wakes`,
`min_padding_frames` and `max_packets_per_wake` stay 0: ASIO has no wait and no padding. The
driver-event tail reads "; overloads N, busy skips N, position jumps N, resets N".

### Measuring it

`audio-bench --backend asio --device "<driver registry name>"` (build with `--features native-asio`;
no SDK and no LLVM) measures the native backend through the trait, with `--period` as the buffer size
asked of `createBuffers` and the usual `--loopback --out-ch --in-ch`. The cpal path (`--backend cpal
--system ASIO`, `--features asio`) stays for comparison. The backend lines of an ASIO run always end
with `overloads, busy skips, position jumps, resets`.

Hardware (2026-10-22, `hardware_scarlett_duplex_streams_two_seconds` and
`hardware_model12_duplex_streams_two_seconds`, capture then render, every channel, two seconds of
silence): the Scarlett at 48 kHz and 64 frames granted 64, reported latencies 188 in / 140 out frames
(572 / 620 at its preferred 256), 1494 buffer switches with late p95 998 us (the burst delivery of known
issue K26), service p95 15 us, 0 overloads, position jumps or resets, every stamp valid; the Model 12 at
44.1 kHz with its fixed 256-frame buffer, 12 in and 10 out of packed `ASIOSTInt24LSB`, latencies
300 / 695 frames, 297 switches with late p95 256 us, service p95 64 us, 0 overloads, jumps or resets.
The loopback rows are in `docs/benchmarks/` (sections "Native ASIO").

Tests: `cargo test -p transit-audio-asio` drives the real host thread, session and callback thunks
against the fake vtable (`stream_tests.rs`), including a zero-allocation check over 200 buffer
switches; `cargo test -p transit-audio-asio -- --ignored hardware_scarlett hardware_model12
--nocapture --test-threads 1` streams the two interfaces for two seconds. (`hardware_control_panel_*`
opens a panel window; run it only on purpose.)

## In the app (slice c)

Code: `crates/playback/src/backend/native.rs` (which backend serves a configuration, the native
listing), `crates/playback/src/player/device.rs` (`open_native`, `open_stepped`),
`crates/playback/src/devices.rs` (`Backend::NativeAsio`, calibration keys),
`crates/ui-studio/src/audio_devices.rs` (the window).

- **Selection.** `AudioConfig.backend` is `NativeAsio` (saved as `"native_asio"`), offered in Audio
  Devices as "Native ASIO (experimental)" when Audio system is ASIO, in a 64-bit Windows build with
  `transit/native-asio`. Each system offers only its own native backend (`Backend::offered`); a native
  choice that does not serve a newly chosen system goes back to Auto. Without the feature the choice
  is not offered and a saved `NativeAsio` opens cpal.
- **Listing.** With the feature, the ASIO system of the Audio Devices lists comes from this crate's
  `Backend::endpoints` (registry names, probed channel counts, the standard rates and buffer range the
  formats cover), whichever backend then opens it, not from cpal's ASIO host. Drivers that could not be
  probed because this process holds another one are still offered by registry name with unknown
  counts; drivers that failed to load are listed under the Output list as "Not loaded: <driver>:
  <problem>" (`AsioBackend::diagnostics`, minus the `blocked` ones). While cpal's ASIO host holds a
  driver nothing is probed (the registry names only), so two drivers are never loaded at once.
- **Open order in the Player.** `Player::open_native` runs the same `open_stepped` ladder as native
  WASAPI, with no mode step (ASIO has one mode; `share_device` does not apply). Because
  `capture_with_render` is true, `open_stepped` builds the input first (`Recorder::build_on`, an
  `open_capture` on the render endpoint), then opens the output, then starts the input, then the
  output: both opens before either start, which is this crate's open-order rule. The Player already
  ran in that order for cpal ASIO, so nothing was restructured. A native open error falls back to the
  cpal adapter before `start` only ("Native ASIO failed: ...; using the cpal backend").
- **Multichannel.** The plans open every channel (`format.channels` is the driver's count), so the
  Model 12 opens 12 in / 10 out; `IoMap::for_device` gives every channel a path ("Out 9-10", "In 12",
  "In 11-12"), keyed `("ASIO", <registry name>)` as cpal's ASIO tables were. `Backend::channel_names`
  carries `getChannelInfo`'s names ("IN 1" .. "IN 12", "OUT 1" .. "OUT 10" on the Model 12; "Input 1",
  "Output 1" on the Scarlett) to the Player and the channel picker. Hardware inserts and stems route to
  them through the ordinary path table (tests: `a_hardware_insert_is_measured_through_the_model12s_last_channels`,
  `an_insert_on_the_last_channels_of_a_12_in_10_out_device_sends_and_returns_there`).
- **What opened.** `Achieved` fills the Output open and Input open lines (buffer, `i24`/`i32`, the
  direction's `getLatencies` figure) and the load readout says `ASIO*` (cpal's ASIO host stays `ASIO`).
- **Calibration** is keyed `(driver registry name, input None, mode "asio", rate, period)`. The bench
  writes none yet (known issue K31: the drivers' `getLatencies` figures over-report the loopback, so a
  stamp-time calibration finds no pair), so the Calibration line says the driver's figures are used.
- **Input on demand.** An input that does not open with the output (`HeldInput::No`, native WASAPI
  without a pairing) opens through the native backend that serves the output, cpal only if that fails.
- **Reset.** `StreamError::ResetRequested` stops the output like a lost device
  (`OutputTrouble::ResetRequested`), and the app reopens the device by itself:
  [Reset handling](#reset-handling-slice-d1).
- **Driver settings.** The button opens the running driver's panel through `Loaded::control_panel`
  on the host thread (the running session's driver is shared with the handle), called from a thread
  of its own so a modal panel does not hold the UI; the status line says when it closed. While the
  panel is open the host thread is busy, so Reconnect, Apply and Rescan Devices wait for it to
  close (they are disabled, and a listing keeps the last list), and an open refused by that blocked
  host does not fall back to cpal, which would load the same driver twice. A reset the driver asks
  for while its panel is open is reopened when the panel closes.

Hardware (2026-10-22, `cargo test -p soundcraft-playback --features native-asio --lib -- --ignored
hardware_player_opens_native_asio --nocapture --test-threads 1`): the whole Player path opens the
Scarlett at 48 kHz, 64 frames (2 in / 2 out, `i32`, 2.9 ms out / 3.9 ms in) and the Model 12 at
44.1 kHz, 256 frames (12 in / 10 out, `i24`, 15.8 ms out / 6.8 ms in) and runs two seconds with no
backend error. The ADR 0015 hardware-insert click train through the Player
(`hardware_player_measures_a_hardware_insert_on_the_*`, on the bench's loopback cables) measured
326 frames (6.79 ms) on the Scarlett, output 2 to input 2 at 64 frames, against the bench's 294 to 300
(the probe's view includes the input ring, known issue K22), and 984 frames (22.3 ms) on the Model 12,
output 2 to input 1, every click found; without clearing the input ring after the open the Model 12
read 1239 frames, one buffer more. On drivers whose loop phase changes between starts, such as the Model 12 (974 or 1239 frames, one buffer apart), remeasure hardware inserts after a reconnect.

## Reset handling (slice d1)

Code: `crates/playback/src/reopen.rs` (`Reopener`), `Player::reopen_cause` and `Player::host_blocked`
(`crates/playback/src/player/device.rs`), `StudioApp::step_reopen` and `stop_for_reopen`
(`crates/ui-studio/src/host.rs`). Issue #19.

- **Close both, reopen both.** A reset request (or a rate change under the streams) delivers
  `StreamError::ResetRequested` to both directions; the Player silences the output and reports
  `reopen_cause() == DriverReset` for as long as it lives (unlike `trouble()`, which is said once).
  The app then replaces the Player through its one reopen path, `reconnect_audio_using`, as
  Reconnect does: the old streams are stopped and dropped (disposeBuffers, the driver released), the
  new Player opens with the same `AudioConfig` (same driver, rate, channels and requested buffer
  size), the input first and then the output (the open-order rule), and `createBuffers` negotiates
  against the driver's `getBufferSize` as it is now, so a buffer size changed in the driver's panel
  is picked up. Monitoring and armed tracks take the reopened input again (`ensure_input`). The
  plug-ins keep their live state (`capture_states_for_reopen`), but they are re-created and their
  editors close and reopen, as on Reconnect (known issue K32).
- **Never a splice.** On the reset the transport stops for good (`stop_for_reopen`): a take in
  progress ends there and is kept ("Recording ended: driver reset. The take is kept up to there.
  Reopening the device."), and since the faulted runtime acknowledges nothing, the transport is
  stopped on the UI side; the runtime gets no more session, I/O or transport work until it is
  replaced. Recording resumes only when the user records again.
- **When.** `Reopener` waits 0.25 s for the driver to settle, then makes at most three attempts
  (waits 0.25, 1 and 3 s); an attempt succeeds when the new Player runs (not silent, no reopen
  cause). A reopened device must run 10 s before the count starts afresh, so a driver that keeps
  asking runs out of attempts instead of looping. After the third failure the status says to use
  Audio › Reconnect Device, as before slice d1. A manual Reconnect or Apply starts the count afresh.
- **The settings panel.** While the driver's settings panel is open (the window's panel flag, or the
  backend's `host_blocked`), nothing is attempted and no attempt is used: the panel holds the host
  thread, and a reopen then would be refused. The panel's close message says the device reopens.
- **What it says.** The status line says what happened and what happens next; Audio Devices shows a
  "Recovery:" line with the state and the counts ("resets 1, stalls 0, reopened 1"). The streams'
  own `resets` counter starts from zero with each reopen.
- **Tests:** `crates/playback/src/reopen.rs` (the policy on a fake clock),
  `a_driver_reset_*` and `a_device_that_does_not_reopen_*` in `crates/ui-studio/src/audio_devices_tests.rs`
  (the whole app over the scripted native ASIO driver).

## The watchdog (slice d1)

Code: `crates/audio-asio/src/watchdog.rs`, `Session::tend`/`watch`, `CallbackState::stalled`.

- The callback stores the QPC of each buffer switch's entry (one relaxed store); the host thread
  compares it with the clock between requests, waking at least every quarter of the limit while the
  driver runs. Nothing waits or spins on the driver's thread.
- The limit is eight buffers, at least 250 ms (`STALL_PERIODS`, `STALL_FLOOR`); until the first
  switch after a start the driver gets at least a second (`FIRST_CALL_GRACE`). It is checked only
  while a stream of the driver is started and fires once per driver start.
- A stall is counted in `timeouts` (driver-wide, apart from `resets`) and each open stream gets
  `StreamError::Stalled` once, delivered under the `busy` flag like a reset. The Player treats it
  like a reset (`OutputTrouble::Stalled`, `reopen_cause() == DriverStalled`), so the app reopens the
  device through the same path; the "Recovery:" line counts stalls apart.
- It does not run while a driver call holds the host thread (the settings panel, a stuck request);
  see known issue K34.
- **Tests:** the limit, grace and once-per-start rule on a fake clock (`watchdog.rs`); through the
  real host thread on the fake driver, `a_driver_that_stops_calling_back_is_reported_stalled_once_by_the_watchdog`
  and `the_watchdog_leaves_a_driver_that_calls_back_and_a_stopped_one_alone` (`stream_tests.rs`).
  The stream tests that play the driver by hand use a watchdog floor of an hour.

## MMCSS (slice d1)

Code: `crates/audio-asio/src/mmcss.rs` (the state, safe), `com.rs` (`avrt.dll` bound by hand through
`raw-dylib`: `AvSetMmThreadCharacteristicsW`, `AvRevertMmThreadCharacteristics`; no new crate).

- The driver calls back on a thread it owns, so it cannot be promoted at its start the way the
  WASAPI worker is. Each driver start arms the session's registration; the first buffer switch
  registers its own thread with the MMCSS task "Pro Audio" (one OS call per start, not per buffer;
  the callback otherwise makes one atomic load). A driver that calls back on the host thread is
  left as it is.
- Before the driver's `stop`, the host asks the registered thread to revert at its next switch and
  waits up to three buffers (20 to 200 ms); a driver that does not call again by then is reverted
  from the host thread, and that is logged (known issue K33).
- `Stream::thread_priority` reports it: `Pending` before the first switch, `MmcssProAudio` while it
  runs, `PromotionFailed` when MMCSS refused (logged at the stop with the OS error; playback goes
  on), `Released`/`ReleaseFailed` after the stop, `Unobserved` on the host thread. The ignored
  hardware stream tests print it and expect `MmcssProAudio` while running and `Released` after.
- **Tests** use counting hooks (no test thread is really registered): `mmcss.rs` and
  `the_buffer_switch_thread_joins_mmcss_*`, `a_driver_that_does_not_call_at_the_stop_*` in
  `stream_tests.rs`; `the_host_thread_runs_above_normal_priority` in `host.rs`.

## Next (slice d2 and later)

- **Listing:** probe only the selected driver, and list off the UI thread.
- **Stop from inside the callback.**
- **Calibration:** the stamp-time loopback calibration cannot pair clicks while the drivers'
  `getLatencies` figures over-report the loop (known issue K31: the Focusrite driver always, the
  Model 12 in the phase that measures 22 ms); the frame-time formulas need the measured offset before
  `audio-bench --settings` can write an `asio` calibration and stamps reach `Measured` in the app.
- A stream-only reopen inside the Player (known issue K32).
- Slice (e): retiring cpal's ASIO path and `crates/asio-panel` (the cpal path's "Driver Control Panel").

Open: the WOW6432Node (32-bit) drivers are not listed; whether a message-only `sysRef` suits every
driver's control panel is untested; a driver whose `init`, `createBuffers` or `controlPanel` waits for
a thread that `SendMessage`s to `sysRef` makes the request time out (the host pumps only between calls;
see the module documentation of `host.rs`), and a modal panel holds the host thread, so other requests
(a reopen) wait behind it.
