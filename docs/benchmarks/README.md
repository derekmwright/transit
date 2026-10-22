# Audio benchmarks

The harness behind the gate of [ADR 0016](../adr/0016-native-wasapi-backend.md) (section 5). It lives in
`tools/audio-bench` and runs through `cargo xtask audio-bench`. It drives a backend through the
`transit-audio-backend` trait only, so cpal-shared, cpal-ASIO and the future native backend are
measured by identical code, and by host time only.

Hardware numbers are notes, never CI gates. Fake-device results prove the harness logic, not any
hardware (see `../contributing/evidence/hardware-and-ci.md`). The fake mode runs inside `cargo xtask ci`.

## Method

Every figure comes from the host clock reading (`HostOrigin`, a monotonic `Instant`; QPC on
Windows) taken at callback entry, as the backend reports it in `Stamp::host_callback_ns`.

- **Achieved period.** The median block size in frames, and in ms at the stream rate. The requested
  period is only a request; a backend may refuse it or round it. If the blocks vary, the table shows
  the range. Compare backends at their own achieved period, never at a common requested one.
- **Jitter.** The deviation of the arrival interval of consecutive render callbacks from the achieved
  period: p50, p95 and max, in ms. The first 8 callbacks are left out (drivers deliver them back to
  back while they fill their buffers).
- **Xruns.** The trait reports none today, so an xrun is an arrival gap longer than 1.5 periods,
  counted on the render stream and on the capture stream (`out/in`). "Xruns/min" is the render count
  over the measured span. The Notes column counts calls of the backends' error callbacks, invalid
  stamps and any log entries lost to a full log. A gap is evidence of a late callback, not proof of an
  audible glitch, and a backend that buffers more deeply may hide an underrun from this method.
- **Callback time.** Wall time spent in the harness's render callback (p50, p95, in us). The harness
  callback does almost nothing, so this is the floor of the backend's own overhead as seen from
  inside the callback, not a CPU load figure.
- **Round trip** (with `--loopback`). The render callback writes silence except for a click every
  500 ms: a one-sample impulse followed by a 1 ms raised-cosine burst of a 3 kHz tone (peak 0.5, about
  -6 dBFS; a tone so that an AC-coupled input passes it). The harness records the host time at the
  entry of the render callback that emitted each click and the click's frame offset in that block. The
  capture callback runs a matched filter (normalized correlation against the known click, with an
  energy floor of -14 dB relative to the emitted level) and records the host time at the entry of the
  capture callback that carried it and the frame offset. Round trip = (capture host time + offset /
  rate) - (render host time + offset / rate), reported as min / median / p95 in ms and in frames.
  Render and capture are separate streams, so this works for any backend that can open both, with no
  shared clock beyond the host clock.
  - **Bias.** Both ends are callback arrival times, so the figure includes the dispatch jitter of both
    callbacks: read the spread (min to p95) as the uncertainty. It is not a calibrated latency. A
    device-position round trip is an additional native-backend column for later.
  - Pairing accepts round trips up to 450 ms. The audio thread allocates nothing and takes no locks;
    fixed-size logs are filled while the stream runs and analyzed after it stops.

## Loopback cabling

Connect an output of the interface to one of its inputs with a short cable (or use the interface's
software loopback channel if its driver exposes one as an input). The channels are 1-based:
`--out-ch 1 --in-ch 1` means output 1 to input 1. Turn the input gain down to unity or lower, do not
engage phantom power or a pad that changes the level a lot, and keep the monitor level low (the click
is audible on the monitors that share the output). Check that the table says the click was found
(`n of n` after the round trip); "no click detected" means the cable, the channel numbers or the
levels are wrong.

## Command lines

Close other audio programs first. Run each command from the repository root. The harness builds in
release; the first run compiles. `--seconds 120` for a baseline, the default 20 for a check.
`--system` and `--device` take the names the app shows in Audio Devices; `--list-devices --system X`
prints them without opening a stream. The ASIO runs need a build with `--features asio` on the
harness (`cargo xtask audio-bench` forwards its arguments only; for ASIO run
`cargo run --release -p audio-bench --features asio -- ...` with the same arguments).

