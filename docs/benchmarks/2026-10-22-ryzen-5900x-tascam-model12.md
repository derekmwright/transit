# 2026-10-22, Ryzen 9 5900X, TASCAM Model 12 (WASAPI shared and exclusive, native ASIO)

Windows 11 (build 10.0.26200.9711), AMD Ryzen 9 5900X, TASCAM Model 12 over USB with TASCAM's Model Mixer
driver, 44.1 kHz, `--out-ch 2 --in-ch 1`, 8 s runs. The unit's loaded SD song pins it to 44.1 kHz: it
shows "USB Fs MISMATCH" and mutes USB audio when a host stream runs at another rate. Cabling: MAIN OUT L
(XLR) into channel 1's line input (channel 1 in LIVE mode, assigned to no bus), channel 2 in PC mode
carrying the computer's return; the bench's output channel 2 goes to its input channel 1. The Windows
engine period for this driver is fixed at 10 ms (441 frames at 44.1 kHz); `--period` requests of 132 and
220 came back as 441.

Caveat: both Windows endpoints were set to 24-bit 44100 Hz, but the input endpoint's mix format still
reported 48000 Hz to the engine during every run, so every capture stream ran at 48 kHz against the
unit's 44.1 kHz. The render stream opened at 44100 Hz. A re-measure with the input endpoint at 44100 Hz is
still to do. A figure the run did not record is shown as "-".

## Shared mode

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| cpal | WASAPI | Speakers (Model 12) | default | 441 fr / 10.000 ms (blocks 441 to 970) | 44100 | 0.231 / 1.027 / 1.552 | 0/0 | 0.00 | 1.5 / 3.2 | 99.358 / 100.329 / 100.901 (20 of 20) | 4382 / 4425 / 4450 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| wasapi | WASAPI | Speakers (Model 12) | default | 441 fr (blocks 441 to 970) | 44100 | - | 0/0 | - | - | 59.395 / 60.374 / 61.402 (16 of 16) | - | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | shared; render buffer 970, capture 1058 at 48000 Hz; a second start phase gave 71.815 / 72.826 / 82.650 |
| wasapi | WASAPI | Speakers (Model 12) | default | 441 fr (blocks 441 to 970) | 44100 | 0.355 / 0.983 / 1.440 | 0/27 | 0.00 | 3.2 / 5.9 | 70.791 / 80.359 / 81.760 (20 of 20) | 3122 / 3544 / 3606 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | shared; the earlier run with 27 input xruns, which did not recur in five later runs |

The shared round trip sits on two values, 60 to 62 ms and 71 to 73 ms, which is one 10 ms period of
start phase. Five later native shared runs had 0/0 xruns; the 27 input xruns of the earlier run did not
recur.

## Exclusive mode

Native backend, `--mode exclusive`, plus the bench's `--in-mode` option, which opens the capture stream in
a different mode than the render stream so that a delay can be assigned to one direction.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| wasapi | WASAPI | Speakers (Model 12) | default | 441 fr / 10.000 ms | 44100 | 0.998 / 1.024 / 9.966 | 0/0 | 0.00 | 3.7 / 4.4 | 263.716 / 276.373 / 276.885 (22 of 22) | 11630 / 12188 / 12211 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive, i24, 441 fr (earlier run, raw file) |
| wasapi | WASAPI | Speakers (Model 12) | default | 441 fr / 10.0 ms | 44100 | - | 0/0 | - | - | 264.271 / 276.441 / 276.818 (17 of 18) | - | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive both ways; render i24 44100, capture opened i24 48000 Hz, buffer 480; three more runs gave 264 to 277 |
| wasapi | WASAPI | Speakers (Model 12) | 256 | 256 fr / 5.8 ms | 44100 | - | 0/0 | - | - | 254.836 / 271.293 / 272.315 (17 of 18) | - | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive both ways; capture buffer 256 at 48000 Hz; a second run gave 254.681 / 271.546 / 272.631 |
| wasapi | WASAPI | Speakers (Model 12) | 128 | refused | - | - | - | - | - | - | - | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | below the device minimum of 132 frames; PeriodRefused |
| wasapi | WASAPI | Speakers (Model 12) | default | 441 fr / 10.0 ms | 44100 | - | 0/0 | - | - | 286.716 / 301.632 / 302.182 (16 of 17) | - | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive render, shared capture (`--in-mode shared`); a second run gave 285.812 / 301.174 / 301.711 |
| wasapi | WASAPI | Speakers (Model 12) | default | 970 fr render (shared) | 44100 | - | 0/0 | - | - | 49.283 / 53.983 / 58.976 (17 of 17) | - | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | shared render, exclusive capture (`--in-mode exclusive`); an identical second run heard no click, input peaks ch1 -81.2 and ch2 -92.2 dBFS |

