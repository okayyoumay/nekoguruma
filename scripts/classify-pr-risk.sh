#!/usr/bin/env bash
# Classifies the changes on the current branch as LOW or HIGH risk, from the
# changed paths and the diff size only (no model judgment), for the
# backlog-loop skill. The verdict is recorded on the pull request; it does not
# merge anything.
#
# LOW means every changed file is on the allowlist below, i.e. the pull
# request's own CI covers everything the change can affect; for tests that
# means new test files only. Anything else is HIGH: when in doubt, the answer
# is HIGH.
#
# Usage: scripts/classify-pr-risk.sh [base-ref]   (default: origin/main)
# Prints the verdict on the first line, then one "- reason" line per rule
# that made it HIGH (or one line saying why it is LOW). Exit status is 0 for
# both verdicts; 2 means the diff could not be computed.
set -euo pipefail

cd "$(dirname "$0")/.."

base="${1:-origin/main}"
max_files=20
max_lines=800

if ! merge_base="$(git merge-base "$base" HEAD 2>/dev/null)"; then
  echo "cannot find a merge base with $base" >&2
  exit 2
fi

# Paths whose changes the pull request's own CI fully covers.
is_low_path() {
  case "$1" in
    work/*) return 0 ;;
    crates/*-sys/*) return 1 ;;
    crates/*/tests/*) return 0 ;;
    docs/glossary.md) return 0 ;;
    *) return 1 ;;
  esac
}

reasons=()
files=0
lines=0

while IFS=$'\t' read -r status path rest; do
  [ -n "$status" ] || continue
  files=$((files + 1))
  case "$status" in
    D*) reasons+=("deletes $path") ;;
    R*) reasons+=("renames $path to $rest") ;;
  esac
  target="${rest:-$path}"
  if ! is_low_path "$target"; then
    reasons+=("changes $target, which is not on the low-risk allowlist")
  elif [ "${status:0:1}" = M ]; then
    case "$target" in
      # Editing an existing test can drop or disable it (removed lines,
      # #[ignore], #[cfg(any())], a loosened assertion) without CI noticing;
      # only new test files are low risk.
      crates/*/tests/*) reasons+=("modifies the existing test file $target") ;;
    esac
  fi
done < <(git diff --name-status "$merge_base" HEAD)

while IFS=$'\t' read -r added removed _; do
  [ "$added" = "-" ] && continue
  lines=$((lines + added + removed))
done < <(git diff --numstat "$merge_base" HEAD)

if [ "$files" -gt "$max_files" ]; then
  reasons+=("changes $files files (limit $max_files)")
fi
if [ "$lines" -gt "$max_lines" ]; then
  reasons+=("changes $lines lines (limit $max_lines)")
fi
# grep -c reads the whole diff; grep -q could exit early and fail git diff
# with SIGPIPE under pipefail, hiding the match.
ignored="$(git diff "$merge_base" HEAD -- '*.rs' | grep -cE '^\+.*#\[ignore' || true)"
if [ "${ignored:-0}" -gt 0 ]; then
  reasons+=("adds #[ignore] to a test")
fi

if [ "$files" -eq 0 ]; then
  echo "HIGH"
  echo "- no changes against $base"
elif [ "${#reasons[@]}" -eq 0 ]; then
  echo "LOW"
  echo "- $files files, $lines lines, all on the low-risk allowlist (work/, new crate test files, glossary)"
else
  echo "HIGH"
  printf -- '- %s\n' "${reasons[@]}"
fi
