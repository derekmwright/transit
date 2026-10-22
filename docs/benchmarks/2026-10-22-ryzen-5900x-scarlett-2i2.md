# 2026-10-22, Ryzen 9 5900X, Focusrite Scarlett 2i2 (WASAPI shared)

Windows 11, AMD Ryzen 9 5900X, Focusrite Scarlett 2i2 over USB, WASAPI shared mode, 48 kHz, 480-frame
(10 ms) period. Line output 2 cabled to input 2 (balanced TRS cable); the input is on Line/Inst with
the gain at mid. The first row is the native WASAPI backend, the second the same cable through cpal's
WASAPI backend.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| wasapi | WASAPI | Speakers (2- Focusrite USB Audio) | default | 480 fr / 10.000 ms | 48000 | 0.407 / 1.034 / 1.665 | 0/63 | 0.00 | 3.1 / 5.0 | 247.568 / 248.599 / 249.523 (37 of 40) | 11883 / 11933 / 11977 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| cpal | WASAPI | Speakers (2- Focusrite USB Audio) | default | 480 fr / 10.000 ms (blocks 480 to 1056) | 48000 | 0.399 / 0.995 / 1.507 | 0/25 | 0.00 | 1.2 / 2.4 | 248.448 / 248.862 / 258.440 (12 of 12) | 11926 / 11945 / 12405 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| cpal | ASIO | Focusrite USB ASIO | default | 256 fr / 5.333 ms | 48000 | 0.666 / 5.318 / 5.321 | 0/0 | 0.00 | 1.0 / 1.4 | 22.576 / 23.251 / 29.238 (20 of 20) | 1084 / 1116 / 1403 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | same cable, the driver's own 256-frame buffer |

calibration: round trip 10525 frames (219.271 ms) +-138 frames (2.875 ms), 37 of 40 clicks paired

## Reading

