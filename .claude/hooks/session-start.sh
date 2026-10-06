#!/usr/bin/env bash
# SessionStart hook.
# - After compaction: re-inject the repository rules most easily lost from a
#   summarized context (stdout becomes context for this event).
# - In Claude Code cloud sessions (CLAUDE_CODE_REMOTE=true) on startup:
#   fetch crates and warm the check cache so builds and tests work offline
#   and start fast. The container state is cached after the hook completes.
set -euo pipefail

input="$(cat || true)"
source="$(printf '%s' "$input" | grep -oE '"source"[[:space:]]*:[[:space:]]*"[a-z]+"' | head -n1 | sed -E 's/.*"([a-z]+)"$/\1/')"

if [ "$source" = "compact" ]; then
  cat <<'MSG'
Reminder of repository rules (CLAUDE.md):
- Open items and TODO notes go in work/; permanent files never name files inside work/.
- Update the documents in CLAUDE.md's documentation-sync table in the same PR.
- Reserve an ADR number (adr-number-reservation skill) before writing it anywhere.
- Never copy standard text (ISO 22900, SAE J2534, ISO 14229 or any other) verbatim; cite the clause and paraphrase.
MSG
  exit 0
fi

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ] || [ "$source" != "startup" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}"

# Progress goes to stderr so it does not end up in Claude's context.
{
  rustup component add rustfmt clippy
  cargo fetch --locked
  cargo check --workspace --all-targets --locked
} >&2
