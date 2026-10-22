# Native ASIO backend

Code: `crates/audio-asio` (`transit-audio-asio`). Decision: the draft native-ASIO amendment to
[ADR 0016](../../adr/0016-native-wasapi-backend.md) (not yet accepted); unsafe policy:
[ADR 0013](../../adr/0013-unsafe-only-in-soundcraft-rt.md) and [`unsafe.md`](unsafe.md). 64-bit Windows
only; the crate is empty elsewhere. No ASIO SDK, C++, bindgen or LLVM in its build.

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
releases its reference on drop. All `unsafe` is in `com.rs`, each block with a `// SAFETY:` comment
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
  container (WASAPI's 24-in-32 is left-justified); slice (b)'s conversion must shift accordingly.

Hardware (2026-10-22, `cargo test -p transit-audio-asio -- --ignored hardware_ --nocapture
--test-threads 1`): Focusrite USB ASIO 2 in / 2 out `ASIOSTInt32LSB`, buffer 16..1024 step 16 (preferred
256), all four rates; Model Mixer ASIO Driver (`getDriverName` "ModelMixer") 12 in / 10 out
`ASIOSTInt24LSB`, buffer fixed at 256, 44.1 and 48 kHz. The other installed drivers (Ableton, Elektron
Overbridge, Maschine, Focusrite Thunderbolt) fail `init` without their hardware and show up as
diagnostics.

## Next

Slice (b): `createBuffers` for every channel, the callbacks (`bufferSwitchTimeInfo` first), sample
conversion (packed 24-bit first), buffer-size negotiation, stamps, `outputReady`. Then capture and
duplex (c), stats, reset and rate-change events and the panel in the UI (d), and retiring cpal's ASIO
path and `crates/asio-panel` (e), which today still acts on cpal's loaded driver. Open: the WOW6432Node
(32-bit) drivers are not listed; whether a message-only `sysRef` suits every driver's control panel is
untested (the panel hardware test is ignored and was not run).
