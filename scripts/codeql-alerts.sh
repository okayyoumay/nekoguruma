#!/usr/bin/env bash
# Read CodeQL code scanning alerts for the codeql-alerts skill, printing only the fields it uses.
#
#   scripts/codeql-alerts.sh list        open CodeQL alerts on main, most urgent first (TSV)
#   scripts/codeql-alerts.sh show <n>    one alert: state, rule, location, message, rule help
#
# The Claude GitHub App cannot read code scanning alerts, so a read-only fine-grained token in
# NGR_CODE_SCANNING_TOKEN is sent when set. The raw API response is never printed.
#
# Exit status: 0 ok; 2 usage; 3 not authorized (401/403); 4 list: no analysis of main (404);
# 5 any other API or network error; 6 show: no such alert (404).
set -euo pipefail

usage() { echo "usage: $0 list | show <alert number>" >&2; exit 2; }

repo=$(git remote get-url origin | sed -E 's#\.git$##; s#^.*[:/]([^/:]+/[^/]+)$#\1#')
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "cannot derive owner/repo from origin" >&2; exit 5; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# get <path and query> <output file> <exit status for 404>: fetch one API page and map the HTTP
# status to an exit status. The token goes to curl on stdin (printf is a builtin), so it never
# appears in a process's command line.
get() {
  local -a cmd=(curl -sS -o "$2" -w '%{http_code}'
    -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28"
    "https://api.github.com/repos/$repo/$1")
  local status
  if [ -n "${NGR_CODE_SCANNING_TOKEN:-}" ]; then
    status=$(printf 'Authorization: Bearer %s\n' "$NGR_CODE_SCANNING_TOKEN" | "${cmd[@]}" -H @-) \
      || { echo "request failed" >&2; exit 5; }
  else
    status=$("${cmd[@]}") || { echo "request failed" >&2; exit 5; }
  fi
  case "$status" in
    200) ;;
    401|403) echo "HTTP $status: not authorized to read code scanning alerts (check NGR_CODE_SCANNING_TOKEN)" >&2; exit 3 ;;
    404) echo "HTTP 404: $(jq -r '.message // "not found"' "$2" 2>/dev/null)" >&2; exit "$3" ;;
    *) echo "HTTP $status: $(jq -r '.message // "unexpected response"' "$2" 2>/dev/null)" >&2; exit 5 ;;
  esac
}

case "${1:-}" in
  list)
    [ $# -eq 1 ] || usage
    page=1
    while :; do
      get "code-scanning/alerts?state=open&ref=refs/heads/main&tool_name=CodeQL&per_page=100&page=$page" "$tmp/page-$page.json" 4
      [ "$(jq length "$tmp/page-$page.json")" -lt 100 ] && break
      page=$((page + 1))
    done
    printf 'number\tsecurity_severity\tseverity\trule\tlocation\n'
    jq -s -r '
      add
      | sort_by(
          ({"critical": 0, "high": 1, "medium": 2, "low": 3}[.rule.security_severity_level // ""] // 4),
          ({"error": 0, "warning": 1, "note": 2}[.rule.severity // ""] // 3),
          .number)
      | .[]
      | [ .number,
          (.rule.security_severity_level // "-"),
          (.rule.severity // "-"),
          .rule.id,
          "\(.most_recent_instance.location.path):\(.most_recent_instance.location.start_line)" ]
      | @tsv' "$tmp"/page-*.json
    ;;
  show)
    [ $# -eq 2 ] && [[ "$2" =~ ^[0-9]+$ ]] || usage
    get "code-scanning/alerts/$2" "$tmp/alert.json" 6
    jq -r '
      "number: \(.number)",
      "state: \(.state)",
      "rule: \(.rule.id) (security severity \(.rule.security_severity_level // "-"), severity \(.rule.severity // "-"))",
      "location: \(.most_recent_instance.location.path):\(.most_recent_instance.location.start_line)-\(.most_recent_instance.location.end_line)",
      "message: \(.most_recent_instance.message.text)",
      "",
      "rule help:",
      (.rule.help // "(none)")' "$tmp/alert.json"
    ;;
  *) usage ;;
esac