Exclusive buffer equals one period and `GetStreamLatency` reports 10 ms in both directions (5.8 ms and
5.3 ms at 256 frames), with 0 xruns and render service p95 of 16 to 64 us. So the roughly 220 ms extra
sits inside the driver's exclusive playback path and is invisible to the API. A `--rate 48000` request did
not reach the device: the render stream fell back to 44100 Hz, the only rate its output endpoint offers, and
the round trips were unchanged.

## ASIO

No row yet. "Model Mixer ASIO" loads and initializes (12 inputs, 10 outputs, ASIOSTInt24LSB), but cpal's
ASIO host has no packed 24-bit sample format and skips the driver, so the cpal ASIO backend cannot open it.
`audio-bench --list-devices --system ASIO` now prints a line for a driver that fails to load, with its
error. The native ASIO backend carries Int24 directly; see Native ASIO below.

## Native ASIO

The native ASIO backend (`transit-audio-asio`, ASIO slice b of the draft ADR 0016 amendment) opens the
driver that cpal skips, at its fixed 256-frame buffer with packed 24-bit samples both ways, all 12
inputs and 10 outputs: `audio-bench --backend asio --device "Model Mixer ASIO Driver" --rate 44100
--loopback --out-ch 2 --in-ch 1 --seconds 10` (built with `--features native-asio,native-wasapi`), same
cabling as above, transit closed. The rate stayed at 44.1 kHz (the backend sets a rate only when the
driver runs at another one).

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| asio | ASIO | Model Mixer ASIO Driver | default | 256 fr / 5.805 ms | 44100 | 0.196 / 0.807 / 1.042 | 0/0 | 0.00 | 12.8 / 22.8 | 27.892 / 28.085 / 28.834 (19 of 19) | 1230 / 1239 / 1272 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |

Backend lines:

- "render backend: wakes 1675, late p95 256 µs, max 528 µs; service p95 64 µs, max 180 µs; skipped 0, timeouts 0; min padding 0; overloads 0, position jumps 0, resets 0; buffer 256 fr; latency 15.8 ms; fmt i24 44100 Hz"
- "capture backend: wakes 1675, late p95 256 µs, max 528 µs; service p95 64 µs, max 180 µs; skipped 0, timeouts 0; max packets 0; overloads 0, position jumps 0, resets 0; buffer 256 fr; latency 6.8 ms; fmt i24 44100 Hz"
- "calibration: round trip 245 frames (5.556 ms) +-1 frames (0.023 ms), 19 of 19 clicks paired"; "input peak on ch1: -12.8 dBFS"

Reading: 28.1 ms round trip (1239 frames), every click paired, 0/0 xruns, no overload, position jump
or reset, against 60 to 73 ms for WASAPI shared (native) and 100 ms (cpal), and 271 to 277 ms for
WASAPI exclusive: ASIO is the tracking path on this interface. The driver reports 300 input plus 695
output frames (995 frames, 22.6 ms); the loopback measures 244 frames (5.5 ms) more, almost exactly one
256-frame buffer, and the stamp-time calibration agrees (245 frames, +-1). The callback time (12.8 us
median, 22.8 us p95) is the conversion of 22 channels of packed 24-bit samples.

## Reading

Shared mode is the usable path on this device: cpal measures 100.3 ms and the native backend 60 to 62 ms
or 71 to 73 ms depending on start phase, every click paired, and no xruns in five runs. Exclusive mode
is worse: 276 to 277 ms at the 441-frame period and 271 ms at 256, so the period barely matters, and a
128-frame request is refused because it is below the device minimum of 132 frames. Mixing modes localizes
the delay: exclusive render with shared capture is 301 ms, shared render with exclusive capture is 54 ms,
so the extra 220 ms or so follows the render stream in exclusive mode and an exclusive capture stream
costs nothing. In one of two identical shared-render, exclusive-capture runs no click was heard while the
input sat at the noise floor; the numbers cannot say whether the click never arrived or the unit muted its
return (known issue K27).

Against the Scarlett 2i2 on the same machine (shared 248 ms, exclusive 223 ms, ASIO 6.2 ms at 64
frames): the Scarlett's shared path is the slow one and exclusive helps only a little, while the Model 12's
shared path is fast and exclusive makes it much slower. Exclusive-mode latency is a property of the
driver, not of WASAPI. The Model 12's ASIO latency is not measured yet.
