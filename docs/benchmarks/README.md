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
- **Xruns.** The trait reports none today, so an xrun is a sustained drift step: the arrival of each
  block is compared with when its frames were due (the first arrival after the warm-up plus the frames
  delivered since, at the nominal rate), the straight line of the device clock's rate error is removed
  by least squares, and each step of the residual of more than half a period (low percentile, p25, of the next
  window against that of the previous one, windows of 8 callbacks) counts once. Bursty delivery
  (a two-period wait, then two callbacks back to back) is a spike that returns to the baseline and
  counts nothing; a skipped buffer leaves the stream behind wall time for good and counts one. The
  render stream also counts every callback whose body time exceeded the period, because on ASIO a
  host-side overrun does not change the callback cadence. Both are reported on the render stream and
  the capture stream (`out/in`; the capture side has no body-time count). "Xruns/min" is the render
  count over the measured span. The Notes column counts calls of the backends' error callbacks, invalid
  stamps and any log entries lost to a full log. A drift step is evidence of lost frames, not proof of
  an audible glitch, and a backend that buffers more deeply may hide an underrun from this method.
  On the native WASAPI backend the "render backend:" and "capture backend:" lines are the direct figures.
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
(`--period` and `--rate` are part of the calibration's key; the record matches when `--in-device`
names the app's Paired input, or, with Paired input on Automatic, the input the app pairs
automatically with that output, compared without regard to case; a run that names no input
matches Automatic only if the record names none):

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

## Exclusive mode

`--mode exclusive` (with `--backend wasapi`) opens both streams in WASAPI exclusive mode, which skips
the Windows audio engine and runs at the driver's period: for example
`cargo run -p audio-bench --features native-wasapi --release -- --backend wasapi --mode exclusive --period 128 --loopback --out-device "<Scarlett out>" --in-device "<Scarlett in>"`.
While it runs it holds the device and every other app on it is silenced; a device another app holds
exclusively fails with "in use", and one whose Advanced settings forbid exclusive control says so. The
Notes column gives the format and period the driver granted ("exclusive, i32/24 valid, 128 fr"), and
with `--settings` the calibration is stored under the mode `exclusive`, apart from the shared one
(see `../contributing/knowledge/wasapi-modes.md`). The fake backend accepts the flag as if shared.
Without `--period` the stream runs at the device's default period; a smaller `--period` is honored as
asked, and the table shows what it costs.

On the Focusrite Scarlett 2i2 (2026-10-22, [run](2026-10-22-ryzen-5900x-scarlett-2i2.md)) exclusive mode
at the 480-frame (10 ms) default is clean but its round trip is still about 223 ms in host time (203 ms
+-12 ms calibrated in stamp time), against 248 ms shared and 23 ms through ASIO on the same cable. So on
this device the roughly 200 ms sits inside the Focusrite WDM driver, not in the Windows audio engine,
and ASIO stays its low-latency path. Below the default the driver signals only every other period
(992 output xruns in 6 s at 144 frames, 213 at 256; known issue K25).

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

