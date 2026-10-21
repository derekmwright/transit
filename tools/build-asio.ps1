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
#>
param(
  [string]$repo = (Split-Path $PSScriptRoot -Parent),
  [string]$package = 'transit',
  [string]$features = 'transit/asio'
)
$llvm = 'C:\Program Files\LLVM\bin'
$env:LIBCLANG_PATH = $llvm                    # clang-sys loads this libclang.dll, not mingw's
$env:CLANG_PATH = "$llvm\clang.exe"           # bindgen's resource-dir probe uses this clang
$env:PATH = "$llvm;" + $env:PATH              # LLVM before C:\mingw64\bin
# Belt and braces: force the MSVC ABI even if some other libclang gets loaded.
$env:BINDGEN_EXTRA_CLANG_ARGS = '--target=x86_64-pc-windows-msvc'
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
