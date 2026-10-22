#!/usr/bin/env bash
# Exercise the packaging reader without building or launching the app.
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p target/tooling-tests
fixture="$(mktemp -d target/tooling-tests/version.XXXXXX)"
mkdir -p "$fixture/packaging"
tr -d '\r' < packaging/env.sh > "$fixture/packaging/env.sh"

for ending in lf crlf; do
  printf '[package]\nversion = "9.9.9"\n[workspace.package]\nversion = "1.2.3-test"\n' > "$fixture/Cargo.toml"
  if [ "$ending" = crlf ]; then
    sed 's/$/\r/' "$fixture/Cargo.toml" > "$fixture/crlf.toml"
    mv "$fixture/crlf.toml" "$fixture/Cargo.toml"
  fi
  actual="$(DIST="$fixture/dist" TRANSIT_VERSION='' TRANSIT_BUILD_SHA=fixture bash -c '. "$1"; printf "%s" "$VERSION"' _ "$fixture/packaging/env.sh")"
  test "$actual" = '1.2.3-test'
done
actual="$(DIST="$fixture/dist" TRANSIT_VERSION='2.3.4-beta.1' TRANSIT_BUILD_SHA=fixture bash -c '. "$1"; printf "%s" "$VERSION"' _ "$fixture/packaging/env.sh")"
test "$actual" = '2.3.4-beta.1'
printf '[workspace.package]\nedition = "2024"\n' > "$fixture/Cargo.toml"
if DIST="$fixture/dist" TRANSIT_VERSION='' TRANSIT_BUILD_SHA=fixture bash -c '. "$1"' _ "$fixture/packaging/env.sh" 2> "$fixture/missing-version.log"; then
  echo 'missing version was accepted' >&2
  exit 1
fi
printf 'packaging shell checks: ok\n'
