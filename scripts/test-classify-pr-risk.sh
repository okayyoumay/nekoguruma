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
other="$w/other-backlog.md"

base_backlog() {
  cat <<'EOF'
> **TEMPORARY WORKING MATERIAL.** Scratch backlog for the classifier test.

# Backlog

## Status

- `crate-a`: a status bullet, not an item

## Area one

- **P1**: First item. Done when: one.
- **P2**: Second item. Done when: two.

```text
## Resolved
- **P1**: Not an item, inside a fence.
```

  ~~~~
- **P1**: Not an item, inside an indented tilde fence.
  ```
- **P1**: Not an item, a shorter backtick run does not close it.
  ~~~~

````md
- **P1**: Not an item, inside a four-backtick fence.
```
````

## Area two

- **P1**: Third item. Done when: three.

### Known Flaky Tests

- **`a_flaky_test`: fails one run in ten.**
  Confirm with a loop in isolation.
  Cause unknown.

#### Resolved

- **`an_old_flaky_test`**: fixed, kept as history.

## Area three

- **P2**: Fifth item. Done when: five.

  A second paragraph of the fifth item.
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
  printf '> **TEMPORARY WORKING MATERIAL.** Second backlog.\n\n## Area four\n\n- **P1**: Fourth item.\n' >"$other"
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
edit '/an_old_flaky_test/d'
expect "deleting a nested Resolved entry is HIGH" HIGH "not a whole item: - **\`an_old_flaky_test"

setup
edit '/Second item/i ### Known Flaky Tests (moved here)\n'
expect "a heading inserted above existing items is HIGH" HIGH "moves items or sections"

setup
edit '/Second item/d'
printf -- '- **P2**: Second item. Done when: two.\n' >>"$other"
expect "moving an item to another backlog file is HIGH" HIGH "moves a backlog item to another backlog file"

setup
printf 'More rules.\n' >>"$w/README.md"
expect "editing work/README.md is HIGH" HIGH "not on the low-risk allowlist"

setup
printf '> **TEMPORARY WORKING MATERIAL.** New.\n\n## A\n\n- **P1**: x.\n' >"$w/new-backlog.md"
expect "adding a backlog file is HIGH" HIGH "adds the backlog file"

setup
edit '/^# Backlog$/i - **P1**: An item before any heading.\n'
expect "adding a priority bullet before the first heading is HIGH" HIGH "neither an item nor a heading"

setup
edit '/^## Status$/i - **P1**: An item under the title, outside every area.\n'
expect "adding a priority bullet under the title is HIGH" HIGH "neither an item nor a heading"

setup
edit '/^## Area two$/i ## Area new\n\n- **P2**: New item.\n'
expect "adding a section in the middle of a file is LOW" LOW

setup
edit '/^## Area two$/i ## Area zero\n\n### Known Flaky Tests\n'
expect "adding a section with a repeated subsection heading is LOW" LOW

setup
edit '/Fifth item/,$d'
expect "deleting a two-paragraph item is LOW" LOW

setup
edit '/^## Area three$/,$d'
expect "closing a section's last item and removing its heading is LOW" LOW

setup
edit '/^## Area two$/d'
expect "removing a heading whose section stays is HIGH" HIGH "not a whole item: ## Area two"

setup
edit '/First item/d'
sed -i '/Fourth item/d' "$other"
expect "deleting one item in each of two files is HIGH" HIGH "deletes 2 backlog items"

setup
git mv "$other" "$w/renamed-backlog.md"
expect "renaming a backlog file is HIGH" HIGH "renames"

setup
git rm -q "$other"
expect "deleting a backlog file is HIGH" HIGH "deletes $other"

setup
edit '/^## Status$/i ### Orphan subsection\n\n- **P1**: An item under a subsection with no area.\n'
expect "adding a priority bullet under a subsection with no area is HIGH" HIGH "neither an item nor a heading"

setup
printf '\n## Area five\n\n### Prioritized Backlog\n\n- **P2**: Sixth item.\n' >>"$other"
printf '\n## Area six\n\n### Prioritized Backlog\n\n- **P2**: Seventh item.\n' >>"$other"
git add -A
git commit -q -m "repeated subsections"
git branch -f base
sed -i '/^## Area five$/,/Sixth item/d' "$other"
expect "closing the last item under a repeated subsection heading is LOW" LOW

setup
chmod +x "$backlog"
expect "changing only a backlog file's mode is HIGH" HIGH "changes the file mode"

setup
sed -i 's/$/\r/' "$backlog"
git add -A
git commit -q -m "crlf"
git branch -f base
edit '/First item/d'
expect "deleting one item from a CRLF file is LOW" LOW

setup
edit '/Not an item, inside a fence/d'
expect "deleting a bullet inside a code fence is HIGH" HIGH "not a whole item"

setup
edit '/inside an indented tilde fence/d'
expect "deleting a bullet inside an indented tilde fence is HIGH" HIGH "not a whole item"

setup
edit '/a shorter backtick run does not close it/d'
expect "deleting a bullet after a non-matching fence line is HIGH" HIGH "not a whole item"

setup
edit '/inside a four-backtick fence/d'
expect "deleting a bullet inside a four-backtick fence is HIGH" HIGH "not a whole item"

setup
edit '/Third item/d'
expect "deleting an item after a fence holding a Resolved heading is LOW" LOW

# A comparison step that fails must not read as "no change": with awk
# failing for the comparison (the only awk call with -F), two deleted items
# must give exit status 2, not LOW.
setup
edit '/First item/d;/Third item/d'
git add -A
git commit -q -m "awk fails"
mkdir -p "$tmp/shim"
real_awk="$(command -v awk)"
printf '#!/bin/sh\ncase "$1" in -F*) exit 2 ;; esac\nexec %s "$@"\n' "$real_awk" >"$tmp/shim/awk"
chmod +x "$tmp/shim/awk"
status=0
PATH="$tmp/shim:$PATH" scripts/classify-pr-risk.sh base >/dev/null 2>&1 || status=$?
if [ "$status" -eq 2 ]; then
  echo "ok: a failing comparison exits with status 2"
else
  echo "FAIL: a failing comparison exits with status 2: got status $status" >&2
  failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
  echo "error: $failures classify-pr-risk.sh test case(s) failed" >&2
  exit 1
fi
