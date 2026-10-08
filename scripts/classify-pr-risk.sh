#!/usr/bin/env bash
# Classifies the changes on the current branch as LOW or HIGH risk, from the
# changed paths and the diff size only (no model judgment), for the
# backlog-loop skill. The verdict is recorded on the pull request; it does not
# merge anything.
#
# LOW means every changed file is on the allowlist below, i.e. the pull
# request's own CI covers everything the change can affect; for tests that
# means new files that Cargo itself lists as integration-test targets
# (`cargo metadata`, so cargo and jq must be installed). Under work/ only
# edits to the existing backlog files that add items (and headings) and
# delete at most one item in total, the item the pull request claims, are
# low risk (see check_work_backlog). Anything else is HIGH: when in doubt,
# the answer is HIGH.
#
# Usage: scripts/classify-pr-risk.sh [--claimed-item LINE] [base-ref]
#   LINE is the first line of the backlog item the pull request works on,
#   exactly as it reads on the base (the "- **P1**: ..." line). A deleted
#   item is low risk only when it is that item; without the option every
#   deleted item makes the verdict HIGH. base-ref defaults to origin/main.
# Prints the verdict on the first line, then one "- reason" line per rule
# that made it HIGH (or one line saying why it is LOW). Exit status is 0 for
# both verdicts; 2 means the diff or the test targets could not be computed
# (no merge base, a dirty working tree, or cargo metadata failing).
set -euo pipefail

cd "$(dirname "$0")/.."

claimed=""
claimed_given=0
while [ $# -gt 0 ]; do
  case "$1" in
    --claimed-item)
      [ $# -ge 2 ] || { echo "--claimed-item needs a value" >&2; exit 2; }
      claimed="$2"
      claimed_given=1
      shift 2
      ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
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
    # check_work_backlog below decides about the edits themselves.
    work/*backlog.md) return 0 ;;
    work/*) return 1 ;;
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

# Prints one backlog file as blocks, one per line: the type, a tab, the
# headings the block sits under (joined by "\036"), a tab, the text. An item
# (I) is a top-level "- **P0**" .. "- **P3**" bullet in an item section (a
# bullet with no "##" area heading above it, such as one under the file's
# "#" title or under a "###" heading placed directly below that title, is O),
# or
# any top-level bullet in a Known Flaky Tests section, with its indented
# continuation lines (also after a blank line) joined by "\037". Headings are
# H, blank lines B, everything else (the header line, prose, Status bullets,
# fenced code) O. As in scripts/check-backlog.sh, a Status, Unverified
# Assumptions or Resolved section and everything below it is not an item
# list, also when it is nested in a Known Flaky Tests section. Carriage
# returns are dropped.
backlog_blocks() {
  awk '
    # Tabs separate the fields, so a tab inside a heading or a line is
    # written as "\035".
    function out(t, p, x) { gsub(/\t/, "\035", p); gsub(/\t/, "\035", x); print t "\t" p "\t" x }
    function flush() { if (item != "") { out("I", path, item); item = "" } }
    { sub(/\r$/, "") }
    # A fence opens with three or more backticks or tildes after up to three
    # spaces, and closes with at least as many of the same character.
    {
      line = $0; ind = 0
      while (ind < 4 && substr(line, 1, 1) == " ") { line = substr(line, 2); ind++ }
      run = ""
      if (ind < 4 && match(line, /^(```+|~~~+)/)) run = substr(line, 1, RLENGTH)
    }
    function closes(f) {
      return run != "" && substr(run, 1, 1) == substr(f, 1, 1) &&
        length(run) >= length(f) && substr(line, length(run) + 1) ~ /^[ \t]*$/
    }
    # An indented fence inside an item belongs to the item up to its close.
    item != "" && ifence != "" {
      if (closes(ifence)) ifence = ""
      item = item gap "\037" $0; gap = ""; next
    }
    item != "" && ind > 0 && run != "" { ifence = run; item = item gap "\037" $0; gap = ""; next }
    fence != "" {
      if (closes(fence)) fence = ""
      out("O", path, $0); next
    }
    run != "" { flush(); fence = run; out("O", path, $0); next }
    /^#+ / {
      flush()
      level = index($0, " ") - 1
      while (depth && lvl[depth] >= level) depth--
      path = ""
      for (d = 1; d <= depth; d++) path = path (d > 1 ? "\036" : "") head[d]
      out("H", path, $0)
      lvl[++depth] = level; head[depth] = $0
      if ($0 ~ /^#+ (Status|Unverified Assumptions|Resolved)/) kind[depth] = "exempt"
      else if ($0 ~ /^#+ Known Flaky Tests/) kind[depth] = "flaky"
      else kind[depth] = ""
      sect = ""; area = 0
      for (d = 1; d <= depth; d++) {
        if (lvl[d] == 2) area = 1
        if (kind[d] == "exempt") sect = "exempt"
        else if (kind[d] == "flaky" && sect == "") sect = "flaky"
      }
      path = path (depth > 1 ? "\036" : "") $0
      next
    }
    /^[ \t]*$/ { if (item != "") gap = gap "\037"; else out("B", path, ""); next }
    /^[ \t]+[^ \t]/ && item != "" { item = item gap "\037" $0; gap = ""; next }
    { flush(); gap = "" }
    /^- / && area && ((sect == "" && $0 ~ /^- \*\*P[0-3]\*\*/) || sect == "flaky") { item = $0; next }
    { out("O", path, $0) }
    END { flush(); print "E\t\t" }
  '
}