Focusrite Scarlett 2i2 (2 in, 2 out), once per period 32, 64 and 128 (and a re-run of each with a 70
percent CPU load running elsewhere, as ADR 0016 section 5 asks):

```
cargo xtask audio-bench --backend cpal --system ASIO  --device "Focusrite USB ASIO"  --period 64 --seconds 120 --loopback --out-ch 1 --in-ch 1 --out docs/benchmarks/<date>-<machine>.md
cargo xtask audio-bench --backend cpal --system WASAPI --device "Speakers (Focusrite USB Audio)" --in-device "Line (Focusrite USB Audio)" --period 64 --seconds 120 --loopback --out-ch 1 --in-ch 1 --out docs/benchmarks/<date>-<machine>.md
```

TASCAM Model 12 (loopback on one output / input pair, for example output 1 to input 1):

```
cargo xtask audio-bench --backend cpal --system ASIO  --device "TASCAM MODEL 12"   --period 64 --seconds 120 --loopback --out-ch 1 --in-ch 1 --out docs/benchmarks/<date>-<machine>.md
cargo xtask audio-bench --backend cpal --system WASAPI --device "<output endpoint of the Model 12>" --in-device "<input endpoint of the Model 12>" --period 64 --seconds 120 --loopback --out-ch 1 --in-ch 1 --out docs/benchmarks/<date>-<machine>.md
```

The device names above are examples; use `--list-devices`. WASAPI lists each endpoint of an interface
separately (a render endpoint and a capture endpoint), hence `--in-device`; ASIO is one device for
both directions. `--out` appends rows to an existing file that has the table, so one file can hold a
session. Without `--loopback` the round trip columns read `n/a`.

## Calibrating the native backend

The native WASAPI backend stamps its streams as `Measured` only for a device pair whose loopback
round trip was measured in the stamps' own time (ADR 0016 slice d; method in
`../contributing/knowledge/clocks.md`). With the output cabled to an input of the same interface,
close the app and run, from the repository root, once per rate and buffer size the app uses
(`--period` and `--rate` are part of the calibration's key, and `--in-device` is the string the
app's Paired input holds):

```
cargo run -p audio-bench --features native-wasapi --release -- --backend wasapi --loopback --out-device "<Scarlett out>" --in-device "<Scarlett in>" --settings "%APPDATA%\transit\ui.json"
```

It prints the round trip in frames and ms with its uncertainty (`+-` the spread of the per-click
round trips plus one frame) and merges it into `audio.calibrations` of that file. The app must be
closed: it rewrites its `audio` block from memory when it saves. The fake mode proves the logic
without a device: `cargo xtask audio-bench --backend fake --loopback --seconds 3`. A run that
detects too few clicks prints the peak level of every input channel (`-inf dBFS` is a silent
channel); the clicks play at -6 dBFS and a detection needs about -20 dBFS on the input channel
(`--in-ch`), so a quieter or silent channel points to the cable, the channel or the input gain.

The hardware tests open the owner's default endpoints and stay `#[ignore]`d; run them by hand:

```
cargo test -p transit-audio-wasapi -- --ignored hardware_ --nocapture
cargo test -p soundcraft-playback --features native-wasapi -- --ignored hardware_ --nocapture
```

Without a device (what CI runs; opens no audio device and finishes at once, in virtual time):

```
cargo xtask audio-bench --backend fake --seconds 2 --loopback --out target/fake.md
```

Also record, once per interface, `GetSharedModeEnginePeriod` (default, fundamental, minimum and
maximum period in frames) from the Windows audio engine and paste it under the table; the harness
does not read it yet (it arrives with the native backend).

## Results

The gate is fixed by amendment to ADR 0016 after this baseline is filled in (starting value: a round
trip within 1.5 ms of ASIO at 64 frames). Fill one row per run; keep the raw `--out` files next to
this README as `<date>-<machine>.md`.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| | | | | | | | | | | | | | | | |

`GetSharedModeEnginePeriod`:

| Interface | Default | Fundamental | Minimum | Maximum |
|---|---:|---:|---:|---:|
| Scarlett 2i2 | | | | |
| TASCAM Model 12 | | | | |
