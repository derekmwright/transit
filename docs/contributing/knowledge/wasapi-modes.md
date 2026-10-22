# WASAPI shared and exclusive mode

Code: `crates/audio-wasapi/src/exclusive.rs` (the open logic, tested on a fake), `com.rs` (the COM
calls), `render_loop.rs` (`Fill`). Decision: [ADR 0016](../../adr/0016-native-wasapi-backend.md), section 2.

**Shared** (`Mode::Shared`) goes through the Windows audio engine: one mix format, the engine's period
(`IAudioClient3` where the driver offers a small one), APOs and rate conversion. Other apps keep playing.
On the Scarlett 2i2 its round trip is about 248 ms against 23 ms through ASIO.

**Exclusive** (`Mode::Exclusive`) bypasses the engine: the stream talks to the driver, so other apps are
silenced while it is open. What changes:

- **Format.** Any format the driver takes, not the mix format. `exclusive_candidates` lists rates, channel
  counts and F32, I32 with 24 valid bits, I32, packed I24, I16; `probe_exclusive` asks
  `IsFormatSupported(EXCLUSIVE)` for each (control thread only, never during enumeration). The open tries
  the requested format, then the candidates at its rate and channel count, and reports the one it got.
- **Period.** `Period::Frames(n)` as asked, or else `GetDevicePeriod`'s default (not its minimum: the
  Scarlett 2i2 reports 3 ms but signals only every other period below its 10 ms default, K25), with
  `hnsPeriodicity == hnsBufferDuration`. The minimum is reported in `Achieved::min_period` for the UI to
  offer. The buffer is one period and the render loop fills all of it on every event (`Fill::Whole`);
  padding is not read.
- **Alignment retry.** `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` means: read the aligned frames from
  `GetBufferSize`, drop the client, activate a new one and `Initialize` once more at that size (a failed
  `Initialize` cannot be retried on the same client). `AUDCLNT_E_BUFFER_SIZE_ERROR` names the range.
- **Errors.** `AUDCLNT_E_DEVICE_IN_USE` is `OpenError::InUse` (another app holds it);
  `AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED` is `OpenError::ExclusiveDisallowed` (the endpoint's Advanced
  settings forbid it); a gone endpoint is `DeviceNotAvailable`. No raw mode, no `AUTOCONVERTPCM`.
- **Calibration.** A loopback calibration is keyed by mode (`"shared"` or `"exclusive"`); measure each.

On the Scarlett 2i2 exclusive mode at 480 frames still measures about 223 ms round trip: that latency is
in the Focusrite WDM driver, and ASIO is the low-latency path there. The cpal adapter opens shared
streams only; it refuses `Mode::Exclusive`.
