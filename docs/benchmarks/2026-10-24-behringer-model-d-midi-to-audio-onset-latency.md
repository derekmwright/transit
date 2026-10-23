# 2026-10-24, Ryzen 9 5900X, Behringer Model D over the TASCAM Model 12 (ASIO): MIDI-to-audio onset latency

The first measurement of MIDI-to-audio onset latency with `--midi-roundtrip` (issue #71): a note goes out
over USB MIDI, a hardware synth plays it, and the bench times when the audio arrives at an ASIO input.
Four runs, 43 trials in all. The method is in the README under "MIDI-to-audio onset latency".

## Setup

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

## Reading

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

## Implication for #71

A stored External Instrument offset near 1070 samples places this synth within about a millisecond of
the grid (the 1 ms is the order of the position-clock residual, and the run medians span about 1.7 ms).
The synth's own attack variation is the limit, not the offset. The offset is specific to this chain,
sample rate and buffer: change any of them and it should be measured again.
