---
name: codeql-alerts
description: Work through the open CodeQL code scanning alerts on main one alert and one pull request at a time. Read the alerts through the GitHub API, verify the top one, fix it in a draft PR, run pr-review-loop, and start the next alert only after the maintainer merges the PR. Run on request or from the weekly routine.
disable-model-invocation: true
argument-hint: "[max alerts, default 3] | alert=<number to work first> | pr=<number of the PR to resume>"
---

# CodeQL alerts

CodeQL runs through GitHub's default code scanning setup: it analyses `main` on every push and
weekly, and every pull request. This skill turns the open alerts on `main` into fixes. Like
`backlog-loop`, it handles one alert and one pull request at a time; the next alert starts only
after the maintainer has merged the previous PR. Claude never merges, and never dismisses an
alert: dismissing is the maintainer's call.

Arguments: $ARGUMENTS. A number is the most alerts to fix in this run (default 3). `alert=<n>`
works that alert first. `pr=<n>` resumes a run at its current PR: go to step 6.

## 1. Read the alerts

```sh
repo=$(git remote get-url origin | sed -E 's#\.git$##; s#^.*[:/]([^/:]+/[^/]+)$#\1#')
auth=()
[ -n "${NGR_CODE_SCANNING_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $NGR_CODE_SCANNING_TOKEN")
curl -sS "${auth[@]}" -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$repo/code-scanning/alerts?state=open&ref=refs/heads/main&tool_name=CodeQL&per_page=100"
```

Check that `repo` came out as `owner/name` before using it. The Claude GitHub App cannot read
code scanning alerts, so the request needs a fine-grained personal access token limited to this
repository with the "Code scanning alerts: Read-only" permission, stored in the cloud
environment as `NGR_CODE_SCANNING_TOKEN` (or as a network secret for `api.github.com`). Never
print the token or write it to a file.

- `403` or `401`: the token is missing, expired or lacks the permission. Tell the maintainer
  that in one message and stop.
- `404` with "no analysis found": code scanning has not analysed `main` yet. Report and stop.
- An empty list: report "no open CodeQL alerts" and stop.

From each alert keep only `number`, `rule.id`, `rule.security_severity_level`,
`rule.severity`, `most_recent_instance.location` (path and lines) and
`most_recent_instance.message.text`. Do not keep the raw response in the conversation.

## 2. Check before each alert

Stop and go to "Report" if any of these holds:

- this run has fixed as many alerts as its maximum;
- an open PR whose head branch is in this repository has a title starting with `[codeql]` or
  `[backlog-loop]`, or carries the `codeql` or `backlog-loop` label. Tasks run one at a time,
  across both loops;
- the latest CI run on `main` failed (a red `main` is P0 work, outside this loop);
- this run's previous PR was closed without being merged.

Then reset the branch's content to `origin/main` exactly as `backlog-loop` step 1 describes,
including its checks that nothing else needs the branch.

## 3. Pick

Leave out the alerts on this run's skipped list. Order the rest by
`rule.security_severity_level` (critical, high, medium, low, none), then `rule.severity`
(error, warning, note), then by number, oldest first, and take the first. `alert=<n>` goes
first if it is still open.

## 4. Verify

Read the code at the alert's location on `main` and the rule's help (`rule.id`; the API's
`GET .../code-scanning/alerts/<n>` returns `rule.help`). Decide which case holds:

- **Real.** The flagged path can be reached with the input or state the rule describes. Go to
  step 5.
- **False positive or not worth fixing.** The path cannot be reached, the input is trusted by
  design (cite the section of `docs/system-architecture.md` that says so), or the code is test
  or example code where the issue cannot matter. Put the alert on the skipped list with a
  one-line reason and the dismissal reason you recommend (false positive, used in tests, won't
  fix). Go to step 2.
- **Generated code** (`crates/vci-service-interface` proto bindings, `crates/*-sys/src/bindings/`).
  Never hand-edit it. Skip the alert with the generator input the fix would belong in, or with
  "won't fix" when there is none.
- **Security severity high or critical.** The repository is public, so a PR would disclose the
  weakness before the fix ships. Do not open a PR: skip the alert with "needs the maintainer's
  decision on a private fix" and go to step 2.

When the case is not clear, follow `unattended-clarification` and skip the alert; do not guess.

## 5. Fix and open the pull request

Fix the cause with the smallest change that removes the flagged behaviour, following
CLAUDE.md and `.claude/README.md` (task size, agents, documentation sync, ADRs). Add a test that
fails without the fix where the behaviour can be tested. Never silence the alert instead: no
suppression comments, no query or path exclusions, and no weakened check.

Open a draft PR from the template. The title starts with `[codeql]` and names the rule and the
place, e.g. `[codeql] rust/cleartext-logging: redact the VIN in agent logs`; add the `codeql`
label (create it if the repository does not have it). The description names the alert number
and rule id and says how the fix removes the flagged path. Describe the weakness in terms of
the code, without a ready-made exploit.

Then run `pr-review-loop`. Before it hands the PR to the maintainer, also check the `CodeQL`
check run on the PR's head (from the `github-advanced-security` app): it must have concluded
`success` and report no new alerts. If it reports new alerts, they are findings of this round.

## 6. Wait for the merge, then continue

End the turn. While the PR is open, any wake other than its merge or close is handled by
`pr-review-loop`.

When the PR is merged, wait for CodeQL to analyse the new `main` (the `Analyze` check runs on
the merge commit), then read the alert again: it should now be `fixed`. If it is still open,
report that and stop rather than retrying. Otherwise go to step 2. When the PR is closed
without merging, go to step 2, which stops the run.

A run keeps its state (alerts fixed, skipped list with reasons) in a "CodeQL run" section of
its current PR's description, so `pr=<n>` can resume it in a fresh session.

## Report

One message to the maintainer:

- the PRs opened or merged in this run, each with its alert number and rule;
- each skipped alert with its reason. For an alert you recommend dismissing, write it as a
  question answerable in a word, with the recommended dismissal reason; the maintainer
  dismisses it in the repository's Security tab;
- the number of open alerts left;
- why the run stopped.

When nothing changed since the previous report (no alerts, or the same skipped alerts and no
PR), say so in one line.
