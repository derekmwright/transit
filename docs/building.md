# Building transit

## Prerequisites

- Rust 1.90 or newer (`rust-version` in `Cargo.toml`), from <https://rustup.rs>.
- Windows: the Visual Studio 2019+ Build Tools with the "Desktop development with C++" workload. The app runs on Windows 10 version 1607 or later (the plug-in editor window uses the per-monitor DPI functions that version added).
- Linux: `libasound2-dev` and `pkg-config` (for example `sudo apt install libasound2-dev pkg-config`).
- Linux, headless test runs (CI, containers, WSL2): the egui tests render with wgpu and need a Vulkan adapter. Install `mesa-vulkan-drivers libvulkan1` (Mesa's CPU driver, lavapipe) and run with `WGPU_BACKEND=vulkan VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json` (`VK_DRIVER_FILES` is the newer name for the same setting). CI sets both. On WSL2 the default GL path can segfault, so set these there too.
- macOS: the Xcode Command Line Tools (`xcode-select --install`).

## Build and run

```sh
cargo build -p transit --release
cargo run -p transit --release
```

The binary is `target/release/transit` (`transit.exe` on Windows).

## Where settings live

transit keeps `ui.json`, `Autosave/` and `logs/` in its own settings folder:

- Windows: `%APPDATA%\transit`
- macOS: `~/Library/Application Support/transit`
- Linux: `~/.config/transit` (or `$XDG_CONFIG_HOME/transit`)

Set `TRANSIT_CONFIG_DIR` to use a different folder, for example to keep a test build away from your
real settings. The plug-in scan cache is shared with the inherited soundcraft apps (`SOUNDCRAFT_CONFIG_DIR` overrides).

## The update check

transit checks once a day, after the splash, whether a newer release exists (Help > Check for Updates… runs it on demand). The check is a single anonymous request to GitHub's public API (`api.github.com/repos/transit-audio/transit/releases`); nothing else is sent, and nothing is downloaded or installed. Turn it off with Help > Check for New Versions on Launch, or set `TRANSIT_NO_UPDATE_CHECK=1` (CI, tests, packagers; this also disables the Help entry).

The code is behind the Cargo feature `update-check` of the `transit` app, on by default (it brings `ureq` on rustls, no OpenSSL). Headless and distro builds can leave it out: `cargo build -p transit --no-default-features --features native-wasapi,native-asio` has no HTTP dependency.

## Packaging

`packaging/` holds the per-platform scripts, all building `-p transit` and reading the version from
`apps/transit/Cargo.toml`: `linux/package.sh` (AppImage, deb, rpm, tar.gz; `flatpak-bundle.sh` and
the `dev.derekmwright.transit*.yml` manifests for Flatpak), `macos/package.sh` (`transit.app` in a DMG
named `transit`; the icon is `assets/app-icon/transit.icns`), `windows/package.ps1` (MSI and portable zip),
and `freebsd/package.sh`. Outputs land in `dist/release/` as `transit-<version>-<platform>-<arch>.*`.
The tagged GitHub release (`.github/workflows/release.yml`) is separate: it ships plain archives, plus on Windows an MSI built from `packaging/windows/transit.wxs` with the same WiX v5 (`dotnet tool install --global wix --version 5.0.2`) and optional signing (`docs/releasing.md`).

## Building with ASIO (Windows)

ASIO has two implementations in the tree. The native backend needs nothing beyond the Windows
prerequisites above; the app offers it with the `native-asio` feature. The cpal ASIO feature is the
legacy path (the app's ASIO path without `native-asio`, and the fallback when the native backend does
not open); it needs the Steinberg SDK and libclang, and retires with ASIO slice (e).

### Native ASIO backend (no SDK)

`crates/audio-asio` (`transit-audio-asio`) has hand-written bindings, so it needs no Steinberg SDK,
no C++ compile, no bindgen, no libclang, no `CPAL_ASIO_DIR` and no environment variables. It is
compiled only on 64-bit Windows (`cfg(all(windows, target_pointer_width = "64"))`; the crate is empty
elsewhere) and builds with a plain `cargo build`. The app uses it with the `native-asio` feature
(`cargo build --release -p transit --features native-asio`, usually beside `native-wasapi`): choose
Audio system ASIO, then Backend "Native ASIO" in Audio Devices. Without the feature the
app's ASIO path is cpal's (below). The benchmark uses it through the `audio-bench/native-asio`
feature:

```powershell
cargo build --release -p audio-bench --features audio-bench/native-asio
target\release\audio-bench.exe --list-devices --system ASIO
target\release\audio-bench.exe --backend asio --device "<driver registry name>" --loopback --out-ch 1 --in-ch 1 --seconds 10
```

Tests, no hardware needed:

```powershell
cargo test -p transit-audio-asio
```

The tests that open real drivers are `#[ignore]`d. Run them on a machine with the interface
attached and its driver installed, one at a time:

```powershell
cargo test -p transit-audio-asio -- --ignored hardware_ --nocapture --test-threads 1
```

Or pick one by name: `hardware_lists_focusrite_and_tascam`, `hardware_open_close_each_driver_twice`,
`hardware_scarlett_duplex_streams_two_seconds`, `hardware_model12_duplex_streams_two_seconds` or
`hardware_control_panel_opens_and_returns`. Design notes:
[contributing/knowledge/asio-backend.md](contributing/knowledge/asio-backend.md).

### cpal ASIO feature (legacy; needs the SDK and libclang)

The `transit/asio` feature enables `cpal`'s `asio` feature through `asio-sys`, which compiles
Steinberg's ASIO SDK and generates bindings with bindgen. Use it until the app moves to the native
backend. In CI, the `windows-asio` job (allowed to fail) builds this feature, and its first run
succeeded, which shows the SDK download works unattended on `windows-latest` with its preinstalled
LLVM. `release.yml` does not build it: the release ships one Windows download (zip and MSI) with the native
ASIO backend, and the cpal ASIO host is build-only (see Licensing below).

The one-command build used for the owner's test builds (app, bench, native WASAPI and both ASIO
paths), in a PowerShell session with the environment from the Build step below:

