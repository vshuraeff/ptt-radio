#!/bin/bash
# checks that the mod (mod/ptt-radio/info.json) and the companion (companion/Cargo.toml) carry one version,
# and, when a tag is given, that the tag is v<that version>. prints every mismatch; exits 1 on any.
# usage: tools/check-version.sh [<tag>]
set -uo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
info=$root/mod/ptt-radio/info.json
manifest=$root/companion/Cargo.toml

# the top-level "version" key only; the leading quote keeps "factorio_version" out
mod=$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$info" | head -n 1)
# the version line of the [package] table, never a dependency's
cargo=$(awk '/^\[/ { pkg = ($0 == "[package]") }
             pkg && /^version[[:space:]]*=/ { sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*/, ""); print; exit }' "$manifest")

failed=0
[ -n "$mod" ] || { echo "check-version: no version in mod/ptt-radio/info.json" >&2; failed=1; }
[ -n "$cargo" ] || { echo "check-version: no [package] version in companion/Cargo.toml" >&2; failed=1; }
if [ "$failed" -eq 0 ] && [ "$mod" != "$cargo" ]; then
  echo "check-version: mod/ptt-radio/info.json has $mod, companion/Cargo.toml has $cargo" >&2
  failed=1
fi
tag=${1:-}
if [ -n "$tag" ]; then
  case $tag in
    v*) [ "${tag#v}" = "$mod" ] || { echo "check-version: tag $tag does not match mod version $mod" >&2; failed=1; } ;;
    *) echo "check-version: tag $tag is not of the form v<version>" >&2; failed=1 ;;
  esac
fi
[ "$failed" -eq 0 ] && echo "check-version: $mod${tag:+, tag $tag}"
exit "$failed"
