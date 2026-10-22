# 2026-10-22, Ryzen 9 5900X, Focusrite Scarlett 2i2 (WASAPI shared)

Windows 11, AMD Ryzen 9 5900X, Focusrite Scarlett 2i2 over USB, WASAPI shared mode, 48 kHz, 480-frame
(10 ms) period. Line output 2 cabled to input 2 (balanced TRS cable); the input is on Line/Inst with
the gain at mid. The first row is the native WASAPI backend, the second the same cable through cpal's
WASAPI backend.

| Backend | System | Device | Period req. (frames) | Period achieved | Rate (Hz) | Jitter p50 / p95 / max (ms) | Xruns (out/in) | Xruns/min | Callback p50 / p95 (us) | Round trip min / median / p95 (ms) | Round trip (frames) | Machine | OS | Date (UTC) | Notes |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|
| wasapi | WASAPI | Speakers (2- Focusrite USB Audio) | default | 480 fr / 10.000 ms | 48000 | 0.407 / 1.034 / 1.665 | 0/63 | 0.00 | 3.1 / 5.0 | 247.568 / 248.599 / 249.523 (37 of 40) | 11883 / 11933 / 11977 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |
| cpal | WASAPI | Speakers (2- Focusrite USB Audio) | default | 480 fr / 10.000 ms (blocks 480 to 1056) | 48000 | 0.399 / 0.995 / 1.507 | 0/25 | 0.00 | 1.2 / 2.4 | 248.448 / 248.862 / 258.440 (12 of 12) | 11926 / 11945 / 12405 | AMD Ryzen 9 5900X 12-Core Processor | Microsoft Windows [Version 10.0.26200.9711] | 2026-10-22 | - |

calibration: round trip 10525 frames (219.271 ms) +-138 frames (2.875 ms), 37 of 40 clicks paired

## Reading

The two backends agree within 1 ms on the host-time round trip (248.6 ms and 248.9 ms medians), so the
roughly 248 ms is the shared-mode path through the Focusrite driver on this machine, not the backend.
In the native stamps' own time the round trip is 10525 frames (219.3 ms) with a spread of 138 frames
(2.9 ms), 37 of 40 clicks paired. For reference, ASIO at 256 frames is what the owner tracks through.
Both runs report input xruns on an idle machine (63 in the native run, 25 in the cpal run, with 0
output xruns); the cause is not identified (known issue K24).

The loopback needs input gain: with the gain at minimum the run detects 0 of 40 clicks on both
backends. The bench prints the input peak per channel on a failed run, which shows a quiet or silent
channel.