Runs: [2026-10-22 AMD Ryzen 9 5900X, Scarlett 2i2, WASAPI shared (native and cpal), WASAPI exclusive, ASIO buffer sweep, re-measured with the drift detector, native ASIO sweep](2026-10-22-ryzen-5900x-scarlett-2i2.md); [2026-10-22 AMD Ryzen 9 5900X, TASCAM Model 12, WASAPI shared (native and cpal), WASAPI exclusive and mixed modes, native ASIO](2026-10-22-ryzen-5900x-tascam-model12.md); [2026-10-23 AMD Ryzen 9 5900X, Scarlett 2i2, ASIO driver-reported vs measured round trip (K31)](2026-10-23-ryzen-5900x-scarlett-2i2-asio-reported-vs-measured.md); [2026-10-24 AMD Ryzen 9 5900X, Behringer Model D over the TASCAM Model 12 and the Scarlett 2i2, MIDI-to-audio onset latency (#71)](2026-10-24-behringer-model-d-midi-to-audio-onset-latency.md).

## MIDI-to-audio onset latency

`--midi-roundtrip` (issue #71) times a hardware synth played over MIDI: the note goes out of a MIDI
output port and the synth's audio comes back into an input of the interface. It measures the
**end-to-end MIDI-to-audio onset latency** of that chain:

    L = MIDI delivery + synth response + audio input

L is the effective compensation offset for the chain: what an External Instrument track needs to
land the synth on the grid. It does **not** isolate MIDI transport accuracy. The instant the OS and
the interface actually transmit the bytes is not observable here. The send instant is
sample-accurate only in the scheduling and timestamp sense. No audio output is in the chain, so the
driver's output latency plays no part. The input side is counted from the driver's position time,
so "audio input" means the driver's input latency plus any constant offset between the driver's
position time and its callback dispatch. The run also prints L minus the driver's reported input
latency, as an *estimate* of MIDI delivery plus synth response. The driver's figure is unverified
(known issue K31), and the dispatch offset is not in it.

Wiring: the interface's (or a USB) MIDI out goes to the synth's MIDI in, and the synth's audio out
goes to an interface input (`--in 1`, or `--in 1,2` to sum a stereo pair). Set the synth to a patch
with its **fastest attack and shortest release**, no delay or reverb, no arpeggiator, and receiving on
`--midi-channel`. Turn its volume and the input gain so a note peaks well above the noise (around
-20 to -6 dBFS). Keep the monitors low. Close transit and other audio programs, then run from the
repository root:

```
cargo run -p audio-bench --features native-asio --release -- --midi-roundtrip --midi-out "<port name or part>" --midi-channel 1 --in 1 --backend asio --device "<ASIO driver>" --rate 44100 --buffer 256 --trials 10 --gap-ms 1500 --json docs/benchmarks/<date>-<machine>-<synth>.json
```

Native ASIO only: both ends are counted in the driver's sample positions. Defaults: note 60,
velocity 100, 10 trials, 1500 ms gap, 1000 ms window (`--max-ms`), 0.5 ms limit on the clock fit's
standard error at a send (`--max-fit-error-ms`; it replaced `--max-fit-residual-ms`, which the
Model D runs used), one unmeasured warm-up note (`--no-warmup` skips it). The gap must outlast the
synth's release. On a Behringer Model D a 750 ms gap left the pre-send window at about -67 dBFS
against a -86 dBFS floor.

Method:

- **Send instant.** The control thread reads the host clock (`HostOrigin`, QPC) right before the
  send call. The bench's position clock turns it into the device's sample position: a least-squares
  line of the capture blocks' arrival time against their first sample position, over the last 2 s of
  blocks (never fewer than 32), with points far outside the spread dropped once. It is the same kind
  of map the app's MIDI output thread uses (ADR 0017), over a longer window, so it converges where
  the app's fitter (about 64 points, 2 ms residual limit) does not: the Scarlett 2i2 at buffer 256,
  whose buffers arrive with about one block of scatter. The app's fitter is fed the same blocks and
  printed beside it ("the app's clock map ... valid at k of n sends"). The warm-up waits for 1 s of
  fit. If no fit comes within 3 s, the error gives the block count, the arrival spacing against the
  block length, the fit scatter and the fitted rate.
- **Fit figures.** Three are printed. The *clock-fit scatter* is the largest distance of a block from
  the line: how irregular the arrivals are, and **not a bound on the send offset** (the line averages
  over hundreds of blocks). The *standard error at the send* is the line's own uncertainty at the
  send position, the send-position uncertainty. The *leave-last-out prediction error* is the newest
  block against the line fitted without it. A trial whose standard error is above
  `--max-fit-error-ms` is skipped before any note is sent; more than half skipped refuses the run. A
  constant offset of the arrivals (the driver's dispatch delay) is in the line and in every L alike;
  no fit sees it. The send call's own time is inside L, because the instant is read before the call.
- **Warm-up note.** One note is played and released before trial 1 and not measured, so a
  first-note effect of the port or the synth stays out of the trials.
- **Onset.** The input is recorded by sample position. The detector takes a 0.25 ms causal envelope
  (the mean absolute value). Its threshold is the noise floor plus 12 dB (never below -60 dBFS). The
  floor is the lowest envelope peak of the 100 ms reference window before the first note and of
  every trial's 100 ms pre-send window, so a tail in one window (a previous run's note) does not set
  it; at the end every onset is found again against that floor. The detector finds the first crossing,
  then refines it to the first sample where the envelope exceeds 10 percent of the attack's peak
  within the next 20 ms. That refined sample is the onset. The distance between the two crossings is
  printed as the detector's uncertainty. It bounds the late side only: the true start, before the
  threshold crossing, is not bounded. A trial whose onset lies more than 1 ms after its crossing
  gets a slow-attack warning.
- **Detector bias.** The onset is late by the attack's rise to 10 percent of its peak, plus up to one
  envelope window less one sample (11 samples at 48 kHz) of envelope lag. The run prints this bias
  on its own line. It is part of L, and nothing is subtracted for it. A fast attack keeps it to a
  sample or two.
- **Per trial.** A pre-send window more than 6 dB above the floor gets a warning, and the trial
  waits another gap and reads it again once. If it is still above, the trial runs with its threshold
  raised to 12 dB over that window (a release still sounding must not count as the onset). A pre-send
  window above -20 dBFS is refused: raise `--gap-ms` or shorten the release. A trial with no onset
  within `--max-ms` is also refused, with the level heard. That points to the channel, the port, the
  input or the level.
- **Safety.** Every exit path sends the note-off and All Notes Off (CC 123) on the channel: errors, a
  refusal and Ctrl+C included. The Ctrl+C handler only sets flags. The first press stops the run,
  which releases the note and exits with status 130. A stop seen before a note-on means that note is
  never sent, including a Ctrl+C during the device open. A further press exits at once only while
  no note-on is outstanding. While one is, the press waits for the release. Closing the console
  window, killing the process or a power loss are not covered. In those cases, silence the synth by
  hand.

The output lists every trial (samples, ms, detector uncertainty). It then gives the distribution:
mean, median, min, max and standard deviation, with the trials beyond 2 standard deviations listed
separately as outliers. There is no pass or fail. USB MIDI moves in 1 ms frames, so a spread of
about 1 ms is expected. Last come the input-latency estimate and the exact command. `--json` writes
the same figures, every trial and the command line.

Results entry (one per synth and chain; keep the JSON next to this README):

```
### <Synth>, <MIDI interface or port> -> <audio interface> input <n>, <rate> Hz, buffer <frames>
- Date, machine: <date>, <CPU>, <OS>
- Patch: <name; attack, release>
- Effective compensation offset: mean <x> ms (<n> samples), median <x> ms, min <x>, max <x>, std dev <x> ms, n = <n>
- Outliers: <none | trial k, ...>; detector uncertainty max <n> samples
- Driver input latency <n> samples; estimated delivery + synth response <x> ms (estimate)
- Command: `<as printed>`; JSON: [<file>](<file>)
```

## Placement against the grid

`--placement` (issue #71) checks a compensation instead of measuring one: does sending each note
early by the measured offset put the synth's onsets on the grid? It schedules a pattern of note-ons
at intended device sample positions P (default 8 notes, 500 ms apart, the first 250 ms after
scheduling), sends each at P minus the compensation, finds each onset O as the round trip does, and
reports the placement error per note:

    E = O - P   (samples and ms)

With `--compensation` it plays the pattern twice and prints the two runs side by side:
**uncompensated** (sent at P; E is the round trip L of each note) and **compensated** (sent the
compensation early; E is what is left). Then the mean, median, standard deviation, min and max of
each run, and one reading line, "compensated placement within +-X ms" (the largest |E| of the
compensated run). Without `--compensation` it plays once and suggests the mean as the compensation.

```
cargo run -p audio-bench --features native-asio --release -- --placement --compensation <samples, or <ms>ms> --midi-out "<port name or part>" --midi-channel 1 --in 1 --backend asio --device "<ASIO driver>" --rate 44100 --buffer 256 --json docs/benchmarks/<date>-<machine>-<synth>-placement.json
```

Options beside the round trip's (all but `--trials`): `--compensation 1070` (samples) or
`--compensation 24.3ms`; `--pattern-ms 0,500,1250,...` (onsets from the pattern's start, increasing,
at least 50 ms apart) or `--pattern-count <n> --pattern-spacing-ms <ms>`; `--lead-ms` (default 250).
The compensation must stay within the lead either way, or the run is refused before anything opens:
a positive one at or above the lead would be due before it was scheduled, a negative one beyond it
would send every note more than the lead after its position. `--gap-ms` is the wait after
the warm-up note and between the two runs. Each note is held 100 ms (at most half the smallest
spacing).

How the sends are timed: each deadline is the host time of P minus the compensation by the
bench's position clock, recomputed from the freshest fit while waiting, and the wait is the MIDI
output thread's: sleep until 300 us before the deadline, then yield until it has come. How late
each send landed against its deadline is printed ("sends against their deadlines"); it is inside E.

How to read it:

- **Uncompensated E** is the round trip of each note: it should agree with `--midi-roundtrip`'s L on
  the same chain within the run-to-run spread.
- **Compensated E** should sit near 0, within the synth's attack variation and the clock's noise
  (the detector's late bias of a sample or two remains, as in L). A mean clearly away from 0 means
  the offset is wrong for this chain, rate or buffer; the mean of the uncompensated run is the better
  figure.
- **The spread** of the compensated run is the synth plus the clock; it is what no stored offset can
  remove. The detector, fit and warning lines are the round trip's.
- A note whose pre-send window (the 100 ms before it, or a quarter of the spacing) is more than 6 dB
  above the floor gets a warning: the previous note's release was still sounding. Its threshold is
  raised over it; for a long release, raise `--pattern-spacing-ms`.

Safety is the round trip's: one guard releases the note on every path, and the same Ctrl+C rules.

## Call for testers

Hardware numbers from other machines are the most useful addition to this folder. See
[How to help](../../README.md#how-to-help) for what to run and send, and file results with the
[measurement issue template](https://github.com/transit-audio/transit/issues/new?template=measurement.md).

### Measured on

| Interface | Driver paths measured | Run | Contributed |
|---|---|---|---|
| Focusrite Scarlett 2i2 | WASAPI shared and exclusive, ASIO (cpal and native) | [2026-10-22](2026-10-22-ryzen-5900x-scarlett-2i2.md) | |
| TASCAM Model 12 | WASAPI shared and exclusive, ASIO (native) | [2026-10-22](2026-10-22-ryzen-5900x-tascam-model12.md) | |
| | | | |
| | | | |

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
