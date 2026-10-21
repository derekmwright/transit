# transit

A desktop DAW built on the [SoundCraft](https://github.com/storytold/soundcraft) engine, with
Ableton/Logic-style ergonomics. Pure Rust, egui UI.

![Arrangement view](docs/images/transit-arrange.png)

![Mixer](docs/images/transit-mixer.png)
![Piano roll](docs/images/transit-pianoroll.png)

## Features

### Arrangement

- Adaptive grid down to 1/128, with Ctrl+1-4 for narrower, wider, triplet and snap on/off.
- Clips snap to the grid as you drag; Ctrl-drag or Alt-drag copies; Shift bypasses snap.
- Loop strip on the ruler: drag to make a loop, drag its edges to resize it, double-click to toggle it. The loop follows tempo changes.
- Automation lanes under any track for volume, pan, mute, sends and every device parameter; Read, Touch, Latch and Write modes.
- Tempo and meter changes along the timeline, edited by dragging or typing in the transport.
- Piano roll with a velocity lane, and step entry: play notes on your keyboard to enter them at the cursor (no MIDI recording yet; it is next).

### Mixer and routing

- Per-track device chains: drag a device to reorder the chain; Alt-drag copies it.
- Buses and sends, with a warning for routes that point nowhere and a refusal for routing loops.
- Sidechain any compressor or gate from any track or bus, pre- or post-fader, latency-aligned.
- Hosted plug-ins can take a sidechain key too.
- Plug-in delay compensation keeps every path time-aligned.
- Opens every output on your interface; name your own paths and route tracks, buses and sends to any of them.

### Instruments and effects

- Hosts CLAP and VST3 plug-ins, with their own editor windows, saved state, and reported latency.
- Plug-in browser with a background scan and your own search folders.
- Built-in instruments: Subtractive Synth, Drum Synth, Sampler.
- Built-in dynamics and EQ: Compressor/Limiter, Expander/Gate, De-Esser, Maximizer, Channel Strip, EQ 7-Band, EQ 1-Band.
- Built-in effects: Room and Plate Reverb, Mod Delay, Chorus, Flanger, Phaser, Saturator, Lo-Fi, Rectifier, Pitch Shifter.
- Utilities: Gain, Trim, Invert, Time Shift, DC Offset Removal, Signal Generator, Dither.

### MIDI

- Play hosted instruments live from a USB controller; on Windows it uses the native WinMM driver.
- Input without usable timestamps is still played, with approximate timing, rather than dropped.

### Audio devices

- Audio Devices window to pick the driver (WASAPI, or ASIO in builds you make yourself), device and buffer size.
- Named input and output paths, saved per machine.
- Tracks, buses and sends route to any output path, with latency aligned across them.

### Sessions and rendering

- Bounce the main mix or any bus over the selection or loop range to WAV, AIFF or FLAC at 16-bit, 24-bit or 32-bit float.
- Bounce in Place turns a track's clips into audio.
- Full undo history; every edit is a command.
- Autosave, and sessions saved as versioned, human-readable JSON.
- Import audio and MIDI files; export MIDI.

### Under the hood

- Pure Rust: egui UI on the SoundCraft engine.
- Offline render runs on the same plan renderer as playback, and golden tests check that the two agree.
- Every edit is a named command, which is what makes undo and the command catalog uniform.

## Status

Pre-alpha. Windows is tested daily; macOS and Linux build but are untested. Releases are WASAPI-only;
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

Settings, autosaves and logs live in a per-user folder: `%APPDATA%\transitDAW` on Windows,
`~/Library/Application Support/transitDAW` on macOS, `$XDG_CONFIG_HOME/transitdaw` (or
`~/.config/transitdaw`) elsewhere. Set `TRANSIT_CONFIG_DIR` to use another folder. An empty value
disables saving settings.

## Status and known issues

- Pre-alpha; the session format and UI change freely.
- Loading a plug-in can block the UI while it initializes.
- Windows releases are unsigned: SmartScreen shows "More info", then "Run anyway".

## Based on SoundCraft

transit is based on [SoundCraft](https://github.com/storytold/soundcraft) by storytold, an
open-source Rust digital audio workstation. The audio engine, plug-in hosts and most crates come
from it; the transit UI and product work are by @derekmwright. The internal crates keep their
`soundcraft-*` names. Engine fixes may be offered back upstream.

## License

MIT OR Apache-2.0, at your option: see [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)
and [NOTICE](NOTICE).

## Contributing

Issues are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
