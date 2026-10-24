#!/bin/bash
# read-only publication checks; output contains locations, never matched content
set -euo pipefail

if [ "$#" -gt 1 ] || [ "${1:-}" = --help ]; then
  echo 'usage: tools/public-audit.sh [<revision-or-range>]'
  echo 'scans the working tree and every reachable snapshot in HEAD (or the supplied range).'
  echo 'requires git with PCRE support. binary contents are skipped; review them separately.'
  exit 2
fi

project=$(cd "$(dirname "$0")/.." && pwd)
repo=$(git -C "$project" rev-parse --show-toplevel)
prefix=$(git -C "$project" rev-parse --show-prefix)
scope=${prefix:-.}
range=${1:-HEAD}
if [[ "$range" = -* ]]; then
  echo 'public-audit: expected a revision or range, not an option' >&2
  exit 2
fi
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
found=0

categories=(home-path private-host email credential exchange)
patterns=(
  '/(?:Users|home|root)/[^/\s"\x27]+|[A-Za-z]:[\\/]+Users[\\/]+[^\\/\s"\x27]+'
  '(?i)(?:["\x27/]|^)(?:[a-z0-9-]+\.)+(?:local|internal|lan|home|corp|cloud)(?=["\x27/:\s]|$)'
  '(?i)\b[a-z0-9._%+-]+@(?!(?:users\.)?noreply\.github\.com\b)[a-z0-9.-]+\.[a-z]{2,}\b'
  '(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|(?:AKIA|ASIA)[A-Z0-9]{16}|xox[baprs]-[A-Za-z0-9-]{20,}|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----|(?i:(?:api[_-]?key|access[_-]?token|password|secret)\s*[=:]\s*["\x27][A-Za-z0-9_+/=-]{20,}["\x27]))'
  '(?i)(?:c[l]aude|co[d]ex)-[0-9]{3,}|[/\\]e[x]change[/\\]|e[x]change[- ](?:root|catalog)|каталог[а-я ]*обмена задачи'
)

scan() {
  local label=$1 index=$2 rc file directory=$repo
  shift 2
  [ "$label" != metadata ] || directory=$scratch
  rc=0
  # git grep status 1 means no matches; errors must fail the audit
  git -C "$directory" grep -I -l -z -P -e "${patterns[$index]}" "$@" > "$scratch/matches" 2> "$scratch/error" || rc=$?
  if [ "$rc" -gt 1 ]; then
    printf 'public-audit: git grep failed for %s (requires PCRE support); no clean result\n' "${categories[$index]}" >&2
    exit 2
  fi
  while IFS= read -r -d '' file; do
    found=1
    if [ "$label" = metadata ]; then
      printf '%s\t%s\t(commit metadata)\n' "${categories[$index]}" "$commit"
    else
      printf '%s\t%s\t%q\n' "${categories[$index]}" "$label" "${file#"$label":}"
    fi
  done < "$scratch/matches"
}

git -C "$repo" rev-list "$range" -- "$scope" > "$scratch/commits"
for index in "${!categories[@]}"; do
  scan worktree "$index" --untracked --exclude-standard -- "$scope"
done
while IFS= read -r commit; do
  git -C "$repo" show -s --format='%B%n%an <%ae>%n%cn <%ce>' "$commit" > "$scratch/metadata"
  for index in "${!categories[@]}"; do
    scan "$commit" "$index" "$commit" -- "$scope"
    scan metadata "$index" --no-index -- metadata
  done
done < "$scratch/commits"

if [ "$found" -eq 0 ]; then
  echo 'public-audit: no pattern findings; review binary files and unknown credential formats separately'
fi
exit "$found"
