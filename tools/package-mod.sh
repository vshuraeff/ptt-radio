#!/bin/bash
# builds the mod portal zip <out dir>/ptt-radio_<ver>.zip from mod/ptt-radio/ under the single top folder
# ptt-radio_<ver>/. dotfiles and dot-directories are left out. the zip is deterministic: entries sorted,
# fixed mtimes and modes, no extra attributes, so the same tree gives the same bytes.
# usage: tools/package-mod.sh <ver> <out dir>
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: tools/package-mod.sh <ver> <out dir>" >&2; exit 2; }
ver=$1
root=$(cd "$(dirname "$0")/.." && pwd)
src=$root/mod/ptt-radio
mkdir -p "$2"
out=$(cd "$2" && pwd)/ptt-radio_$ver.zip
top=ptt-radio_$ver

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$top"
(cd "$src" && find . -name '.*' ! -name . -prune -o -type f -print) | while IFS= read -r f; do
  mkdir -p "$stage/$top/$(dirname "$f")"
  cp "$src/$f" "$stage/$top/$f"
done

cd "$stage"
find "$top" -type d -exec chmod 755 {} +
find "$top" -type f -exec chmod 644 {} +
# zip stores local time, so the timezone is fixed too
export TZ=UTC
find "$top" -exec touch -t 202001010000 {} +
rm -f "$out"
find "$top" | LC_ALL=C sort | zip -X -q "$out" -@
echo "$out"
