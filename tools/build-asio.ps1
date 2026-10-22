# NOTE: this script builds the legacy cpal ASIO feature (asio-sys, which needs the Steinberg SDK and
# libclang). The native ASIO backend, crates/audio-asio (transit-audio-asio), needs none of this: it
# builds with a plain `cargo build` (bench: `--features audio-bench/native-asio`). This script retires
# when the app moves to the native backend.
#
<#
Builds transit with the opt-in ASIO feature, from a clean or warm target directory.

Needs LLVM (winget install LLVM.LLVM) for bindgen and the Visual Studio 2019+ Build Tools with the
C++ workload (asio-sys runs vcvarsall itself when VCINSTALLDIR is unset). asio-sys downloads the
Steinberg ASIO SDK on the first build into %TEMP%\asio_sdk unless CPAL_ASIO_DIR points at an
unpacked copy. See docs/building.md for the licensing terms that apply to redistribution.

Why the environment is forced: with C:\mingw64\bin on PATH and no LIBCLANG_PATH, clang-sys loads
mingw's libclang (default triple x86_64-w64-windows-gnu). bindgen then lays out AsioDrivers with the
Itanium ABI, while rustc and cl.exe use the MSVC ABI, and the build fails with E0080 "Offset of
field: AsioDrivers::connID". asio-sys's build.rs only generates asio_bindings.rs when the file does
not exist, so wrong bindings stay in target\<profile>\build\asio-sys-*\out and break every later
build, even with a correct environment. This script fixes the environment and purges the stale
output.

Usage: pwsh tools\build-asio.ps1 [-repo <path>] [-package transit] [-features transit/asio]
CI setup only: pwsh tools\build-asio.ps1 -SetupOnly -GitHubEnvironment
#>
param(
  [string]$repo = (Split-Path $PSScriptRoot -Parent),
  [string]$package = 'transit',
  [string]$features = 'transit/asio',
  [switch]$SetupOnly,
  [switch]$GitHubEnvironment
)
$ErrorActionPreference = 'Stop'
$llvm = 'C:\Program Files\LLVM\bin'
if (-not (Test-Path -LiteralPath "$llvm\clang.exe")) { throw "LLVM not found at $llvm" }
$env:LIBCLANG_PATH = $llvm                    # clang-sys loads this libclang.dll, not mingw's
$env:CLANG_PATH = "$llvm\clang.exe"           # bindgen's resource-dir probe uses this clang
$env:PATH = "$llvm;" + $env:PATH              # LLVM before C:\mingw64\bin
# Belt and braces: force the MSVC ABI even if some other libclang gets loaded.
$env:BINDGEN_EXTRA_CLANG_ARGS = '--target=x86_64-pc-windows-msvc'
# CI can reuse setup without purging caches or launching a build.
if ($GitHubEnvironment) {
  if (-not $env:GITHUB_ENV -or -not $env:GITHUB_PATH) { throw 'GitHub environment files are required' }
  "LIBCLANG_PATH=$env:LIBCLANG_PATH" >> $env:GITHUB_ENV
  "CLANG_PATH=$env:CLANG_PATH" >> $env:GITHUB_ENV
  "BINDGEN_EXTRA_CLANG_ARGS=$env:BINDGEN_EXTRA_CLANG_ARGS" >> $env:GITHUB_ENV
  $llvm >> $env:GITHUB_PATH
  if ($env:ASIO_SDK_URL) {
    $zip = Join-Path $env:RUNNER_TEMP 'asio_sdk.zip'
    $dst = Join-Path $env:RUNNER_TEMP 'asio_sdk_unpacked'
    Invoke-WebRequest -Uri $env:ASIO_SDK_URL -OutFile $zip
    Expand-Archive -LiteralPath $zip -DestinationPath $dst -Force
    $sdk = Get-ChildItem -LiteralPath $dst -Directory | Select-Object -First 1
    if (-not $sdk -or -not (Test-Path -LiteralPath (Join-Path $sdk.FullName 'common'))) {
      throw "ASIO SDK layout unexpected in $dst"
    }
    "CPAL_ASIO_DIR=$($sdk.FullName)" >> $env:GITHUB_ENV
  }
}
if ($SetupOnly) { return }

Set-Location $repo
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }

# Purge asio-sys bindings generated with the wrong (Itanium/mingw) ABI; they are never regenerated.
Get-ChildItem "$target\*\build\asio-sys-*\out\asio_bindings.rs" -ErrorAction SilentlyContinue | ForEach-Object {
  if (Select-String -Path $_.FullName -Pattern '_ZN' -Quiet) {
    Write-Host "Removing stale mingw-ABI asio-sys build output: $($_.Directory.Parent.FullName)"
    Remove-Item -Recurse -Force $_.Directory.Parent.FullName
    Get-ChildItem "$target\*\.fingerprint\asio-sys-*" -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force
  }
}
cargo build --release -p $package --features $features
