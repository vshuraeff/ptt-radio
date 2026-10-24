#!/bin/bash
# writes the README copy that ships inside a companion archive: every repository-relative link except
# LICENSE (which the archive carries) becomes an absolute URL at tag v<ver>, images on
# raw.githubusercontent.com and other links on github.com/.../blob. anchors are kept.
# usage: tools/archive-readme.sh <ver> <in> <out>
set -euo pipefail
[ $# -eq 3 ] || { echo "usage: tools/archive-readme.sh <ver> <in> <out>" >&2; exit 2; }
repo=vshuraeff/ptt-radio
awk -v blob="https://github.com/$repo/blob/v$1/" -v raw="https://raw.githubusercontent.com/$repo/v$1/" '
  function absolute(t, img,   i, path) {
    if (t == "" || t ~ /^#/ || t ~ /^\// || t ~ /^[A-Za-z][A-Za-z0-9+.-]*:/) return t
    sub(/^\.\//, "", t)
    i = index(t, "#")
    path = i ? substr(t, 1, i - 1) : t
    if (path == "LICENSE") return t
    return (img ? raw : blob) t
  }
  {
    out = ""; rest = $0
    # markdown [text](target) and ![alt](target), html src="target" and href="target"
    while (match(rest, /!?\[[^]]*\]\(|(src|href)="/)) {
      tok = substr(rest, RSTART, RLENGTH)
      out = out substr(rest, 1, RSTART + RLENGTH - 1)
      rest = substr(rest, RSTART + RLENGTH)
      if (tok ~ /"$/) { end = index(rest, "\""); img = (tok ~ /^src/) }
      else { end = match(rest, /[) ]/) ? RSTART : 0; img = (tok ~ /^!/) }
      if (end == 0) continue
      out = out absolute(substr(rest, 1, end - 1), img)
      rest = substr(rest, end)
    }
    print out rest
  }
' "$2" > "$3"