```powershell
cargo build --release -p transit -p audio-bench --features transit/asio,transit/native-wasapi,audio-bench/asio,audio-bench/native-wasapi,audio-bench/native-asio
```

#### Prerequisites

- Everything in the Windows list above (MSVC Build Tools with the C++ workload). `asio-sys` runs
  `vcvarsall` itself when `VCINSTALLDIR` is unset, so a plain PowerShell window is fine.
- LLVM, for bindgen's libclang: `winget install LLVM.LLVM` (installs to `C:\Program Files\LLVM`).
- The Steinberg ASIO SDK. `asio-sys` downloads it on the first build into `%TEMP%\asio_sdk`. To use
  an SDK you already unpacked, set `CPAL_ASIO_DIR` to its folder.

#### Build

The scripted version sets the environment below, purges stale bindings, and builds:

```powershell
pwsh tools\build-asio.ps1
```

It takes `-repo` (default: this checkout), `-package` (default `transit`) and `-features` (default
`transit/asio`). The equivalent by hand, in the same PowerShell session:

```powershell
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
$env:CLANG_PATH = 'C:\Program Files\LLVM\bin\clang.exe'
$env:PATH = 'C:\Program Files\LLVM\bin;' + $env:PATH
$env:BINDGEN_EXTRA_CLANG_ARGS = '--target=x86_64-pc-windows-msvc'
cargo build -p transit --release --features transit/asio
```

#### Pitfall: MinGW's libclang and E0080

If `C:\mingw64\bin` (or another MinGW) is on `PATH` and `LIBCLANG_PATH` is unset, `clang-sys` loads
MinGW's libclang, whose default target is `x86_64-w64-windows-gnu`. bindgen then lays out
`AsioDrivers` with the Itanium C++ ABI, while rustc and `cl.exe` use the MSVC ABI. The build fails with:

```
error[E0080]: ... Offset of field: AsioDrivers::connID
```

The fix is the environment above: `LIBCLANG_PATH`, `CLANG_PATH`, LLVM first on `PATH`, and
`BINDGEN_EXTRA_CLANG_ARGS=--target=x86_64-pc-windows-msvc`.

`asio-sys` takes `libclang` and the `clang` resource headers from `LIBCLANG_PATH` and `PATH`. If a
MinGW LLVM (for example `C:\mingw64in`) comes before the MSVC LLVM on `PATH`, the bindings come out in
the GNU ABI (Itanium mangling, GCC struct offsets) and the crate fails to compile with an E0080 layout
assertion. Setting `LIBCLANG_PATH` alone then fails with undeclared `__builtin_ia32_*` identifiers,
because the resource headers still come from MinGW. Put the MSVC LLVM's `bin` first on `PATH` (and in
`LIBCLANG_PATH`) for the build and run `cargo clean -p asio-sys` once.

#### Stale bindings

`asio-sys`'s build script only generates `asio_bindings.rs` when the file does not exist. Wrong
bindings from an earlier attempt stay in `target\<profile>\build\asio-sys-*\out` and keep failing
even after you fix the environment. Delete those `asio-sys-*` build folders (and the matching
`target\<profile>\.fingerprint\asio-sys-*`) before rebuilding, or run `cargo clean -p asio-sys`.
`tools\build-asio.ps1` removes bindings that contain Itanium (`_ZN`) names automatically.

#### Verify

Run the new binary, open Audio Devices, and look for your interface listed under the ASIO host.
If only WASAPI appears, the binary was built without the feature. To measure it, run
`audio-bench --backend cpal --system ASIO --device "<ASIO device>" --loopback ...` (see
[docs/benchmarks](benchmarks/README.md)); the cpal path has no hardware test suite of its own.

#### Licensing

Building the ASIO SDK into a binary for your own use is allowed by the SDK license. If you want to
redistribute an ASIO-enabled binary, you must first sign Steinberg's ASIO SDK Licensing Agreement
(free; signed and emailed to Steinberg) and show this notice: "ASIO is a trademark of Steinberg
Media Technologies GmbH, registered in Europe and other countries." The published
release does not include the cpal ASIO host; its one Windows download (zip and MSI) carries native
WASAPI and native ASIO, which need no SDK.
