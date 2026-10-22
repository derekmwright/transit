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
   - macOS arm64: unsigned and not notarized. Right-click the binary and choose Open, or run
     `xattr -dr com.apple.quarantine transit`.
   - Linux x86_64.
4. Check the pre-release page, edit the notes, and publish it when the archives look right.

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
