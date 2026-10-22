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
