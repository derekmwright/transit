# Device-free checks for the setup-only ASIO path and packaging version readers.
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$testRoot = Join-Path $repoRoot ('target/tooling-tests/' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$keys = @('LIBCLANG_PATH', 'CLANG_PATH', 'PATH', 'BINDGEN_EXTRA_CLANG_ARGS', 'GITHUB_ENV', 'GITHUB_PATH', 'ASIO_SDK_URL', 'RUNNER_TEMP', 'TRANSIT_VERSION')
$saved = @{}
foreach ($key in $keys) { $saved[$key] = [Environment]::GetEnvironmentVariable($key) }

# Only the fixed LLVM probe is simulated; all output and SDK layout checks use real files.
$hasLlvm = $true
$emptySdk = $false
function Test-Path {
  param([string]$LiteralPath)
  if ($LiteralPath -eq 'C:\Program Files\LLVM\bin\clang.exe') { return $hasLlvm }
  Microsoft.PowerShell.Management\Test-Path -LiteralPath $LiteralPath
}
function Invoke-WebRequest {
  param($Uri, $OutFile)
  if ($Uri -ne 'https://example.invalid/asio.zip') { throw 'unexpected SDK request' }
  [IO.File]::WriteAllText($OutFile, 'fixture archive')
}
function Expand-Archive {
  param($LiteralPath, $DestinationPath, [switch]$Force)
  if (-not [IO.File]::Exists($LiteralPath)) { throw 'SDK archive was not downloaded' }
  [IO.Directory]::CreateDirectory($DestinationPath) | Out-Null
  if (-not $emptySdk) { [IO.Directory]::CreateDirectory((Join-Path $DestinationPath 'asiosdk_fixture/common')) | Out-Null }
}
function cargo { throw 'setup-only must not launch cargo' }
function Assert-Equal($actual, $expected) {
  if ($actual -ne $expected) { throw "expected '$expected', got '$actual'" }
}
function Assert-Fails([scriptblock]$operation, [string]$message) {
  try { & $operation } catch {
    if ($_.Exception.Message -notlike "*$message*") { throw }
    return
  }
  throw "expected failure: $message"
}

try {
  $env:ASIO_SDK_URL = ''
  $env:GITHUB_ENV = Join-Path $testRoot 'github-env'
  $env:GITHUB_PATH = Join-Path $testRoot 'github-path'
  $env:RUNNER_TEMP = Join-Path $testRoot 'sdk'
  [IO.Directory]::CreateDirectory($env:RUNNER_TEMP) | Out-Null
  & "$repoRoot/tools/build-asio.ps1" -SetupOnly -GitHubEnvironment
  Assert-Equal $env:LIBCLANG_PATH 'C:\Program Files\LLVM\bin'
  Assert-Equal $env:CLANG_PATH 'C:\Program Files\LLVM\bin\clang.exe'
  Assert-Equal $env:BINDGEN_EXTRA_CLANG_ARGS '--target=x86_64-pc-windows-msvc'
  Assert-Equal ($env:PATH.Split(';')[0]) $env:LIBCLANG_PATH
  $exports = Get-Content -LiteralPath $env:GITHUB_ENV
  foreach ($name in @('LIBCLANG_PATH', 'CLANG_PATH', 'BINDGEN_EXTRA_CLANG_ARGS')) {
    if ($exports -notcontains "$name=$([Environment]::GetEnvironmentVariable($name))") { throw "missing export: $name" }
  }
  Assert-Equal (Get-Content -LiteralPath $env:GITHUB_PATH) $env:LIBCLANG_PATH

  $env:ASIO_SDK_URL = 'https://example.invalid/asio.zip'
  & "$repoRoot/tools/build-asio.ps1" -SetupOnly -GitHubEnvironment
  $expectedSdk = [IO.Path]::GetFullPath((Join-Path $env:RUNNER_TEMP 'asio_sdk_unpacked/asiosdk_fixture'))
  if ((Get-Content -LiteralPath $env:GITHUB_ENV) -notcontains "CPAL_ASIO_DIR=$expectedSdk") { throw 'missing SDK export' }

  $emptySdk = $true
  $env:RUNNER_TEMP = Join-Path $testRoot 'empty-sdk'
  [IO.Directory]::CreateDirectory($env:RUNNER_TEMP) | Out-Null
  Assert-Fails { & "$repoRoot/tools/build-asio.ps1" -SetupOnly -GitHubEnvironment } 'ASIO SDK layout unexpected'
  $hasLlvm = $false
  Assert-Fails { & "$repoRoot/tools/build-asio.ps1" -SetupOnly } 'LLVM not found'
  $hasLlvm = $true
  $env:GITHUB_ENV = ''
  Assert-Fails { & "$repoRoot/tools/build-asio.ps1" -SetupOnly -GitHubEnvironment } 'GitHub environment files are required'

  # Execute only the version-reader prefix; no build, signing, or executable launch.
  $packager = Get-Content -Raw -LiteralPath "$repoRoot/packaging/windows/package.ps1"
  $prefix = $packager.Substring(0, $packager.IndexOf('$Target = switch')).Replace('$PSScriptRoot', '(Join-Path $repoRoot "packaging/windows")')
  $env:TRANSIT_VERSION = ''
  $version = & {
    & ([scriptblock]::Create($prefix + [Environment]::NewLine + '$Version'))
  }
  $workspaceVersion = [regex]::Match((Get-Content -Raw -LiteralPath "$repoRoot/Cargo.toml"), '(?s)\[workspace.package\].*?version = "([^"]+)"').Groups[1].Value
  Assert-Equal $version $workspaceVersion
  $env:TRANSIT_VERSION = '2.3.4-beta.1'
  $version = & {
    & ([scriptblock]::Create($prefix + [Environment]::NewLine + '$Version'))
  }
  Assert-Equal $version '2.3.4-beta.1'
  Write-Output 'tooling PowerShell checks: ok'
} finally {
  foreach ($key in $keys) { [Environment]::SetEnvironmentVariable($key, $saved[$key]) }
}
