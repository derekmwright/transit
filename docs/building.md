# Building transit

## Prerequisites

- Rust 1.90 or newer (`rust-version` in `Cargo.toml`), from <https://rustup.rs>.
- Windows: the Visual Studio 2019+ Build Tools with the "Desktop development with C++" workload.
- Linux: `libasound2-dev` and `pkg-config` (for example `sudo apt install libasound2-dev pkg-config`).
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
real settings. The plug-in scan cache is shared with SoundCraft (`SOUNDCRAFT_CONFIG_DIR` overrides).

## Packaging

`packaging/` holds the per-platform scripts, all building `-p transit` and reading the version from
`apps/transit/Cargo.toml`: `linux/package.sh` (AppImage, deb, rpm, tar.gz; `flatpak-bundle.sh` and
the `dev.derekmwright.transit*.yml` manifests for Flatpak), `macos/package.sh` (`transit.app` in a DMG
named `transit`; the icon is `assets/app-icon/transit.icns`), `windows/package.ps1` (MSI and portable zip),
and `freebsd/package.sh`. Outputs land in `dist/release/` as `transit-<version>-<platform>-<arch>.*`.
The tagged GitHub release (`.github/workflows/release.yml`) is separate and ships plain archives.

## Building with ASIO (Windows)

The published releases use WASAPI only. If you have an ASIO audio interface and want ASIO in your
own build, compile with the `transit/asio` feature. It enables `cpal`'s `asio` feature through
`asio-sys`, which compiles Steinberg's ASIO SDK and generates bindings with bindgen.

### Prerequisites

- Everything in the Windows list above (MSVC Build Tools with the C++ workload). `asio-sys` runs
  `vcvarsall` itself when `VCINSTALLDIR` is unset, so a plain PowerShell window is fine.
- LLVM, for bindgen's libclang: `winget install LLVM.LLVM` (installs to `C:\Program Files\LLVM`).
- The Steinberg ASIO SDK. `asio-sys` downloads it on the first build into `%TEMP%\asio_sdk`. To use
  an SDK you already unpacked, set `CPAL_ASIO_DIR` to its folder.

### Build

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

### Pitfall: MinGW's libclang and E0080

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

### Stale bindings

`asio-sys`'s build script only generates `asio_bindings.rs` when the file does not exist. Wrong
bindings from an earlier attempt stay in `target\<profile>\build\asio-sys-*\out` and keep failing
even after you fix the environment. Delete those `asio-sys-*` build folders (and the matching
`target\<profile>\.fingerprint\asio-sys-*`) before rebuilding, or run `cargo clean -p asio-sys`.
`tools\build-asio.ps1` removes bindings that contain Itanium (`_ZN`) names automatically.

### Verify

Run the new binary, open Audio Devices, and look for your interface listed under the ASIO host.
If only WASAPI appears, the binary was built without the feature.

### Licensing

Building the ASIO SDK into a binary for your own use is allowed by the SDK license. If you want to
redistribute an ASIO-enabled binary, you must first sign Steinberg's ASIO SDK Licensing Agreement
(free; signed and emailed to Steinberg) and show this notice: "ASIO is a trademark of Steinberg
Media Technologies GmbH, registered in Europe and other countries." That is why the published
releases are WASAPI-only.
