#!/usr/bin/env bash
# Checks docs/adr/: no duplicate ADR numbers, INDEX.md links every ADR file
# and nothing else, and its theme sections list one ADR per line in numeric
# order (one line per ADR keeps two pull requests that add to the
# same theme from editing the same line). A duplicate number never conflicts in git (the two
# files have different slugs), so this is the backstop for the number
# reservation procedure (.claude/skills/adr-number-reservation/SKILL.md).
set -euo pipefail

cd "$(dirname "$0")/.."

status=0
dups="$(
  ls docs/adr \
    | sed -n 's/^ADR-\([0-9][0-9]*\)-.*\.md$/\1/p' \
    | while IFS= read -r n; do echo "$((10#$n))"; done \
    | sort -n | uniq -d
)"
if [ -n "$dups" ]; then
  echo "duplicate ADR number(s) in docs/adr/:" $dups >&2
  status=1
fi
for f in docs/adr/ADR-*.md; do
  base="${f##*/}"
  if ! grep -qF "($base)" docs/adr/INDEX.md; then
    echo "docs/adr/INDEX.md has no row linking $base" >&2
    status=1
  fi
done
while IFS= read -r link; do
  if [ ! -f "docs/adr/$link" ]; then
    echo "docs/adr/INDEX.md links nonexistent file $link" >&2
    status=1
  fi
done < <(grep -o 'ADR-[0-9][0-9]*-[A-Za-z0-9-]*\.md' docs/adr/INDEX.md | sort -u)
theme_errors="$(
  awk '
    /^## Classification by Theme/ { in_themes = 1; next }
    !in_themes { next }
    /^## / { in_themes = 0; next }
    /^### / { theme = substr($0, 5); prev = 0; next }
    /^[[:space:]]*$/ { next }
    /^- ADR-[0-9][0-9][0-9]( \(.*\))?$/ {
      n = substr($2, 5, 3) + 0
      if (n <= prev) printf "theme \"%s\": ADR-%03d is not after ADR-%03d (keep numeric order, one entry per ADR)\n", theme, n, prev
      prev = n
      next
    }
    { printf "theme \"%s\": not one ADR per line: %s\n", theme, substr($0, 1, 80) }
  ' docs/adr/INDEX.md
)"
if [ -n "$theme_errors" ]; then
  sed 's|^|docs/adr/INDEX.md |' <<<"$theme_errors" >&2
  status=1
fi
exit "$status"
