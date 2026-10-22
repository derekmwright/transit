# Hardware evidence and CI

## CI

`.github/workflows/ci.yml` and `release.yml` run only on release tags (and by manual dispatch);
`rt.yml` adds the realtime-crate matrix. A pull request therefore has no automatic checks: run the
gates locally (`gates.md`) and report what you ran.

The Windows release job builds twice: `windows-x86_64-asio` (`transit/asio` and
`transit/native-wasapi`, LLVM from the runner for bindgen, optional `ASIO_SDK_URL` secret for the SDK
zip) and the WASAPI-only `windows-x86_64`. Like the others it runs only on tags and manual dispatch.

## Hardware evidence

Needed when the claim is about real devices: latency, clock behavior, driver quirks, MIDI timing,
plug-in editors. A report includes:

- OS version, audio backend (WASAPI, ASIO, CoreAudio, ALSA or JACK), device and driver, sample rate,
  buffer size.
- The build identifier (version and commit) that was tested.
- The steps, the observed numbers (frames, milliseconds, xrun counts), and the app log from the
  settings folder (see `../../building.md`).
- A control: the same steps with the change reverted, when feasible.

Backend clock numbers carry unknown offset uncertainty; do not present them as certified latency
(see `../knowledge/clocks.md`). Fake-device results prove logic, not hardware.
