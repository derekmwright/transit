# 2026-10-24, Ryzen 9 5900X, Behringer Model D over the TASCAM Model 12 and the Focusrite Scarlett 2i2 (ASIO): MIDI-to-audio onset latency

The first measurement of MIDI-to-audio onset latency with `--midi-roundtrip` (issue #71): a note goes out
over USB MIDI, a hardware synth plays it, and the bench times when the audio arrives at an ASIO input.
Four runs over the TASCAM Model 12 (43 trials), then three kept runs and one discarded run over the
Focusrite Scarlett 2i2. The method is in the README under "MIDI-to-audio onset latency".

## Setup (TASCAM Model 12)

- **Machine.** Windows 11, AMD Ryzen 9 5900X.
- **Chain.** The bench sends MIDI out over the Model D's own USB MIDI port ("MODEL D"), channel 1, to the
  Behringer Model D (fastest attack, low sustain patch). The synth's audio goes to input channel 3 of the
  TASCAM Model 12, read over its native ASIO driver ("Model Mixer ASIO Driver") at 44.1 kHz, buffer 256.
- **Buffer.** Run 3 asked for buffer 64. The driver snaps 64 to 256, so all four runs ran at 256 frames.
- **Bench.** Branch `feat/bench-midi-roundtrip`, commit `bdc7e05`. Every run used note 60 velocity 100
  except run 4, which used note 48. The command, from the output of run 2:

```
cargo run -p audio-bench --features native-asio --release -- --midi-roundtrip --midi-out "MODEL D" --midi-channel 1 --in 3 --backend asio --device "Model Mixer ASIO Driver" --rate 44100 --buffer 256 --trials 20 --gap-ms 1500 --json run2.json
```

| Run | Differs from the command above |
|---|---|
| 1 | `--trials 3 --gap-ms 750` |
| 2 | none |
| 3 | `--buffer 64 --trials 10` |
| 4 | `--note 48 --trials 10` |

## Results

Onset latency L per trial, the effective compensation offset for this chain.

| Run | Trials | Mean (samples / ms) | Median (samples / ms) | Min (samples / ms) | Max (samples / ms) | Std dev (samples / ms) | Outliers (beyond 2 std dev) | Max detector uncertainty | Send-position uncertainty | Warnings |
|---|---:|---|---|---|---|---|---|---|---|---|
| 1 (pre-change, gap 750 ms) | 3 | 1062.7 / 24.10 | 1059.0 / 24.01 | 1052 / 23.85 | 1077 / 24.42 | 12.9 / 0.29 | none | 20 samples (0.45 ms) | 496.7 us (21.9 samples) | none |
| 2 | 20 | 1071.0 / 24.29 | 1067.5 / 24.21 | 1043 / 23.65 | 1132 / 25.67 | 27.1 / 0.61 | trials 4, 20 | 72 samples (1.63 ms) | 930.0 us (41.0 samples) | slow attack: trial 4 (1.63 ms), trial 18 (1.25 ms) |
| 3 (buffer 64 requested, ran at 256) | 10 | 1090.7 / 24.73 | 1082.0 / 24.54 | 1046 / 23.72 | 1154 / 26.17 | 41.9 / 0.95 | none | 92 samples (2.09 ms) | 536.2 us (23.6 samples) | slow attack: trial 9 (2.02 ms), trial 10 (2.09 ms) |
| 4 (note 48) | 10 | 1047.0 / 23.74 | 1045.0 / 23.70 | 1022 / 23.17 | 1092 / 24.76 | 19.7 / 0.45 | trial 1 | 19 samples (0.43 ms) | 885.3 us (39.0 samples) | none |

The send-position uncertainty is the largest clock-fit residual at a send (the bench skips trials above
1.00 ms). Run 1's sample figure is converted from the printed 496.7 us at 44.1 kHz.

### Derived

The driver reports an input latency of 300 samples (6.80 ms). That figure is unverified (known issue K31).
Subtracting it from L leaves an estimate of MIDI delivery plus synth response:

| Run | Mean L minus 300 samples (samples) | ms |
|---|---:|---:|
| 1 | 762.7 | 17.29 |
| 2 | 771.0 | 17.48 |
| 3 | 790.7 | 17.93 |
| 4 | 747.0 | 16.94 |

That is about 17 ms for MIDI delivery plus the Model D's response.

## Reading (Model 12 runs)

- **What the number is.** L is the end-to-end onset latency: from the instant transit schedules the MIDI
  send to the onset of the synth's audio at the interface input, counted in the driver's sample positions.
  It is the effective compensation offset for this chain, about 1070 samples (24.3 ms) at 44.1 kHz and
  buffer 256. The median of each run sits between 1045 and 1082 samples.
- **What it is not.** It is not a measure of MIDI transport accuracy. The instant the USB stack transmits
  the bytes is unobservable here, so delivery, the synth's response and the audio input cannot be
  separated. The 17 ms figure is an estimate that leans on the driver's own input latency.
- **Where the spread comes from.**
  - *Model D attack variation.* Four trials were flagged as slow attacks (runs 2 and 3, 1.25 to 2.09 ms
    between the threshold crossing and the onset). The oscillator's attack is not identical from note
    to note, and those trials land on the high side. The largest values (1132 and 1154 samples) belong to
    this kind of trial or sit next to one.
  - *Model 12 position-clock residual.* The fit of host time to the driver's sample position leaves up
    to 0.93 ms of residual at a send (run 2). That is the uncertainty of where the send falls on the
    sample axis, and it moves L by up to about 41 samples either way.
  - *Detector late-side bias.* The onset is detected after the floor-threshold crossing, so the detector
    can only err late. The bench includes the attack's rise to 10% of its peak plus up to 10 samples of
    envelope lag, and subtracts nothing.
- **Run 1 is the pre-change run.** It had 3 trials with a 750 ms gap, before the bench fit the clock
  residual as a send-position uncertainty. The gap was short for the Model D's release: trials 2 and 3
  show a pre-send tail at -67.3 and -68.0 dBFS against a floor of -86.2 dBFS. Later runs used a 1500 ms
  gap and held the pre-send window near -82 dBFS. Its 3 trials agree with the others, but it is too
  small to weigh much.
- **Note 48 (run 4).** The median is 1045 samples, 22 below run 2. With 10 trials and a 20-sample
  spread this is not a clear difference between notes.

## Scarlett 2i2

Same bench commit (`bdc7e05`) and the same Model D USB MIDI port ("MODEL D"), channel 1. The Model D's audio
goes into Focusrite Scarlett 2i2 input 1 (Air off, gain lowered after the first, clipped run) over the
"Focusrite USB ASIO" driver at 48 kHz. All runs: `--max-fit-residual-ms 2` (except the discarded run, which
used the 1.0 ms default), `--gap-ms 1500`, note 60.

```
cargo run -p audio-bench --features native-asio --release -- --midi-roundtrip --midi-out "MODEL D" --midi-channel 1 --in 1 --backend asio --device "Focusrite USB ASIO" --rate 48000 --buffer 64 --note 60 --trials 12 --gap-ms 1500 --max-fit-residual-ms 2 --json scarlett-s1b.json
```

| Run | Differs from the command above |
|---|---|
| S1 (discarded) | `--trials 10`, default fit residual limit (1.0 ms) |
| S1b | none |
| S2 | `--buffer 128` |
| S4 | `--note 48`, `--trials 12`, buffer 64 |
| S3 | `--buffer 256`: refused, see "Bench limitations found" |

### Results

Onset latency L per trial. Driver input latency is the driver's reported figure.

| Run | Buffer | Trials | Mean (samples / ms) | Median (samples / ms) | Min (samples / ms) | Max (samples / ms) | Std dev (samples / ms) | Outliers (beyond 2 std dev) | Max detector uncertainty | Send-position uncertainty | Warnings |
|---|---:|---:|---|---|---|---|---|---|---|---|---|
| S1b | 64 | 12 | 949.8 / 19.79 | 955.0 / 19.90 | 924 / 19.25 | 981 / 20.44 | 20.0 / 0.42 | none | 65 samples (1.35 ms) | 973.0 us (46.7 samples) | slow attack: trial 5 (1.35 ms) |
| S2 | 128 | 12 | 1072.1 / 22.34 | 1068.5 / 22.26 | 1041 / 21.69 | 1121 / 23.35 | 24.3 / 0.51 | trial 10 | 67 samples (1.40 ms) | 1703.2 us (81.8 samples) | slow attack: trial 8 (1.40 ms) |
| S4 (note 48) | 64 | 12 | 931.0 / 19.40 | 923.5 / 19.24 | 903 / 18.81 | 958 / 19.96 | 18.3 / 0.38 | none | 25 samples (0.52 ms) | 1063.0 us (51.0 samples) | none |
| S1 (discarded) | 64 | 10 | 1231.1 / 25.65 | 990.5 / 20.64 | 955 / 19.90 | 3458 / 72.04 | 782.7 / 16.31 | trial 1 | 30 samples (0.62 ms) | 979.6 us (47.0 samples) | none |

Peaks of the kept runs were -13.5 to -14.1 dBFS (S1b) and -13.5 to -13.9 dBFS (S2), with pre-send windows
near -87 dBFS. S4's attack peaks alternated between -12.6 and -20.6 dBFS, a level behavior of the synth also seen in the
Model 12 note-48 run. S1b, S2 and S4 are the numbers to use. Note 48 against note 60 at buffer 64 differs by
about 0.4 ms (931.0 against 949.8 samples), within one standard deviation.

**Discarded run (S1).** The input was clipped: every attack peaked at -0.0 dBFS, and the pre-send window sat
near -70 dBFS. Trial 1 came out at 3458 samples (72.04 ms), a first-trial outlier, which pulls the mean to
1231 samples. The other nine trials span 955 to 1007 samples (mean 983.7), a few samples above the unclipped
S1b. The run is kept here only as a record. The gain was lowered and the run repeated as S1b.

### Derived

| Run | Driver input latency (samples / ms) | Mean L minus driver (samples) | ms |
|---|---|---:|---:|
| S1b (64) | 188 / 3.92 | 761.8 | 15.87 |
| S2 (128) | 300 / 6.25 | 772.1 | 16.09 |
| S4 (64, note 48) | 188 / 3.92 | 743.0 | 15.48 |

### Cross-check against the Model 12

MIDI delivery plus Model D response comes out at about 15.9 ms (buffer 64) and 16.1 ms (buffer 128) on the
Scarlett, against about 17 ms on the Model 12, from the same synth, port and notes. The two chains agree
within about 1 ms, which is the size of the scatter in a single run. Between the two Scarlett buffers the
mean L moves by 122 samples, and the driver's reported input latency steps by 112 samples (188 to 300). The
remainder, 10 samples (0.2 ms), is inside the run-to-run noise. So the part of L that depends on the audio
side moves with the driver's figure, and the remaining part (MIDI plus synth) stays put.

## Bench limitations found

These came out of the Scarlett runs. They are for the next bench slice.

- **The floor is taken from a tail.** The detector's noise floor is read from the pre-send window of the
  first trial. In S2, trial 1's window still held the tail of the previous note, so the run's floor became
  -56.3 dBFS and the threshold -44.2 dBFS instead of about -87 and -60 dBFS. It did not change the onsets
  materially: attacks rise quickly to about -14 dBFS, and trial 1 detected its onset 4 samples after the
  crossing. It does make the later threshold figure wrong for that run, and a quieter or slower attack could
  be shifted by it.
- **The clock fit did not converge at 256.** With buffer 256 on the Scarlett (S3), the bench gave up: "no clock
  map and input after 3000 ms", with 541 rejected fits. No note was sent. The Model 12 ran at 256 without
  trouble. This is a bench defect, not a driver verdict.
- **The fit residual is large on this driver.** The Scarlett's residual at a send reached 0.98 ms (S1b) and
  1.70 ms (S2), well above the Model 12's 0.5 to 0.93 ms, which is why the runs needed the 2 ms limit.
  The send-position uncertainty in the table is that residual.
- **First-note effect on one run.** The one 72 ms trial of S1 was trial 1. It was the only discarded-run
  trial that far out, and it may be a start-of-run effect rather than clipping. It is not yet separated from
  the clipping, so a warm-up trial may be needed. S4's first trial (943 samples) showed no such effect.

## Implication for #71 (Model 12)

A stored External Instrument offset near 1070 samples places this synth within about a millisecond of
the grid (the 1 ms is the order of the position-clock residual, and the run medians span about 1.7 ms).
The synth's own attack variation is the limit, not the offset. The offset is specific to this chain,
sample rate and buffer: change any of them and it should be measured again.

## Next: placement with the measured offsets

The bench limitations above are fixed in the next bench slice (issue #71): a warm-up note before
trial 1, the floor taken as the lowest window of the run, and a clock fit over 2 s of blocks that
converges with the Scarlett's buffer-256 arrivals. `--placement` then checks the offsets measured
here: each note of a pattern is sent the compensation early, and its onset's error against its
intended position is reported, uncompensated and compensated side by side (see the README, "Placement
against the grid"). The pattern spacing matches the round trip's 1500 ms gap, which the Model D's
release needs.

TASCAM Model 12, input 3, 44.1 kHz, buffer 256, compensation 1070 samples:

```
cargo run -p audio-bench --features native-asio --release -- --placement --compensation 1070 --pattern-spacing-ms 1500 --midi-out "MODEL D" --midi-channel 1 --in 3 --backend asio --device "Model Mixer ASIO Driver" --rate 44100 --buffer 256 --json model-d-model12-placement.json
```

Scarlett 2i2, input 1, 48 kHz, buffer 64, compensation 955 samples:

```
cargo run -p audio-bench --features native-asio --release -- --placement --compensation 955 --pattern-spacing-ms 1500 --midi-out "MODEL D" --midi-channel 1 --in 1 --backend asio --device "Focusrite USB ASIO" --rate 48000 --buffer 64 --json model-d-scarlett-placement.json
```

The Scarlett run that was refused at buffer 256 (S3), to check the clock fit fix:

```
cargo run -p audio-bench --features native-asio --release -- --midi-roundtrip --midi-out "MODEL D" --midi-channel 1 --in 1 --backend asio --device "Focusrite USB ASIO" --rate 48000 --buffer 256 --note 60 --trials 12 --gap-ms 1500 --json scarlett-s3b.json
```

A compensated run within about 1 ms of zero confirms the offset for that chain; the uncompensated
run should repeat the round trip's median within its spread.