# Compares a backlog file's blocks before (first file) and after (second
# file); blank lines are ignored. Prints one "deleted <item>" line for every
# whole item removed and one "added <item>" line for every item added, then
# one "reason <text>" line for every other kind of change:
# - a removed or changed line that is not a whole item, except a heading
#   whose whole section is removed along with it (closing the last item);
# - an added line that is neither an item nor a heading;
# - blocks that kept their text but changed their order or the headings they
#   sit under (an item moved to another section, or a heading inserted above
#   existing items).
# The last line is "end"; the caller treats its absence as a failure. An
# item rewritten in place shows as one deleted and one added item, as
# closing an item and adding its residual does; the limit of one deletion
# bounds that case.
compare_blocks() {
  awk -F'\t' '
    FNR == 1 { pass++ }
    $1 == "B" { next }
    $1 == "E" { ends++; next }
    { k = $1 "\t" $3 }
    pass == 1 { old[++no] = $0; okey[no] = k; cold[k]++; fold[$0]++; next }
    { new[++nn] = $0; nkey[nn] = k; cnew[k]++; fnew[$0]++ }
    function short(b) { b = substr(b, 3); gsub(/\037/, " ", b); return substr(b, 1, 70) }
    function hlevel(b) { return index(substr(b, 3), " ") - 1 }
    # Counts are read through cnt() so that no missing element is created
    # or used uninitialized; awks differ there (gawk 5.2 can even crash).
    function cnt(arr, key) { return (key in arr) ? arr[key] + 0 : 0 }
    function min(x, y) { return x < y ? x : y }
    END {
      if (ends + 0 != 2) exit 1
      for (k in cold) if (cold[k] > cnt(cnew, k)) gone[k] = 1
      for (k in cold) {
        extra = cold[k] - cnt(cnew, k)
        if (extra <= 0) continue
        if (substr(k, 1, 1) == "I") { while (extra-- > 0) print "deleted " substr(k, 3); continue }
        if (substr(k, 1, 1) == "H") {
          # A removed heading is fine when everything in its section goes too.
          # Each removed occurrence is checked under its own parent headings,
          # since subsection headings such as Prioritized Backlog repeat.
          emptied = 1
          for (i = 1; i <= no; i++) if (okey[i] == k && cnt(fnew, old[i]) < fold[old[i]]) {
            L = hlevel(k)
            for (j = i + 1; j <= no; j++) {
              if (substr(okey[j], 1, 1) == "H" && hlevel(okey[j]) <= L) break
              if (!(okey[j] in gone) || substr(okey[j], 1, 1) == "O") emptied = 0
            }
          }
          if (emptied) continue
        }
        print "reason removes or changes a line that is not a whole item: " short(k)
      }
      for (k in cnew) {
        extra = cnew[k] - cnt(cold, k)
        if (extra <= 0) continue
        t = substr(k, 1, 1)
        if (t == "I") while (extra-- > 0) print "added " substr(k, 3)
        else if (t != "H") print "reason adds a line that is neither an item nor a heading: " short(k)
      }
      # A block both versions keep must stay under the same headings ...
      for (f in fold) {
        k = substr(f, 1, 1) "\t" substr(f, index(substr(f, 3), "\t") + 3)
        kept[k] = cnt(kept, k) + min(fold[f], cnt(fnew, f))
      }
      for (k in cold) if (cnt(cnew, k) && cnt(kept, k) < min(cold[k], cnt(cnew, k))) {
        print "reason moves items or sections: " short(k); moved = 1; break
      }
      # ... and in the same order.
      if (!moved) {
        for (i = 1; i <= no; i++) {
          c = cnt(keep, old[i]); keep[old[i]] = c + 1
          if (c < cnt(fnew, old[i])) a[++na] = i
        }
        for (i = 1; i <= nn; i++) {
          c = cnt(seen, new[i]); seen[new[i]] = c + 1
          if (c < cnt(fold, new[i])) b[++nb] = i
        }
        for (i = 1; i <= na; i++) if (old[a[i]] != new[b[i]]) {
          print "reason moves items or sections: " short(nkey[b[i]]); break
        }
      }
      print "end"
    }
  ' "$1" "$2"
}

