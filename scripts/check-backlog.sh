#!/usr/bin/env bash
# Checks the backlog files in work/ against the format in work/README.md:
# - every file in work/ starts with the "TEMPORARY WORKING MATERIAL" line;
# - in the backlog files (work/*backlog.md), every top-level bullet in an
#   item section starts with a priority, "- **P0**" to "- **P3**".
# Sections named Status, Unverified Assumptions, Known Flaky Tests or
# Resolved (and everything below them) are not item lists and are skipped.
set -euo pipefail

cd "$(dirname "$0")/.."

status=0
shopt -s nullglob
for f in work/*.md; do
  if ! head -n1 "$f" | grep -q '^> \*\*TEMPORARY WORKING MATERIAL\.\*\*'; then
    echo "$f:1: missing the TEMPORARY WORKING MATERIAL header line" >&2
    status=1
  fi
  case "${f##*/}" in *backlog.md) ;; *) continue ;; esac
  awk -v file="$f" '
    /^```/ { fence = !fence; next }
    fence { next }
    /^#+ / {
      level = index($0, " ") - 1
      if (exempt && level <= exempt_level) exempt = 0
      if (!exempt && $0 ~ /^#+ (Status|Unverified Assumptions|Known Flaky Tests|Resolved)/) {
        exempt = 1; exempt_level = level
      }
      insection = 1
      next
    }
    /^- / && insection && !exempt && $0 !~ /^- \*\*P[0-3]\*\*/ {
      printf "%s:%d: item without a priority prefix (- **P0** .. - **P3**): %s\n", file, NR, substr($0, 1, 80) > "/dev/stderr"
      bad = 1
    }
    END { exit bad }
  ' "$f" || status=1
done

if [ "$status" -ne 0 ]; then
  echo "error: backlog format check failed (see work/README.md, \"Backlog format\")" >&2
fi
exit "$status"
