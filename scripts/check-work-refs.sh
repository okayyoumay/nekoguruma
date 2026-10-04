#!/usr/bin/env bash
# Checks that permanent files do not reference files inside work/.
#
# work/ holds temporary working material that is deleted once done, so a
# permanent file that names a file there (e.g. "work/<file>.md") goes stale.
# Naming the folder itself ("work/") or its own README ("work/README.md") is
# allowed, because those describe the convention rather than depend on content.
#
# Usage:
#   scripts/check-work-refs.sh            check every tracked file outside work/
#   scripts/check-work-refs.sh FILE...    check only the given files
#   scripts/check-work-refs.sh --hook     Claude Code PostToolUse hook mode: read
#                                         the hook JSON from stdin and check the
#                                         edited file; exit 2 reports to Claude
set -euo pipefail

cd "$(dirname "$0")/.."

pattern='work/[A-Za-z0-9_-][A-Za-z0-9_.-]*[A-Za-z0-9_]'

is_permanent() {
  case "$1" in
    work/*|./work/*|target/*|.git/*|Cargo.lock) return 1 ;;
    *) return 0 ;;
  esac
}

# Prints offending lines of one file as path:line:text.
scan() {
  local f="$1"
  [ -f "$f" ] || return 0
  is_permanent "$f" || return 0
  grep -nIoE "$pattern" "$f" 2>/dev/null | grep -vE ':work/README(\.md)?$' | sed "s|^|$f:|" || true
}

if [ "${1:-}" = "--hook" ]; then
  input="$(cat)"
  if command -v jq >/dev/null 2>&1; then
    file="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')"
  else
    file="$(printf '%s' "$input" | grep -oE '"file_path"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n1 | sed -E 's/.*"([^"]*)"$/\1/')"
  fi
  [ -n "$file" ] || exit 0
  root="$(pwd)"
  file="${file#"$root"/}"
  # Files outside the repository are not ours to check.
  case "$file" in /*) exit 0 ;; esac
  hits="$(scan "$file")"
  if [ -n "$hits" ]; then
    {
      echo "Permanent file references a file inside work/ (temporary material; see work/README.md):"
      echo "$hits"
      echo "Move the lasting content into a permanent document, or describe it without naming the work/ file."
    } >&2
    exit 2
  fi
  exit 0
fi

if [ "$#" -gt 0 ]; then
  files=("$@")
else
  mapfile -t files < <(git ls-files)
fi

status=0
for f in "${files[@]}"; do
  hits="$(scan "$f")"
  if [ -n "$hits" ]; then
    echo "$hits"
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo "error: permanent files must not reference files inside work/ (see work/README.md)" >&2
fi
exit "$status"
