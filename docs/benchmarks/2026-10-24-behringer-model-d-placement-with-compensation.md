# 2026-10-24, Ryzen 9 5900X, Behringer Model D on the Focusrite Scarlett 2i2 (ASIO): placement with the measured compensation

The second #71 measurement. The [first](2026-10-24-behringer-model-d-midi-to-audio-onset-latency.md)
measured how late a hardware synth's audio arrives after a MIDI note is sent (the onset latency L).
This one checks the other half: if every note is sent L early, do the synth's onsets land on the grid?
Two buffer sizes (64 and 128), two compensated runs at 64 and one at 128, and one deliberately wrong
compensation at each size as a control. The method is in the README under "Placement against the grid".
The runs were made on the evening of 2026-10-23 (US Eastern); the bench stamps them 2026-10-24 (UTC),
which names this file.

## Setup

- **Machine.** Windows 11, AMD Ryzen 9 5900X.
- **Chain.** The bench sends MIDI out over the Behringer Model D's own USB MIDI port ("MODEL D"),
  channel 1, note 60, velocity 100. The synth's audio goes to input 1 of a Focusrite Scarlett 2i2, read
  over its "Focusrite USB ASIO" driver at 48 kHz.
- **Buffers.** 64 and 128 frames (1.33 ms and 2.67 ms per block). The driver reports an input latency of
  188 and 300 samples at these sizes (unverified, known issue K31).
- **Pattern.** 8 notes at 1500 ms spacing (0, 1500, ... 10500 ms; the first 250 ms after scheduling),
  each held 100 ms. The spacing is the round trip's gap, which the Model D's release needs.
- **Program state.** transit closed, no other audio program running.
- **Bench.** The `audio-bench` tool at commit `da073c6`.
- **Timing reference** (printed by every run, verbatim): `capture-block arrival (includes input buffer and
  mean driver dispatch; same as the app's ClockFitter map and the published runs)`.

## Method

1. **Measure the compensation.** `--midi-roundtrip` sends a note and times when its audio arrives. The
   median of L over the trials is the compensation, in samples. At buffer 128 a fresh 10-trial run
   gave a median of 1038.5 samples (21.64 ms, std dev 17.1 samples), and 1039 was used. At buffer 64 a
   fresh 4-trial probe gave 937.0 (19.52 ms, std dev 12.1), 18 samples below the median of 955.0 from
   the 12-trial run S1b on the [first page](2026-10-24-behringer-model-d-midi-to-audio-onset-latency.md);
   955 was used. The difference is inside the run-to-run spread.
2. **Place against a grid.** `--placement` schedules the 8 notes at intended device sample positions P.
   Each note-on is sent at P minus the compensation: the deadline is the host time of that position by the
   bench's position clock (a least-squares line of capture-block arrival against sample position over the
   last 2 s), and the wait is the app's MIDI output thread's (sleep until 300 us before the deadline, then
   yield). The return is captured by sample position and each onset O is found as in the round trip.
3. **The error.** E = O - P, in samples and ms. Each run plays the pattern twice, **uncompensated** (sent
   at P, so E is the round trip of each note) and **compensated** (sent early by the compensation, so E
   is what is left: synth and clock noise, or a wrong offset).
4. **Control.** A compensation that is 200 samples too small (755 at buffer 64, 839 at buffer 128) must
   move the compensated E by about +200 samples and leave the uncompensated E alone. If it did not, the
   compensated figures would not be measuring the compensation.

```
cargo run -p audio-bench --features native-asio --release -- --placement --compensation 955 --pattern-spacing-ms 1500 --midi-out "MODEL D" --midi-channel 1 --in 1 --backend asio --device "Focusrite USB ASIO" --rate 48000 --buffer 64 --json pl-scarlett-64-run1.json
```

| Run | Differs from the command above |
|---|---|
| 64 run 1 | none |
| 64 run 2 | none (repeat) |
| 64 wrong | `--compensation 755` |
| 128 run 1 | `--compensation 1039 --buffer 128` |
| 128 wrong | `--compensation 839 --buffer 128` |

## Results

E per run: median, standard deviation and the largest absolute error over the 8 notes of each pass.
Samples / ms at 48 kHz.

| Run | Buffer | Compensation (samples / ms) | Uncompensated E median | Uncompensated E std dev | Compensated E median | Compensated E std dev | Compensated worst abs | Drift warnings (uncomp / comp) | Slow-attack warnings (uncomp / comp) |
|---|---:|---|---|---|---|---|---|---|---|
| 64 run 1 | 64 | 955 / 19.90 | +939.0 / +19.56 | 16.7 / 0.35 | -12.0 / -0.25 | 18.9 / 0.39 | 42 / 0.88 ms | 8 / 8 | 1 / 1 |
| 64 run 2 | 64 | 955 / 19.90 | +936.5 / +19.51 | 13.5 / 0.28 | -12.0 / -0.25 | 17.9 / 0.37 | 33 / 0.69 ms | 0 / 8 | 0 / 0 |
| 128 run 1 | 128 | 1039 / 21.65 | +1033.5 / +21.53 | 13.5 / 0.28 | +2.5 / +0.05 | 20.8 / 0.43 | 43 / 0.90 ms | 8 / 8 | 0 / 1 |
| 64 wrong | 64 | 755 / 15.73 | +932.5 / +19.43 | 15.8 / 0.33 | +185.5 / +3.86 | 17.1 / 0.36 | 209 / 4.35 ms | 1 / 0 | 0 / 0 |
| 128 wrong | 128 | 839 / 17.48 | +1041.0 / +21.69 | 30.7 / 0.64 | +193.5 / +4.03 | 17.7 / 0.37 | 221 / 4.60 ms | 4 / 4 | 1 / 0 |

