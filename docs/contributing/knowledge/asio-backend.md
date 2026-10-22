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
one by name (for example `hardware_scarlett_duplex_streams_two_seconds`). The app's own ASIO path is
still cpal's until it is wired to this crate; its build is in [docs/building.md](../../building.md).

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
  sleeps in `MsgWaitForMultipleObjects` on that event and its queue.
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

The callback runs at the driver's thread priority (`WorkerPriority::Unobserved`); no MMCSS promotion
is attempted (the amendment defers that to slice d).

### Driver messages

`asioMessage`: `kAsioSelectorSupported` (1 for the selectors below), `kAsioEngineVersion` 2,
`kAsioSupportsTimeInfo` 1 (so drivers call `bufferSwitchTimeInfo`), `kAsioSupportsTimeCode` 0,
`kAsioBufferSizeChange` 0 (the SDK: "not yet supported ... use kAsioResetRequest instead"),
`kAsioResetRequest` (counted in `resets`; each open stream gets `StreamError::ResetRequested` once),
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
  does not advance by exactly one buffer is counted in `position_jumps`.
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
included), `faults` (panicked callbacks), and the driver-wide `overloads`, `position_jumps` and
`resets`. `timeouts`, `skipped_wakes`, `min_padding_frames` and `max_packets_per_wake` stay 0: ASIO
has no wait and no padding. A driver that stops calling is not detected yet (no watchdog).

### Measuring it

`audio-bench --backend asio --device "<driver registry name>"` (build with `--features native-asio`;
no SDK and no LLVM) measures the native backend through the trait, with `--period` as the buffer size
asked of `createBuffers` and the usual `--loopback --out-ch --in-ch`. The cpal path (`--backend cpal
--system ASIO`, `--features asio`) stays for comparison. The backend lines of an ASIO run always end
with `overloads, position jumps, resets`.

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

## Next

Slice (c): wire the Player (`AudioConfig.backend` gains the native ASIO choice; the cpal adapter's ASIO
order rule, `ASIO_OUTPUT` cache and `capture_with_render` special case are not needed by this backend,
whose rule is only "open both before starting either"), hardware inserts over the native path. Slice
(d): the reset and rate-change errors reaching the Fault path in the Player, the panel in the UI, a
watchdog for a driver that stops calling, the MMCSS decision, the loopback calibration reaching
`Measured` in the app. Slice (e): retiring cpal's ASIO path and `crates/asio-panel`, which today still
acts on cpal's loaded driver. Open: the WOW6432Node (32-bit) drivers are not listed; whether a
message-only `sysRef` suits every driver's control panel is untested; a driver whose `init`,
`createBuffers` or `controlPanel` waits for a thread that `SendMessage`s to `sysRef` makes the request
time out (the host pumps only between calls; see the module documentation of `host.rs`).
