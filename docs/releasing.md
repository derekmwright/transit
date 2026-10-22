# Releasing transit

Releases are tag-driven. `.github/workflows/release.yml` does the building.

1. Bump `[workspace.package] version` in the root `Cargo.toml` (for example `0.2.0-alpha.1`; the app and
   every crate inherit it), refresh `Cargo.lock` with `cargo metadata`, commit, push, and wait for CI to be green.
2. Tag that commit `vX.Y.Z` or `vX.Y.Z-pre` (for example `v0.2.0-alpha.1`) and push the tag:

   ```sh
   git tag v0.2.0-alpha.1
   git push origin v0.2.0-alpha.1
   ```

3. `release.yml` builds four archives and attaches them to a GitHub pre-release for the tag:
   - Windows x86_64 (one download, a `.msi` installer and a portable `.zip`): `windows-x86_64`
     (native WASAPI and native ASIO; no cpal ASIO, see below). Unsigned unless the
     Windows signing secrets below are set. SmartScreen may ask for "More info", then "Run anyway".
   - macOS arm64: `transit-<version>-macos-aarch64.dmg` (`transit.app` with a drag-to-Applications
     window, built by `packaging/macos/package.sh`) and a `macos-arm64` tarball with the bare binary
     and `audio-bench`. Ad-hoc signed and not notarized unless the signing secrets below are set.
     If macOS blocks the app, run `xattr -dr com.apple.quarantine /Applications/transit.app` or
     right-click it and choose Open.
   - Linux x86_64.
4. Check the pre-release page and publish it when the archives look right. The workflow body is generic:
   paste or replace it after publication with `gh release edit <tag> --notes-file <file>`.

## macOS signing and notarization (optional)

With all five repository secrets set, the macOS job imports the certificate into a temporary
keychain, signs `transit.app` and the DMG (hardened runtime, `packaging/macos/entitlements.plist`),
submits them with `notarytool --wait` and staples the tickets. With any of them missing the step logs
that it is skipping and the DMG is ad-hoc signed. The owner provides:

- `MACOS_CERTIFICATE_P12`: a Developer ID Application certificate exported with its private key
  as a `.p12`, base64-encoded (`base64 -i certificate.p12 | pbcopy`).
- `MACOS_CERTIFICATE_PASSWORD`: the password chosen when exporting the `.p12`.
- `APPLE_ID`: the Apple ID of the developer account.
- `APPLE_TEAM_ID`: the 10-character team ID.
- `APPLE_APP_PASSWORD`: an app-specific password for that Apple ID (appleid.apple.com).

When the build was notarized, say so in the release notes; they say the app is unsigned otherwise.

## Windows MSI and signing (optional)

Each Windows build also produces `transit-<tag>-<name>.msi`, built from `packaging/windows/transit.wxs`
with WiX v5 (`dotnet tool install --global wix --version 5.0.2` in the job; the source uses the WiX
v4 schema, which the WiX v3 on `windows-latest` cannot compile). It installs `transit.exe` and
`audio-bench.exe` per machine into Program Files (UAC prompt), adds a Start Menu shortcut and the
`.transit` and `.transitz` associations, and offers transit under "Open with"
for audio and MIDI files. The zip stays as the portable form. The upgrade code in the `.wxs` never changes.

An MSI version is numeric `major.minor.build`, so the tag `v0.2.0` becomes `0.2.0` and a pre-release
tag such as `v0.1.0-alpha.1` becomes `0.1.0` (the suffix is dropped; the workflow fails if the result
is not three numeric fields). The full version is kept in the installer's comments (Apps and
features) and in `HKLM\Software\transit\Version`. Same-version upgrades are allowed, so successive
pre-releases of one `X.Y.Z` replace each other. Successive Windows MSIs share one product,
so installing a newer one replaces the older.

With both repository secrets set, `packaging/windows/sign.ps1` signs `transit.exe`, `audio-bench.exe`
and the MSI with `signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256` (the files in
the zip are the signed ones). With either missing it logs that the files are unsigned and the build
continues. The owner provides:

- `WINDOWS_CERTIFICATE_PFX`: a code-signing certificate with its private key exported as a `.pfx`,
  base64-encoded (`[Convert]::ToBase64String([IO.File]::ReadAllBytes("cert.pfx"))`).
- `WINDOWS_CERTIFICATE_PASSWORD`: the password chosen when exporting the `.pfx`.

(`sign.ps1` also supports Azure Trusted Signing through `AZURE_*` secrets.) An EV or OV
code-signing certificate costs money (roughly a few hundred dollars a year; EV needs a hardware
token or a cloud signing service). Until the releases are signed, and until a signed publisher
builds reputation with SmartScreen, Windows warns on download and install: "More info", then "Run anyway".
Say in the release notes when the build was signed; the notes say unsigned otherwise.

## ASIO

The release has one Windows download (zip and MSI). It carries the native ASIO backend
(`transit/native-asio`), which is SDK-free and needs no Steinberg agreement. The cpal ASIO host
(`--features transit/asio`) compiles against the Steinberg ASIO SDK, so it is build-only: it is not
released, and `release.yml` does not build it. To build it yourself you need the SDK and LLVM; see
docs/building.md. Distributing a binary built with it requires Steinberg's ASIO SDK license.