The mean of the compensated E is -14.4 and -9.9 samples (64 runs 1 and 2) and +3.4 samples (128 run 1).
In every run the clock fit's standard error at a send stayed at or below 28.2 us (1.35 samples) at
buffer 64 and 63.1 us (3.03 samples) at buffer 128, and the app's own clock map was valid at every send
(at most 17 samples from the bench fit at 64, 4 at 128). Sends landed on average 1.4 to 3.7 samples
after their deadlines (at most 20 samples, 0.42 ms); that is included in E.

### Before and after: a wrong compensation

The compensated E should follow the compensation and the uncompensated E should not.

| Buffer | Compensation | Uncompensated E median (samples) | Compensated E median (samples) |
|---|---|---:|---:|
| 64 | 955 (measured) | +939.0 / +936.5 (runs 1, 2) | -12.0 / -12.0 |
| 64 | 755 (200 too small) | +932.5 | +185.5 |
| 128 | 1039 (measured) | +1033.5 | +2.5 |
| 128 | 839 (200 too small) | +1041.0 | +193.5 |

Sending 200 samples later than the measured compensation shifted the compensated median by +197.5
samples at 64 and +191 at 128, close to the +200 applied. The uncompensated medians did not follow
the compensation: 932.5 against 936.5 and 939.0 at 64, and 1041.0 against 1033.5 at 128 (where the
839 run also had a larger spread, 30.7 against 13.5 samples). The placement numbers therefore measure
the compensation.

## Interpretation

- **The measured compensation places the Model D within the synth's own jitter.** The compensated
  median error is -0.25 ms at buffer 64 (both runs) and +0.05 ms at buffer 128, with a standard
  deviation of 0.37 to 0.43 ms. With the measured compensation no note landed more than 0.90 ms from
  the grid (worst absolute error 0.69 to 0.90 ms).
- **What remains is the synth.** The standard deviation of the compensated E (about 0.4 ms) is close to
  that of the uncompensated E (0.28 to 0.35 ms in the runs with the measured compensation) and of L in
  the round trip runs (0.25 to 0.36 ms). Subtracting a constant cannot remove it: the Model D's onset
  varies by that much from note to note (its attack is not identical each time, and the detector can
  only err late). The offset removed the mean and left the scatter, which is the expected result.
- **The offset is good to about a quarter of a millisecond.** The 64-buffer medians of -12 samples
  (-0.25 ms) come from the 955 taken from an earlier run; the fresh 4-trial probe gave 937, which would
  have put the median near +6. Both are inside the scatter of 8 to 12 notes.
- **For the External Instrument device (slice 3 of #71).** A stored per-chain offset, measured by the
  round trip method, is enough: it puts the synth's notes within the instrument's own jitter, with no
  per-note correction. The device needs (a) an in-app Measure button that runs the round trip and
  stores the median, keyed by interface, sample rate and buffer size, since L moves with the buffer
  (955 samples at 64, 1039 at 128 in these runs); and (b) the reconciliation of clocks that the
  placement module's docs flag: this bench schedules on the capture-block position clock, which is
  the app's ClockFitter input map, while the app's MIDI output thread schedules on the output
  presentation map. The device has to use one reference, and Measure has to be taken in it.

## Caveats

- **The fit drift warning is miscalibrated at these buffer sizes.** The warning fires when the clock
  fit's leave-last-out error at a send exceeds 0.5 ms (the same limit as the fit error limit). At buffer
  64 the warnings sat at +-0.65 to 0.68 ms, which is half a block (1.33 ms / 2): the driver delivers
  blocks in whole-block steps, so the newest block sits up to about half a block off the line. At buffer
  128 they sat at +-0.65 to 0.69 ms (and up to +-1.03 ms in the wrong-839 run), below half a block
  (1.33 ms) but above 0.5 ms. The warning therefore fired on most notes (16 of 16 in the
  128 run 1; every compensated note in the 64 runs) while the fit's standard error at the send stayed
  under 63 us and the placement results above held. The warning does not mean the dispatch delay
  changed: the app's clock map and the compensated E agreed with the bench fit throughout. A 0.5 ms
  threshold is below the arrival quantization of the driver, so for this device the warning is noise.
  It should scale with the block period (a fraction of it), so a real step in the dispatch delay still
  trips it.
- **One synth, one interface.** The numbers are for the Model D with the fastest attack on a Scarlett
  2i2 over ASIO at 48 kHz, with the gain set so attack peaks sat near -13.5 dBFS. The compensation is
  specific to the chain, rate and buffer.
- **Small samples.** 8 notes per pass, two compensated runs at 64 and one at 128. The 0.3 ms difference
  between the 64 and 128 compensated medians is inside the scatter.
- **Slow attacks.** Four notes were flagged as slow (1.33 to 2.12 ms from the threshold crossing to the
  onset); they land on the late side and are part of the scatter above. They are warnings, not exclusions.
- **Not a MIDI transport measure.** As in the first page, the instant the USB stack transmits the bytes
  is unobservable here. The figures are end to end.
