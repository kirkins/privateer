#!/bin/sh
# Lay out one versioned release: OUT/COMMIT/bin (every binary a release
# ships), OUT/COMMIT/share/privateer/plugins, and OUT/COMMIT/SHA256SUMS
# covering every other file. CI runs this file for each platform's artifact;
# `privateer upgrade fetch --build` runs the copy built into the binary, so a
# source build has the same layout and the same checks.
#
#   release-layout.sh COMMIT BUILD_DIR PLUGINS_DIR OUT
#
# BUILD_DIR holds the release binaries (cargo's target/release). OUT/COMMIT
# must not exist yet.
set -eu

[ "$#" -eq 4 ] || { echo "usage: release-layout.sh COMMIT BUILD_DIR PLUGINS_DIR OUT" >&2; exit 2; }
commit=$1 build=$2 plugins=$3 out=$4
case "$commit" in
  *[!0-9a-f]* | "") echo "release-layout: $commit is not a full lowercase commit" >&2; exit 2 ;;
esac
[ "${#commit}" -eq 40 ] || [ "${#commit}" -eq 64 ] || {
  echo "release-layout: $commit is not a full lowercase commit" >&2; exit 2; }
root="$out/$commit"
[ ! -e "$root" ] || { echo "release-layout: $root already exists" >&2; exit 1; }

mkdir -p "$root/bin" "$root/share/privateer/plugins"
cp "$build/privateer" "$build/privateer-dashboard" "$build/privateer-mcp-broker" "$root/bin/"
# Bytecode caches are written at run time, never shipped.
tar -C "$plugins" --exclude __pycache__ -cf - . | tar -C "$root/share/privateer/plugins" -xf -

cd "$root"
# A release is plain files and directories: no links, and no names a
# SHA256SUMS line could not carry.
if [ -n "$(find . -type l)" ]; then
  echo "release-layout: a release holds no links:" >&2; find . -type l >&2; exit 1
fi
if find . | grep -q '[^A-Za-z0-9._/@+-]'; then
  echo "release-layout: unsupported file names:" >&2; find . | grep '[^A-Za-z0-9._/@+-]' >&2; exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then sum="sha256sum"; else sum="shasum -a 256"; fi
find . -type f ! -path ./SHA256SUMS ! -path ./SHA256SUMS.new | sed 's|^\./||' | LC_ALL=C sort | xargs $sum > SHA256SUMS.new
mv SHA256SUMS.new SHA256SUMS

# The image must know itself as this exact release.
bin/privateer upgrade status | grep -q "\"running_commit\": \"$commit\"" || {
  echo "release-layout: bin/privateer does not report itself as $commit" >&2; exit 1; }
