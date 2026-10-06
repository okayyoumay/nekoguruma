#!/usr/bin/env bash
# Classifies the changes on the current branch as LOW or HIGH risk, from the
# changed paths and the diff size only (no model judgment), for the
# backlog-loop skill. The verdict is recorded on the pull request; it does not
# merge anything.
#
# LOW means every changed file is on the allowlist below, i.e. the pull
# request's own CI covers everything the change can affect; for tests that
# means new files that Cargo itself lists as integration-test targets
# (`cargo metadata`, so cargo and jq must be installed). Anything else is
# HIGH: when in doubt, the answer is HIGH.
#
# Usage: scripts/classify-pr-risk.sh [base-ref]   (default: origin/main)
# Prints the verdict on the first line, then one "- reason" line per rule
# that made it HIGH (or one line saying why it is LOW). Exit status is 0 for
# both verdicts; 2 means the diff or the test targets could not be computed
# (no merge base, a dirty working tree, or cargo metadata failing).
set -euo pipefail

cd "$(dirname "$0")/.."

base="${1:-origin/main}"
max_files=20
max_lines=800

if ! merge_base="$(git merge-base "$base" HEAD 2>/dev/null)"; then
  echo "cannot find a merge base with $base" >&2
  exit 2
fi

# cargo metadata reads the working tree, so it must match HEAD, the revision
# CI checks out and the diff below classifies.
if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree has uncommitted or untracked changes; commit or remove them first" >&2
  exit 2
fi

# Integration-test targets Cargo discovers on HEAD, as repository-relative
# paths. Asking Cargo avoids re-implementing its discovery rules (helper
# modules, dot-prefixed names, crates outside the workspace).
if ! test_targets="$(cargo metadata --no-deps --format-version 1 --offline 2>/dev/null |
  jq -r '.workspace_root as $r | .packages[].targets[]
    | select(.kind | index("test")) | .src_path | ltrimstr($r + "/")')"; then
  echo "cannot list the workspace's test targets (cargo metadata or jq failed)" >&2
  exit 2
fi

# Plain, unquoted diff output whatever the user's git configuration says.
gitd() { git -c core.quotePath=false diff --no-color --no-ext-diff "$@"; }


# Paths whose changes the pull request's own CI fully covers.
is_low_path() {
  case "$1" in
    work/*) return 0 ;;
    crates/*-sys/*) return 1 ;;
    # CI excludes sim-vci from the test runs (CLAUDE.md, "Building and testing").
    crates/sim-vci/*) return 1 ;;
    docs/glossary.md) return 0 ;;
  esac
  is_crate_test "$1"
}

# A file that is itself an integration-test target (see test_targets above).
is_crate_test() {
  grep -Fxq -- "$1" <<<"$test_targets"
}

reasons=()
files=0
lines=0

while IFS=$'\t' read -r status path rest; do
  [ -n "$status" ] || continue
  files=$((files + 1))
  case "$status" in
    A* | M*) ;;
    D*) reasons+=("deletes $path") ;;
    R*) reasons+=("renames $path to $rest") ;;
    *) reasons+=("changes $path with git status $status (copy, type change or other)") ;;
  esac
  target="${rest:-$path}"
  if ! is_low_path "$target"; then
    reasons+=("changes $target, which is not on the low-risk allowlist")
  elif [ "${status:0:1}" != A ]; then
    # Editing an existing test can drop or disable it (removed lines,
    # #[ignore], #[cfg(any())], a loosened assertion) without CI noticing;
    # only new test files are low risk.
    if is_crate_test "$target"; then
      reasons+=("modifies the existing test file $target")
    fi
  fi
done < <(gitd --name-status "$merge_base" HEAD)

while IFS=$'\t' read -r added removed path; do
  if [ "$added" = "-" ]; then
    reasons+=("changes the binary file $path")
    continue
  fi
  lines=$((lines + added + removed))
done < <(gitd --numstat "$merge_base" HEAD)

if [ "$files" -gt "$max_files" ]; then
  reasons+=("changes $files files (limit $max_files)")
fi
if [ "$lines" -gt "$max_lines" ]; then
  reasons+=("changes $lines lines (limit $max_lines)")
fi
# An added attribute that mentions ignore or cfg can switch a test off
# (#[ignore], #[cfg_attr(..., ignore)], #![cfg(any())], #[cfg(windows)]).
# grep -c reads the whole diff; grep -q could exit early and fail git diff
# with SIGPIPE under pipefail, hiding the match.
gated="$(gitd "$merge_base" HEAD -- '*.rs' | grep -cE '^\+.*#!?\[.*(ignore|cfg)' || true)"
if [ "${gated:-0}" -gt 0 ]; then
  reasons+=("adds an attribute with ignore or cfg to Rust code")
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
