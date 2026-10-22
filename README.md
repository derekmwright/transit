<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/transit-wordmark-white-2172.png">
    <source media="(prefers-color-scheme: light)" srcset="assets/brand/transit-wordmark-graphite-2172.png">
    <img alt="transit" src="assets/brand/transit-wordmark-graphite-2172.png" width="360">
  </picture>
</p>

A desktop DAW built on the [SoundCraft](https://github.com/storytold/soundcraft) engine, with
Ableton/Logic-style ergonomics. Pure Rust, egui UI.

![Arrangement view](docs/images/transit-arrange.png)

![Mixer](docs/images/transit-mixer.png)
![Piano roll](docs/images/transit-pianoroll.png)

## Features

### Arrangement

- Adaptive grid down to 1/128, with Ctrl+1-4 for narrower, wider, triplet and snap on/off.
- Clips snap to the grid as you drag; Ctrl-drag or Alt-drag copies; Shift bypasses snap.
- Loop strip on the ruler, in amber: drag to make a loop, drag its edges to resize it, double-click to toggle it. The loop follows tempo changes.
- Automation lanes under any track for volume, pan, mute, sends and every device parameter; Read, Touch, Latch and Write modes.
- Tempo and meter changes along the timeline, edited by dragging or typing in the transport.
- Transport metronome button: right-click for click level and count-in bars. Count-in works at bar 1, and recording starts sample-exact at the end of it.
- Transport load readout (for example "30%  128 @ 48 kHz  ASIO") with an overload warning.
- Piano roll with a velocity lane, and step entry: play notes on your keyboard to enter them at the cursor. Keys you play live are accented on the piano roll's keyboard. It shares the adaptive grid (Ctrl+1-4), with step-cursor keys and a fixed-velocity setting.
- Freeze, Unfreeze and Flatten a track (the snowflake on the track header); a frozen track plays its rendered audio, with its key sources running.

### Mixer and routing

- Per-track device chains: drag a device to reorder the chain; Alt-drag copies it.
- Buses and sends, with a warning for routes that point nowhere and a refusal for routing loops.
- Sidechain any compressor or gate from any track or bus, pre- or post-fader, latency-aligned.
- Hosted plug-ins can take a sidechain key too.
- Plug-in delay compensation keeps every path time-aligned.
- Opens every output on your interface; name your own paths and route tracks, buses and sends to any of them.
- Hardware inserts: send a track through outboard gear on your interface. Choose "Hardware Insert..." in the device chain's "+" menu, then pick a send path and a return path (a pair) on the insert. Audio Devices > I/O > Hardware inserts has a Measure button that plays a click train and records the round trip, which delay compensation then uses so the track stays aligned. Measure runs in the background with a progress count (n/8 clicks) and a Cancel button, so the UI stays responsive. Transport starts (Play, Record, Space) are refused while a measurement runs, and re-opening the audio device cancels it at once. The insert shows Ready, Unmeasured, No input, Missing send, Missing return or Disabled, with the reason. A route to a path an insert sends on is refused.

### Instruments and effects

- Hosts CLAP and VST3 plug-ins, with their own editor windows, saved state, and reported latency.
- The device card shows Loading while a heavy plug-in instantiates.
- Plug-in browser with a background scan and your own search folders.
- Built-in instruments: Subtractive Synth, Drum Synth, Sampler. All built-ins open an editor window: parameter rows, Sampler load or drop, Drum Synth by pad.
- Built-in panels with knobs, and bespoke layouts for Compressor, Gate, EQ, Synth and Plate; factory and user presets; a scope and a gain-reduction display on the dynamics panels. Moving a plug-in knob while a lane is armed (Write, Latch, Touch) records into it.
- Built-in dynamics and EQ: Compressor/Limiter, Expander/Gate, De-Esser, Maximizer, Channel Strip, EQ 7-Band, EQ 1-Band.
- Built-in effects: Room and Plate Reverb, Mod Delay, Chorus, Flanger, Phaser, Saturator, Lo-Fi, Rectifier, Pitch Shifter.
- Utilities: Gain, Trim, Invert, Time Shift, DC Offset Removal, Signal Generator, Dither.

### MIDI

- Play hosted instruments live from a USB controller; on Windows it uses the native WinMM driver.
- Input without usable timestamps is still played, with approximate timing, rather than dropped.
- MIDI recording: arm an instrument track and record, with a count-in. Overdub merges into the clip under the pass; each loop pass becomes a take; the lane shows the growing clip while you play.
- A MIDI take journal is written during the pass and kept until an autosave that holds the recording lands; after a crash, the next launch recovers it as a `<Track>_recovered` clip. Record works without a validated device clock: alignment is approximate, a take recorded that way says so, and when recording cannot start you get a plain-English reason.
- A hosted plug-in that re-sends an unchanged parameter value no longer registers an automation touch; only a real change does.

### Audio devices

- Audio Devices window to pick the driver (WASAPI, or ASIO in builds you make yourself), device and buffer size.
- Named input and output paths, saved per machine.
- Experimental native WASAPI backend (build with the `native-wasapi` feature; pick it under Audio Devices > Backend), with a cpal fallback. It plays, captures (filling any lost packets with zeros, so a take keeps its length) and runs duplex: an input is paired with its output by device, chosen with the Paired input selector. `audio-bench --backend wasapi` measures it.
- Loopback calibration: `audio-bench --backend wasapi --loopback --settings` measures the interface's round trip and saves it, and the timing label then reads "calibrated".
- Tracks, buses and sends route to any output path, with latency aligned across them.

### Sessions and rendering

- Bounce the main mix or any bus over the selection or loop range to WAV, AIFF or FLAC at 16-bit, 24-bit or 32-bit float.
- Stems by output path from the Bounce dialog.
- Bounce in Place turns a track's clips into audio.
- Full undo history; every edit is a command.
- Autosave, and sessions saved as versioned, human-readable JSON.
- A launch splash that reports the audio device opening first, then plug-in scan progress, and Help > About with version info.
- Import audio and MIDI files; export MIDI. File > Open Recent lists your last ten sessions (Ctrl+Shift+O).

### Under the hood

- An audio backend trait sits between the Player and the audio system, with cpal behind it as the first adapter.
- One renderer: playback and offline render both use the plan renderer; the legacy renderer is gone.
- Offline render and playback are checked against each other by golden tests.
- Every edit is a named command, which is what makes undo and the command catalog uniform.

## Status

Pre-alpha. The product is now called transit, and its settings folder moved from `transitDAW` to `transit` (your settings are moved once on first launch). Windows is tested daily; macOS and Linux build but are untested (their packaging ids are `dev.derekmwright.transit`). The packaging scripts under `packaging/` build the transit app on every platform ([docs/building.md](docs/building.md#packaging)). Releases are WASAPI-only;
ASIO is available when you build it yourself ([docs/building.md](docs/building.md#building-with-asio-windows)).
Expect rough edges and breaking changes to the session format. Back up anything you care about.

## Build

You need a Rust toolchain at 1.90 or newer (`rust-version` in `Cargo.toml`). On Linux, install
`libasound2-dev` and `pkg-config` first.

```sh
cargo build -p transit --release
```

The binary is `target/release/transit` (`transit.exe` on Windows).

### ASIO on Windows (optional)

ASIO is behind the `transit/asio` feature: `cargo build -p transit --release --features transit/asio`.
It needs LLVM (for bindgen; set `LIBCLANG_PATH` to LLVM's `bin`, and put LLVM before MinGW on
`PATH`), the Visual Studio 2019+ Build Tools, and the Steinberg ASIO SDK (`asio-sys` downloads it on
the first build unless `CPAL_ASIO_DIR` points at a copy). Steinberg's licensing terms apply to
anyone who redistributes ASIO-enabled binaries, so the published releases are WASAPI-only.

Full steps, the MinGW/libclang pitfall, and licensing: [docs/building.md](docs/building.md#building-with-asio-windows); scripted build: `tools/build-asio.ps1`.

## Run

```sh
cargo run -p transit --release            # empty session
cargo run -p transit --release -- --demo  # built-in demo song
transit --version
```

Settings, autosaves and logs live in a per-user folder: `%APPDATA%\transit` on Windows,
`~/Library/Application Support/transit` on macOS, `$XDG_CONFIG_HOME/transit` (or
`~/.config/transit`) elsewhere. Set `TRANSIT_CONFIG_DIR` to use another folder. An empty value
disables saving settings.

## Status and known issues

- Pre-alpha; the session format and UI change freely.
- A heavy plug-in can still block the UI for the duration of its single create call.
- Approximate record alignment on devices whose clock never validates can differ by up to one input block (K14); Freeze renders a fixed 2 second tail.
- Windows releases are unsigned: SmartScreen shows "More info", then "Run anyway".

## Based on SoundCraft

transit is based on [SoundCraft](https://github.com/storytold/soundcraft) by storytold, an
open-source Rust digital audio workstation. The audio engine, plug-in hosts and most crates come
from it; the transit UI and product work are by @derekmwright. The crates we own are named `transit-*`; crates inherited from SoundCraft keep their
`soundcraft-*` names until they are renamed. Engine fixes may be offered back upstream.

## License

MIT OR Apache-2.0, at your option: see [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)
and [NOTICE](NOTICE).

## Contributing

Issues are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Background on past decisions is in the [ADR index](docs/adr/README.md).

## For contributors and their agents

- [AGENTS.md](AGENTS.md): read this first.
- Crates we own are `transit-*`; inherited ones are still `soundcraft-*` ([ADR 0014](docs/adr/0014-transit-gets-its-own-repository.md)).
- [docs/adr/](docs/adr/README.md): architecture decisions.
- [docs/contributing/](docs/contributing/README.md): evidence and knowledge notes.
- [docs/known-issues/](docs/known-issues/README.md): accepted, tracked limitations.
