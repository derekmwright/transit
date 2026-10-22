# Hardware evidence and CI

## CI

`.github/workflows/ci.yml` is the push gate: it runs on every pull request and on every push to
`main`, and cancels a superseded run. Jobs:

- `windows`: `cargo xtask layers`, `cargo xtask assets`, then clippy (`-D warnings`) and the tests for
  the whole workspace with `--features transit/native-wasapi`.
- `windows-asio`: clippy and tests with the ASIO features (`transit/asio`, `audio-bench/asio`,
  `soundcraft-playback/asio`). It needs LLVM for bindgen (set up from the runner) and Steinberg's SDK,
  which `asio-sys` downloads or the optional `ASIO_SDK_URL` secret supplies. It is
  `continue-on-error`: a failed SDK download reports red but does not block a merge.
- `linux`: `cargo fmt --check`, clippy and the tests on default features.
- `macos`: clippy and the tests on default features (`crates/au-host` and the objc2 code compile only
  there).
- `wasm`: `cargo xtask wasm`.

`rt.yml` (the realtime-crate platform matrix) runs on pushes and pull requests that touch `crates/rt`,
`crates/mix` or the workspace manifest, and on tags. `release.yml` runs only on release tags and
manual dispatch. The gate does not call `cargo xtask ci` as a whole: `rt-targets` is covered by
`rt.yml`, and the bench step is not run. Cargo calls use `--locked`, so a stale `Cargo.lock` (which
also pins the patched cpal revision) fails the build.

Not gated: FreeBSD, `cargo xtask rt-targets` and the device-free `audio-bench` run, dependency
advisories (no `deny.toml`), and every hardware test. Hardware tests are `#[ignore]`d and never CI
gates: they need a real device and a person (see below).

The Windows release job builds twice: `windows-x86_64-asio` (`transit/asio` and
`transit/native-wasapi`, LLVM from the runner for bindgen, optional `ASIO_SDK_URL` secret for the SDK
zip) and the WASAPI-only `windows-x86_64`.

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
