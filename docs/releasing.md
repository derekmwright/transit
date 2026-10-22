# Releasing transit

Releases are tag-driven. `.github/workflows/release.yml` does the building.

1. Bump `version` in `apps/transit/Cargo.toml` (for example `0.1.0-alpha.2`) and commit it.
2. Tag the commit `vX.Y.Z` or `vX.Y.Z-pre` (for example `v0.1.0-alpha.2`) and push the tag:

   ```sh
   git tag v0.1.0-alpha.2
   git push origin v0.1.0-alpha.2
   ```

3. `release.yml` builds four archives and attaches them to a GitHub pre-release for the tag:
   - Windows x86_64 (two zips): `windows-x86_64-asio` (ASIO and native WASAPI; see below) and
     `windows-x86_64` (WASAPI only, the fallback). Both unsigned. SmartScreen may ask for "More info", then "Run anyway".
   - macOS arm64: `transit-<version>-macos-aarch64.dmg` (`transit.app` with a drag-to-Applications
     window, built by `packaging/macos/package.sh`) and a `macos-arm64` tarball with the bare binary
     and `audio-bench`. Ad-hoc signed and not notarized unless the signing secrets below are set.
     If macOS blocks the app, run `xattr -dr com.apple.quarantine /Applications/transit.app` or
     right-click it and choose Open.
   - Linux x86_64.
4. Check the pre-release page, edit the notes, and publish it when the archives look right.

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

## ASIO

The `windows-x86_64-asio` zip is built with the cpal ASIO feature (`--features transit/asio`), so the
job needs the Steinberg ASIO SDK. By default `asio-sys` downloads it from steinberg.net during the
build, using the LLVM preinstalled on `windows-latest`. To use an SDK zip the owner hosts instead, set
the optional repository secret `ASIO_SDK_URL` to its URL; the workflow unpacks it and sets
`CPAL_ASIO_DIR`. The native ASIO backend needs none of this and will replace the cpal path once the
app uses it.

Distributing an ASIO-enabled binary is subject to Steinberg's ASIO SDK license, which the owner
handles. The zip carries the required notice in `README-WINDOWS.txt`: "ASIO is a trademark of Steinberg Media
Technologies GmbH, registered in Europe and other countries."
