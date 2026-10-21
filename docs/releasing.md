# Releasing transit

Releases are tag-driven. `.github/workflows/release.yml` does the building.

1. Bump `version` in `apps/transit/Cargo.toml` (for example `0.1.0-alpha.2`) and commit it.
2. Tag the commit `vX.Y.Z` or `vX.Y.Z-pre` (for example `v0.1.0-alpha.2`) and push the tag:

   ```sh
   git tag v0.1.0-alpha.2
   git push origin v0.1.0-alpha.2
   ```

3. `release.yml` builds three archives and attaches them to a GitHub pre-release for the tag:
   - Windows x86_64: WASAPI only, unsigned. SmartScreen may ask for "More info", then "Run anyway".
   - macOS arm64: unsigned and not notarized. Right-click the binary and choose Open, or run
     `xattr -dr com.apple.quarantine transit`.
   - Linux x86_64.
4. Check the pre-release page, edit the notes, and publish it when the archives look right.

## ASIO

ASIO builds (`--features transit/asio`) are local-only. They are not published until the Steinberg
ASIO SDK licensing agreement is signed; see the README for the build requirements.
