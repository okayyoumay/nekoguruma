#!/usr/bin/env bash
# Tests scripts/classify-pr-risk.sh's rules for work/: each case commits an
# edit to a backlog file (or another file in work/) in a scratch repository
# holding a copy of the script and a one-crate workspace, and checks the
# verdict and, for HIGH, a reason line. Needs git, cargo and jq, as the
# classifier does.
#
# Usage: scripts/test-classify-pr-risk.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/classify-pr-risk.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The backlog file's path is built from parts so that this permanent file
# does not name a file in work/ (scripts/check-work-refs.sh).
w=work
backlog="$w/backlog.md"

base_backlog() {
  cat <<'EOF'
> **TEMPORARY WORKING MATERIAL.** Scratch backlog for the classifier test.

## Status

- `crate-a`: a status bullet, not an item

## Area one

- **P1**: First item. Done when: one.
- **P2**: Second item. Done when: two.

## Area two

- **P1**: Third item. Done when: three.

### Known Flaky Tests

- **`a_flaky_test`: fails one run in ten.**
  Confirm with a loop in isolation.
  Cause unknown.
EOF
}

setup() {
  rm -rf "$tmp/repo"
  mkdir -p "$tmp/repo/scripts" "$tmp/repo/$w" "$tmp/repo/src"
  cd "$tmp/repo"
  git init -q -b main
  git config user.email test@example.invalid
  git config user.name test
  cp "$script" scripts/
  printf '[package]\nname = "scratch"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
  : >src/lib.rs
  printf 'target/\nCargo.lock\n' >.gitignore
  base_backlog >"$backlog"
  printf '> **TEMPORARY WORKING MATERIAL.** Rules.\n' >"$w/README.md"
  git add -A
  git commit -q -m base
  git branch base
  git checkout -q -b change
}

failures=0

# expect VERDICT [REASON-SUBSTRING]: runs the classifier against the base
# branch after the case has committed its edit.
expect() {
  local name="$1" want="$2" reason="${3:-}" out
  git add -A
  git commit -q -m "$name"
  out="$(scripts/classify-pr-risk.sh base)"
  if [ "$(head -n1 <<<"$out")" != "$want" ] ||
    { [ -n "$reason" ] && ! grep -qF -- "$reason" <<<"$out"; }; then
    echo "FAIL: $name: expected $want${reason:+ with \"$reason\"}, got:" >&2
    sed 's/^/  /' <<<"$out" >&2
    failures=$((failures + 1))
  else
    echo "ok: $name"
  fi
}

# Edits the backlog file with a sed script.
edit() { sed -i "$1" "$backlog"; }

setup
edit '/First item/d'
expect "deleting one item is LOW" LOW

setup
edit '/First item/d'
edit '/Third item/a - **P2**: A residual. Done when: four.'
edit '$a\\n## Area three\n\n- **P3**: New item. Done when: five.'
expect "deleting one item and adding items and a section is LOW" LOW

setup
edit '/a_flaky_test/,/Cause unknown/d'
expect "deleting a whole flaky-test entry is LOW" LOW

setup
edit '/First item/d;/Third item/d'
expect "deleting two items is HIGH" HIGH "deletes 2 backlog items"

setup
edit '/First item/d;/Cause unknown/d'
expect "deleting part of a flaky-test entry as well is HIGH" HIGH "deletes 2 backlog items"

setup
edit 's/^## Area two$/## Area 2/'
expect "renaming a section is HIGH" HIGH "not a whole item: ## Area two"

setup
edit 's/status bullet/changed status bullet/'
expect "changing a Status bullet is HIGH" HIGH "not a whole item"

setup
edit '/^## Area one/a The area holds two items.'
expect "adding prose is HIGH" HIGH "neither an item nor a heading"

setup
edit '/Second item/d'
edit '/Third item/a - **P2**: Second item. Done when: two.'
expect "moving an item to another section is HIGH" HIGH "moves items or sections"

setup
printf 'More rules.\n' >>"$w/README.md"
expect "editing work/README.md is HIGH" HIGH "not on the low-risk allowlist"

setup
printf '> **TEMPORARY WORKING MATERIAL.** New.\n\n## A\n\n- **P1**: x.\n' >"$w/other-backlog.md"
expect "adding a backlog file is HIGH" HIGH "adds the backlog file"

if [ "$failures" -ne 0 ]; then
  echo "error: $failures classify-pr-risk.sh test case(s) failed" >&2
  exit 1
fi
