<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/transit-wordmark-white-2172.png">
    <source media="(prefers-color-scheme: light)" srcset="assets/brand/transit-wordmark-graphite-2172.png">
    <img alt="transit" src="assets/brand/transit-wordmark-graphite-2172.png" width="360">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/derekmwright/transit/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/derekmwright/transit/actions/workflows/ci.yml/badge.svg"></a>
</p>

A desktop DAW in pure Rust with Ableton/Logic-style ergonomics and its own audio engine, device
layer and egui UI. transit began in October 2026 as a fork of
[SoundCraft](https://github.com/storytold/soundcraft) and has diverged since; the lineage is
credited in About and in NOTICE.

> [!NOTE]
> transit is alpha software under active development; the current pre-release is [v0.2.0-alpha.1](https://github.com/derekmwright/transit/releases/tag/v0.2.0-alpha.1) (see [Downloads](#downloads)). transit checks once a day for a newer release with one anonymous request to GitHub and tells you; turn it off in the Help menu or with `TRANSIT_NO_UPDATE_CHECK=1` ([details](docs/building.md#the-update-check)). The current priority is core engine
> correctness, stability and performance, measured on real hardware, ahead of new features.
> Expect rough edges in the UI and some missing features; the audio engine, recording alignment
> and device handling get fixed first, following [the engineering principles](docs/principles.md).
> Releases are frequent, and each lists its
> [known issues](docs/known-issues/README.md). Measurements and bug reports are the most valuable
> help right now: see [How to help](#how-to-help).

## Downloads

The current pre-release is [v0.2.0-alpha.1](https://github.com/derekmwright/transit/releases/tag/v0.2.0-alpha.1).

- **Windows (x86_64):** one download, the `.zip` (portable) or the `.msi` (installer; it registers `.transit` and `.transitz`). It has native WASAPI and native ASIO (experimental).
- **macOS (Apple silicon):** the `.dmg`; open it and drag `transit.app` to Applications.
- **Linux (x86_64):** the tarball; unpack it and run `transit`.

The builds are unsigned. On Windows, SmartScreen shows "More info", then "Run anyway". On macOS, if the app is blocked, run `xattr -dr com.apple.quarantine /Applications/transit.app` or right-click it and choose Open.

![Arrangement view](docs/images/transit-arrange.png)

![Mixer](docs/images/transit-mixer.png)
![Piano roll](docs/images/transit-pianoroll.png)

## How to help

transit is measured on real hardware, and the bench has only two Windows interfaces on it. Loopback
measurements from other machines are the most useful contribution right now.

### What we are looking for

- macOS with any Core Audio interface. We have no Mac; aggregate devices especially.
- Linux with ALSA, JACK or PipeWire.
- Windows with an interface other than the Focusrite Scarlett 2i2 or TASCAM Model 12 (RME, MOTU, Universal Audio, Behringer, Audient, PreSonus, SSL, onboard Realtek), on both its ASIO and its WASAPI path.
- Multichannel interfaces in general.

### What to run

`audio-bench` plays clicks out of one output and finds them again on one input. Cable one output
of your interface into one input of the same interface, turn the monitors down, set the input gain
to about the middle and the input to Line/Inst, and close other audio apps. From the next release
the bench ships in every release zip; until then build it with `cargo xtask audio-bench` (see
[docs/benchmarks](docs/benchmarks/README.md)). List your devices first (`--list-devices --system WASAPI`
or `--system ASIO`), then run, with your own names and channels:

```
audio-bench --backend cpal --system WASAPI --device "<output>" --in-device "<input>" --loopback --out-ch 1 --in-ch 1 --seconds 10 --out results.md
audio-bench --backend wasapi --loopback --out-device "<output>" --in-device "<input>" --out-ch 1 --in-ch 1 --seconds 10 --out results.md
audio-bench --backend cpal --system ASIO --device "<ASIO device>" --loopback --out-ch 1 --in-ch 1 --seconds 10 --out results.md
```

The first is shared WASAPI through cpal; on macOS and Linux use the platform's default system the
same way. The second is transit's native WASAPI backend and the third is ASIO, both Windows only.

### What to send

- The table row the bench prints, and the `render backend:` and `capture backend:` lines under it.
- The interface, driver version, OS build and CPU, and whether all the clicks were detected.

File it with the [measurement issue template](https://github.com/derekmwright/transit/issues/new?template=measurement.md)
or wherever you find us. <!-- TODO(owner): add the Discord invite here; the repo names none yet. -->

### Other ways to help

- Try the latest release and report what breaks, with the steps to make it happen.
- Read the [known issues](docs/known-issues/README.md) and pick one.
- Review an [ADR](docs/adr) draft when one is posted.

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
- MIDI recording: arm an instrument track and record, with a count-in. Overdub merges into the clip under the pass. With a loop on, each lap is committed as a stacked take and recording continues; stopping mid-lap keeps the partial lap, and the status line reports the take count. Held notes are cut at the loop end; resuming them in the next lap is planned.
- A MIDI take journal is written during the pass and kept until an autosave that holds the recording lands; after a crash, the next launch recovers it as a `<Track>_recovered` clip. Record works without a validated device clock: alignment is approximate, a take recorded that way says so, and when recording cannot start you get a plain-English reason.
- A hosted plug-in that re-sends an unchanged parameter value no longer registers an automation touch; only a real change does.

### Audio devices

- Audio Devices window to pick the driver (WASAPI, or ASIO (native ASIO in the release; the cpal ASIO host only in builds you make yourself)), device and buffer size.
- Named input and output paths, saved per machine.
- Experimental native WASAPI backend (build with the `native-wasapi` feature; pick it under Audio Devices > Backend), with a cpal fallback. It plays, captures (filling any lost packets with zeros, so a take keeps its length) and runs duplex: an input is paired with its output by device, chosen with the Paired input selector. It opens the device shared by default; a "Take exclusive control of the device" checkbox opens it exclusive instead (other apps are silenced while transit runs), and an exclusive open that fails steps down to shared, with a message, rather than failing silently. `audio-bench --backend wasapi` measures it. Verified on a Scarlett 2i2 in shared and exclusive mode (a Model 12 run is pending). On Windows, native WASAPI gives any interface a dependable, calibrated stereo path with no SDK and no extra driver install. How many channels it sees is decided by the vendor's WDM driver: the TASCAM Model 12 registers one stereo playback and one stereo recording endpoint, so WASAPI shows 2 in and 2 out on it in every mode, and individual channels cannot be recorded over WASAPI. Multichannel and tracking-grade latency on Windows are the ASIO path (a native ASIO backend is in progress).
- Experimental native ASIO backend (build with the `native-asio` feature; no Steinberg SDK and no LLVM): choose Audio system ASIO, then Backend > Native ASIO. It lists every installed driver and says which failed to load, opens every channel of the interface (all 12 inputs and 10 outputs of the TASCAM Model 12, named as the driver names them), plays, records and runs hardware inserts duplex, shows the buffer, sample format and latency the driver reports, and opens the driver's own settings (Driver settings). About 6 ms round trip on the Scarlett 2i2 at 64 samples. A driver reset request stops playback with a message to reconnect.
- Loopback calibration: `audio-bench --backend wasapi --loopback --settings` measures the interface's round trip and saves it, and the timing label then reads "calibrated". Over native ASIO the Calibration line in Audio Devices has a Measure button: a click through a cable from an output to an input times the loop at the current buffer size and rate, recordings are placed from it, and the driver's own latency figures are shown for reference only. Shared mode is not a low-latency path (about 248 ms round trip on the Scarlett 2i2); exclusive mode measured about 223 ms there (and about 276 ms, often silent, on a Model 12), and ASIO about 23 ms, so ASIO remains the tracking path on that interface.
- Tracks, buses and sends route to any output path, with latency aligned across them.

### Sessions and rendering

- Bounce the main mix or any bus over the selection or loop range to WAV, AIFF or FLAC at 16-bit, 24-bit or 32-bit float.
- Stems by output path from the Bounce dialog.
- Bounce in Place turns a track's clips into audio.
- Full undo history; every edit is a command.
- Autosave, and sessions saved as versioned, human-readable JSON in `.transit` files.
- A launch splash that reports the audio device opening first, then plug-in scan progress, and Help > About with version info and a link to the transit repository.
- Import audio and MIDI files; export MIDI. File > Open Recent lists your last ten sessions (Ctrl+Shift+O).

### Under the hood

- An audio backend trait sits between the Player and the audio system, with cpal behind it as the first adapter.
- One renderer: playback and offline render both use the plan renderer; the legacy renderer is gone.
- Offline render and playback are checked against each other by golden tests.
- Every edit is a named command, which is what makes undo and the command catalog uniform.

## Status

Pre-alpha. The product is now called transit, and its settings folder moved from `transitDAW` to `transit` (your settings are moved once on first launch). Windows is tested daily; macOS and Linux build but are untested (their packaging ids are `dev.derekmwright.transit`). The packaging scripts under `packaging/` build the transit app on every platform ([docs/building.md](docs/building.md#packaging)). The Windows release is one download (zip and MSI) with native WASAPI and native ASIO; the cpal ASIO host is build-only (`--features asio`, needs the Steinberg SDK and LLVM) and you can build it yourself ([docs/building.md](docs/building.md#building-with-asio-windows)).
Expect rough edges and breaking changes to the session format. Back up anything you care about.

## Build

You need a Rust toolchain at 1.90 or newer (`rust-version` in `Cargo.toml`). On Linux, install
`libasound2-dev` and `pkg-config` first.

Every pull request and push to `main` is checked with clippy, tests and the layer rules on Windows,
Linux and macOS, plus the web build and the ASIO and native WASAPI features. Hardware tests are
never CI gates.

```sh
cargo build -p transit --release
```

The binary is `target/release/transit` (`transit.exe` on Windows).

### ASIO on Windows (optional)

The native ASIO backend (`crates/audio-asio`, used by `audio-bench --backend asio`) needs nothing
beyond the Windows prerequisites: no SDK, LLVM or environment variables. The app's ASIO path is still
cpal's, behind the `transit/asio` feature (`cargo build -p transit --release --features transit/asio`),
which needs the Visual Studio 2019+ Build Tools, LLVM and the Steinberg ASIO SDK (`asio-sys` downloads
it on the first build unless `CPAL_ASIO_DIR` points at a copy). Releases do not include it; the one Windows download
carries the native ASIO backend. Steinberg's licensing terms apply to anyone who redistributes
binaries built with the cpal ASIO feature.

Full steps, the MinGW/libclang pitfall, and the hardware tests: [docs/building.md](docs/building.md#building-with-asio-windows); scripted build of the cpal feature: `tools/build-asio.ps1`.

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

`transit --control <port>` (or `TRANSIT_CONTROL_PORT`; `0` picks a free port) starts a JSON-lines control server for scripts and the MCP server. It binds to `127.0.0.1` only. At start it writes a random per-run token to `control-token` in the settings folder (override the path with `TRANSIT_CONTROL_TOKEN_FILE`) and logs the path; each connection must send `auth <token>` as its first line or it is closed. `soundcraft-cli mcp --connect <port>` reads the file itself. The MCP tools are `list_commands`, `execute`, `batch`, `inspect_session`, `new_session`, `open_session`, `save_session`, `import_audio`, `bounce_mix`, and, with the app, `screenshot`, `ui_inspect`, `ui_set`, `click`, `drag` and `key`. See `docs/control-protocol.md` and `docs/mcp.md`.

## Status and known issues

- Pre-alpha; the session format and UI change freely.
- A heavy plug-in can still block the UI for the duration of its single create call.
- Approximate record alignment on devices whose clock never validates can differ by up to one input block (K14); Freeze renders a fixed 2 second tail.
- The Windows release is one download (zip and MSI) with native WASAPI and native ASIO. It is unsigned: SmartScreen shows "More info", then "Run anyway".

## Lineage

transit began in October 2026 as a fork of [SoundCraft](https://github.com/storytold/soundcraft) by
storytold, an open-source Rust digital audio workstation. transit has its own audio engine (native
WASAPI and native ASIO backends) and its own product direction; the plug-in hosts and a number of
crates were inherited and keep their `soundcraft-*` names until they are renamed. Engine fixes that
apply upstream may be offered back.

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

ASIO is a trademark of Steinberg Media Technologies GmbH, registered in Europe and other countries.
