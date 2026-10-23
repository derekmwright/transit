# 2026-10-23, Ryzen 9 5900X, Focusrite Scarlett 2i2 (ASIO): driver-reported vs measured latency

Windows 11, AMD Ryzen 9 5900X, Focusrite Scarlett 2i2 over USB, native ASIO backend, Focusrite Safe Mode
off, build 0.2.0-alpha.1 bench. Loop: output 2 cabled to input 2. Each run is 16 clicks over 8 s; 16 of 16
clicks were detected in every ASIO run, with 0 xruns. This is known issue K31: the driver's
`getLatencies` figures are compared with the click round trip the bench measures on the same cable.

"Measured" is the median click round trip from the bench, in frames (min to p95 in the raw rows). "Driver"
is the `getLatencies` figure the bench prints, out and in in ms, one decimal per direction, so each figure
is good to about ±2 frames. The driver sum is that pair converted at the stream rate.

## Measured against reported

| Rate (Hz) | Period (frames) | Measured round trip, median (frames) | Driver out + in (ms) | Driver sum (frames) | Measured minus driver (frames) |
|---|---:|---:|---:|---:|---:|
| 48000 | 32 | 220, 214 | 2.2 + 2.2 | ~211 | +9, +3 |
| 48000 | 64 | 294, 300, 300 | 2.9 + 3.9 | ~328 | -34, -28 |
| 48000 | 128 | 725, 725 (low mode 582) | 7.2 + 6.2 | ~643 | +82 (low mode -61) |
| 48000 | 256 | 1068, 1062, 1062 | 12.9 + 11.9 | ~1190 | -122, -128 |
| 48000 | 512 | 2247, 2252 | 23.2 + 22.2 | ~2179 | +68, +73 |
| 44100 | 64 | 322, 322 | 3.1 + 4.1 | ~318 | +4 |
| 44100 | 256 | 1172, 1174 | 13.4 + 12.4 | ~1138 | +34, +36 |

Each row with two values is two separate runs. The low mode is the lower of the two round trip modes
described below.

cpal's ASIO host on the same driver, 48000 Hz: 64 frames measured 294 and 300 (driver sum 328, differences
-34 and -28); 256 frames measured 1068 and 1068 (driver sum 1190, difference -122). The same driver through
both hosts gives the same round trip within a few frames.

Native WASAPI shared mode (not ASIO), default 480-frame period: the click round trip measured 2955 frames
(61.6 ms median), while the bench's own stamp calibration said 1816 frames (37.8 ms, ±106 frames). The run
had 23 input xruns, so it is not a clean reference for either number.

## Reading

- **The difference has no fixed value.** It is not a constant number of frames or of periods. Its sign
  flips with the period: the driver over-reports at 64 and 256 frames (measured less than driver) and
  under-reports at 32, 128 and 512 (measured more than driver). It also moves with the sample rate: at
  44.1 kHz it is +4 frames at 64 and +34 to +36 at 256, where 48 kHz gives -34 and -122.
- **Two modes at 128 and 512 frames.** Within a single run the loop splits between two round trip modes.
  The modes sit 143 frames apart at 128 and about 455 apart at 512, close to one period each, so the shift is
  about one buffer.
- **Repeats are stable at 64 and 256 frames.** The three 64-frame runs measured 294 to 300 frames and the
  three 256-frame runs 1062 to 1068 frames, within 6 frames of each other.
- **Stamp calibration is erratic.** Across the ASIO runs the stamp-based calibration either pairs no click,
  or pairs every click and reports a 4-frame round trip. The measured loops in the same runs were 214 to
  about 2250 frames. The native WASAPI shared run shows the same kind of gap: the stamps said 1816 frames
  and the clicks measured 2955.

## Conclusion

The driver's reported figures cannot be corrected by a fixed model. The sign and the size of the error
change with the period and with the sample rate, so no single offset or per-period table would hold. The
measured click round trip at the configured period and rate is the ground truth. Calibration over ASIO
should therefore measure the loop rather than model it, and the stamp-based path has a defect to fix or
retire (ASIO slice d; see known issue K31).