# work/ edits: only modified backlog files are checked here; is_low_path and
# the status checks below already make any other work/ change HIGH. The
# deleted and added items of all files are collected, so that an item moved
# from one backlog file to another is not taken for a closed item. A step
# that fails stops the script with status 2 rather than reading as no
# change.
work_deleted=()
work_added=()
# Prints lf, crlf or mixed for the text on standard input.
eol_style() {
  awk '{ n++; if (/\r$/) c++ } END { print (c == 0 ? "lf" : c == n ? "crlf" : "mixed") }'
}
work_tmp="$(mktemp -d)"
trap 'rm -rf "$work_tmp"' EXIT
check_work_backlog() {
  local path="$1" out line
  # A change of line endings is hidden once carriage returns are dropped.
  if [ "$(git show "$merge_base:$path" | eol_style)" != "$(git show "HEAD:$path" | eol_style)" ]; then
    reasons+=("changes the line endings of $path")
  fi
  # A mode-only change (an executable bit) has no text to compare.
  if [ "$(git ls-tree "$merge_base" -- "$path" | cut -d' ' -f1)" != \
    "$(git ls-tree HEAD -- "$path" | cut -d' ' -f1)" ]; then
    reasons+=("changes the file mode of $path")
  fi
  if ! git show "$merge_base:$path" | backlog_blocks >"$work_tmp/before" ||
    ! git show "HEAD:$path" | backlog_blocks >"$work_tmp/after" ||
    ! out="$(compare_blocks "$work_tmp/before" "$work_tmp/after")" ||
    [ "${out##*$'\n'}" != end ]; then
    echo "cannot compare the backlog file $path" >&2
    exit 2
  fi
  while IFS= read -r line; do
    case "$line" in
      "deleted "*) work_deleted+=("${line#deleted }") ;;
      "added "*) work_added+=("${line#added }") ;;
      "reason "*) reasons+=("$path: ${line#reason }") ;;
    esac
  done <<<"$out"
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
  case "$status:$target" in
    M*:work/*backlog.md) check_work_backlog "$target" ;;
    A*:work/*backlog.md) reasons+=("adds the backlog file $target") ;;
  esac
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

if [ "${#work_deleted[@]}" -gt 1 ]; then
  reasons+=("deletes ${#work_deleted[@]} backlog items (limit 1: the item the pull request finished)")
fi
# The claimed item must be exactly one item of the backlog files on the base,
# so that a mistyped claim cannot match nothing and pass, and an ambiguous
# one cannot cover two items. Items are compared by their
# first line as backlog_blocks writes it (tabs as \035).
claimed_key="${claimed//$'\t'/$'\035'}"
if [ "$claimed_given" -eq 1 ]; then
  claimed_found=0
  while IFS= read -r f; do
    case "$f" in work/*backlog.md) ;; *) continue ;; esac
    if ! blocks="$(git show "$merge_base:$f" | backlog_blocks)"; then
      echo "cannot read the backlog file $f on $base" >&2
      exit 2
    fi
    while IFS=$'\t' read -r t _ text; do
      if [ "$t" = I ] && [ "${text%%$'\037'*}" = "$claimed_key" ]; then
        claimed_found=$((claimed_found + 1))
      fi
    done <<<"$blocks"
  done < <(git ls-tree -r --name-only "$merge_base" -- work/)
  if [ "$claimed_found" -eq 0 ]; then
    reasons+=("the claimed item is not an item of a backlog file on $base: ${claimed:0:70}")
  elif [ "$claimed_found" -gt 1 ]; then
    # Items are matched by their first line, so a claim that names several
    # items cannot tell which one the pull request may delete.
    reasons+=("the claimed item's first line matches $claimed_found items on $base: ${claimed:0:70}")
  fi
fi
for item in ${work_deleted[@]+"${work_deleted[@]}"}; do
  shown="${item//$'\037'/ }"
  if [ "$claimed_given" -eq 0 ]; then
    reasons+=("deletes a backlog item, but no claimed item was given (--claimed-item): ${shown:0:70}")
  elif [ "${item%%$'\037'*}" != "$claimed_key" ]; then
    reasons+=("deletes a backlog item other than the claimed one: ${shown:0:70}")
  fi
done
for item in ${work_deleted[@]+"${work_deleted[@]}"}; do
  for added_item in ${work_added[@]+"${work_added[@]}"}; do
    if [ "$item" = "$added_item" ]; then
      item="${item//$'\037'/ }"
      reasons+=("moves a backlog item to another backlog file: ${item:0:70}")
      break
    fi
  done
done
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
  echo "- $files files, $lines lines, all on the low-risk allowlist (work/ backlog items added or one deleted, new crate test files, glossary)"
else
  echo "HIGH"
  printf -- '- %s\n' "${reasons[@]}"
fi