The two backends agree within 1 ms on the host-time round trip (248.6 ms and 248.9 ms medians), so the
roughly 248 ms is the shared-mode path through the Focusrite driver on this machine, not the backend.
In the native stamps' own time the round trip is 10525 frames (219.3 ms) with a spread of 138 frames
(2.9 ms), 37 of 40 clicks paired. The third row is the same cable through the interface's ASIO driver at
its own 256-frame buffer: 23.3 ms median, 20 of 20 clicks, no xruns. So the interface and its driver reach
tracking latency; it is the Windows shared-mode path (the audio engine plus the driver's WDM buffering)
that costs about 225 ms on this device, and exclusive mode (ADR 0016 slice e) is the way around it.
Both runs report input xruns on an idle machine (63 in the native run, 25 in the cpal run, with 0
output xruns); the cause is not identified (known issue K24).

The loopback needs input gain: with the gain at minimum the run detects 0 of 40 clicks on both
backends. The bench prints the input peak per channel on a failed run, which shows a quiet or silent
channel.

## Exclusive mode

The same cable at 48 kHz through the native backend in WASAPI exclusive mode (`--mode exclusive`),
at `--period` 480, 256 and 144 frames (144 is the 3 ms minimum the driver reports); the driver granted
packed 24-bit stereo each time.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| wasapi | WASAPI | Speakers (2- Focusrite USB Audio) | 480 | 480 fr / 10.000 ms | 48000 | 2.009 / 4.016 / 4.093 | 0/0 | 0.00 | 3.6 / 4.6 | 217.021 / 223.067 / 229.095 (12 of 12) | 10417 / 10707 / 10997 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive, i24, 480 fr |
| wasapi | WASAPI | Speakers (2- Focusrite USB Audio) | 256 | 256 fr / 5.333 ms | 48000 | 0.673 / 6.680 / 6.942 | 213/2 | 2129.82 | 2.1 / 2.5 | 119.102 / 146.403 / 289.768 (8 of 8) | 5717 / 7027 / 13909 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive, i24, 256 fr; invalid stamps 134 |
| wasapi | WASAPI | Speakers (2- Focusrite USB Audio) | 144 | 144 fr / 3.000 ms | 48000 | 3.000 / 3.024 / 3.087 | 992/91 | 9929.22 | 1.5 / 1.6 | 168.966 / 258.008 / 361.955 (4 of 6) | 8110 / 12384 / 17374 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | exclusive, i24, 144 fr; invalid stamps 950 |

Calibrations, in the same order:

- calibration: round trip 9747 frames (203.062 ms) +-581 frames (12.104 ms), 12 of 12 clicks paired
- calibration: round trip 6516 frames (135.750 ms) +-8194 frames (170.708 ms), 8 of 8 clicks paired
- calibration: round trip 12096 frames (252.000 ms) +-9265 frames (193.021 ms), 4 of 6 clicks paired

Exclusive mode at the 10 ms default period is clean (no xruns, 12 of 12 clicks) but its round trip is still 223 ms, against 248 ms shared and 23 ms through ASIO, so the latency sits inside the Focusrite WDM driver, not the Windows audio engine. Below the default the driver signals only every other period (6 ms intervals at a 3 ms period), which shows as hundreds of xruns and invalid stamps; see known issue K25.

## ASIO buffer sweep (Safe Mode off)

The same cable (output 2 to input 2) through cpal's ASIO host with Focusrite Control's Safe Mode turned off, at the driver's 256-frame default and with `--period 128`, `64` and `32` requested (the driver honored each).

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| cpal | ASIO | Focusrite USB ASIO | default | 256 fr / 5.333 ms | 48000 | 0.667 / 5.318 / 5.321 | 0/0 | 0.00 | 1.0 / 1.1 | 21.433 / 22.133 / 28.121 (20 of 20) | 1029 / 1062 / 1350 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | Safe Mode off |
| cpal | ASIO | Focusrite USB ASIO | 128 | 128 fr / 2.667 ms | 48000 | 0.333 / 2.658 / 2.660 | 0/0 | 0.00 | 0.6 / 0.8 | 12.237 / 12.253 / 15.242 (20 of 20) | 587 / 588 / 732 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | Safe Mode off |
| cpal | ASIO | Focusrite USB ASIO | 64 | 64 fr / 1.333 ms | 48000 | 0.669 / 1.329 / 1.329 | 1666/1617 | 10008.33 | 0.4 / 0.6 | 6.101 / 6.122 / 6.136 (20 of 20) | 293 / 294 / 295 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | Safe Mode off; gap counts are burst delivery, see Reading |
| cpal | ASIO | Focusrite USB ASIO | 32 | 32 fr / 0.667 ms | 48000 | 0.336 / 0.663 / 0.664 | 3784/3653 | 22703.81 | 0.2 / 0.5 | 4.361 / 4.458 / 4.467 (20 of 20) | 209 / 214 / 214 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | Safe Mode off; gap counts are burst delivery, see Reading |

Reading: Safe Mode off saved about 1 ms at 256 frames (22.1 ms against 23.3 ms earlier that day). The round trip scales with the buffer: about 12.3 ms at 128, 6.1 ms at 64 and 4.5 ms at 32, so the driver's fixed overhead is roughly 3 ms plus two buffers. The xrun column on the cpal path is not a driver report: `audio-bench` counted every gap between callbacks longer than 1.5 periods. At 64 and 32 frames the Focusrite ASIO driver delivers callbacks in bursts (jitter p95 equals exactly one period), so about a fifth of the callbacks follow a two-period gap and are counted, while the round trip stays within 0.03 ms and every click pairs; real dropouts on that scale would lose clicks and spread the round trip. Treat those counts as delivery cadence, not glitches, and note that the shared-mode input xruns of known issue K24 come from the same heuristic and may have the same explanation. The counts in this table predate the drift-based detector (known issue K26), which counts a burst as nothing and would report these runs differently.

## Re-measured with the drift detector

The same cable (output 2 to input 2) at 48 kHz with Focusrite Safe Mode off, measured again after the bench's xrun column became a drift-based detector (known issue K26). The two ASIO rows are cpal's ASIO host at `--period` 64 and 32; the last row is the native WASAPI backend in shared mode.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| cpal | ASIO | Focusrite USB ASIO | 64 | 64 fr / 1.333 ms | 48000 | 0.669 / 1.329 / 1.728 | 0/0 | 0.00 | 0.4 / 0.6 | 6.231 / 6.244 / 6.255 (20 of 20) | 299 / 300 / 300 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | Safe Mode off; drift detector |
| cpal | ASIO | Focusrite USB ASIO | 32 | 32 fr / 0.667 ms | 48000 | 0.335 / 0.663 / 0.664 | 0/0 | 0.00 | 0.2 / 0.5 | 4.450 / 4.456 / 4.469 (20 of 20) | 214 / 214 / 215 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | Safe Mode off; drift detector |
| wasapi | WASAPI | Speakers (2- Focusrite USB Audio) | default | 480 fr / 10.000 ms (blocks 480 to 1056) | 48000 | 0.386 / 1.153 / 10.488 | 3/0 | 18.02 | 3.2 / 5.9 | 235.746 / 236.477 / 265.622 (19 of 20) | 11316 / 11351 / 12750 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | drift detector; a second interface's driver was installing during the run |

Backend lines of the shared-mode run: "render backend: wakes 999, late p95 1024 µs, max 11146 µs; service p95 32 µs, max 32 µs; skipped 0, timeouts 0; min padding 0" and "capture backend: wakes 999, late p95 8192 µs, max 12144 µs; service p95 54 µs, max 54 µs; skipped 0, timeouts 0; max packets 2".

calibration: round trip 10057 frames (209.521 ms) +-1517 frames (31.604 ms), 19 of 20 clicks paired

Reading: the ASIO rows at 64 and 32 frames now report 0/0 where the gap heuristic reported 1666/1617 and 3784/3653, with the round trips unchanged within 0.15 ms. That confirms the old counts were burst delivery, not lost frames. The shared-mode run reports 0 input xruns where the heuristic reported 63 and 25, so the unexplained input counts of known issue K24 were the heuristic. Its 3 render xruns are real: one wake came 11.1 ms late with the padding at zero, and the engine then handed a 1056-frame block. The run was measured while another interface's driver was being installed, so that is a machine load event, not a steady fault. The shared round trip measured 236 ms in this run against 248 ms in the morning runs; the difference is not explained (the Windows audio engine had restarted for the new device).

## Native ASIO

The same cable (output 2 to input 2) at 48 kHz with Focusrite Safe Mode off, through the native ASIO
backend (`transit-audio-asio`, ASIO slice b of the draft ADR 0016 amendment), with the drift detector:
`audio-bench --backend asio --device "Focusrite USB ASIO" --loopback --out-ch 2 --in-ch 2 --seconds 10
--period N` (built with `--features native-asio,native-wasapi`; no ASIO SDK, no LLVM), run one after
the other with transit closed.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| asio | ASIO | Focusrite USB ASIO | 256 | 256 fr / 5.333 ms | 48000 | 0.666 / 5.316 / 5.320 | 0/0 | 0.00 | 2.1 / 3.7 | 21.344 / 22.139 / 28.129 (20 of 20) | 1025 / 1063 / 1350 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| asio | ASIO | Focusrite USB ASIO | 128 | 128 fr / 2.667 ms | 48000 | 0.333 / 2.657 / 2.659 | 0/0 | 0.00 | 1.2 / 1.9 | 12.047 / 12.138 / 15.137 (20 of 20) | 578 / 583 / 727 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| asio | ASIO | Focusrite USB ASIO | 64 | 64 fr / 1.333 ms | 48000 | 0.669 / 1.328 / 1.328 | 0/0 | 0.00 | 0.8 / 1.1 | 6.101 / 6.125 / 6.136 (20 of 20) | 293 / 294 / 295 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| asio | ASIO | Focusrite USB ASIO | 32 | 32 fr / 0.667 ms | 48000 | 0.337 / 0.663 / 0.703 | 0/0 | 0.00 | 0.5 / 0.8 | 4.429 / 4.452 / 4.459 (20 of 20) | 213 / 214 / 214 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |

Backend lines (render, then capture, for 256, 128, 64 and 32 frames):

- "render backend: wakes 1873, late p95 992 µs, max 992 µs; service p95 32 µs, max 66 µs; skipped 0, timeouts 0; min padding 0; overloads 0, position jumps 0, resets 0; buffer 256 fr; latency 12.9 ms; fmt i32 48000 Hz"
- "capture backend: wakes 1873, late p95 992 µs, max 992 µs; service p95 32 µs, max 66 µs; skipped 0, timeouts 0; max packets 0; overloads 0, position jumps 0, resets 0; buffer 256 fr; latency 11.9 ms; fmt i32 48000 Hz"
- "render backend: wakes 3746, late p95 512 µs, max 662 µs; service p95 16 µs, max 50 µs; skipped 0, timeouts 0; min padding 0; overloads 0, position jumps 0, resets 0; buffer 128 fr; latency 7.2 ms; fmt i32 48000 Hz"
- "capture backend: wakes 3746, late p95 512 µs, max 662 µs; service p95 16 µs, max 50 µs; skipped 0, timeouts 0; max packets 0; overloads 0, position jumps 0, resets 0; buffer 128 fr; latency 6.2 ms; fmt i32 48000 Hz"
- "render backend: wakes 7492, late p95 1024 µs, max 1262 µs; service p95 16 µs, max 51 µs; skipped 0, timeouts 0; min padding 0; overloads 0, position jumps 0, resets 0; buffer 64 fr; latency 2.9 ms; fmt i32 48000 Hz"
- "capture backend: wakes 7492, late p95 1024 µs, max 1262 µs; service p95 16 µs, max 51 µs; skipped 0, timeouts 0; max packets 0; overloads 0, position jumps 0, resets 0; buffer 64 fr; latency 3.9 ms; fmt i32 48000 Hz"
- "render backend: wakes 14985, late p95 512 µs, max 702 µs; service p95 16 µs, max 64 µs; skipped 0, timeouts 0; min padding 0; overloads 0, position jumps 0, resets 0; buffer 32 fr; latency 2.2 ms; fmt i32 48000 Hz"
- "capture backend: wakes 14985, late p95 512 µs, max 702 µs; service p95 16 µs, max 64 µs; skipped 0, timeouts 0; max packets 0; overloads 0, position jumps 0, resets 0; buffer 32 fr; latency 2.2 ms; fmt i32 48000 Hz"

Every run printed "calibration: none (only 0 of 20 clicks were detected on the loopback ...)" with the
input peak on ch2 at -14.9 dBFS: the clicks were detected and paired by host time (20 of 20 above), but
the stamp-time pairing found no click whose capture frame time came after its render frame time.

Reading: the native backend matches cpal's ASIO host to within 0.2 ms at every buffer size: 22.14 ms
against 22.13 at 256, 12.14 against 12.25 at 128, 6.13 against 6.12 and 6.24 at 64, 4.45 against 4.46
at 32 (the 1 ms gate of ASIO slice b is met), with every click paired, 0/0 xruns on the drift detector,
and the driver reporting 0 overloads, position jumps and resets. Callback time is 0.5 to 2.1 us at the
median against cpal's 0.2 to 1.0 (the bench measures inside the callback, so the difference is ours:
conversion of planar Int32 plus the stamp). Lateness p95 near one buffer at 256 and 64 frames is the
driver's burst delivery (K26), not lost frames: the sample position never jumped. The stamps are the
open item: the Focusrite driver's `getLatencies` figures (input plus output: 24.8 ms at 256, 13.4 at 128,
6.8 at 64, 4.4 at 32 frames) exceed the measured round trip at 256, 128 and 64 frames, so the stamp
frame times put the captured click before the played one and no loopback calibration can be formed
(the calibration's round trip is unsigned). Either the driver counts a buffer it should not, or the
stamp model's "latency includes the buffer" reading is wrong for this driver; ASIO slice d decides.
