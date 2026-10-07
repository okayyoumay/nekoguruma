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
works that alert first. `pr=<n>` resumes a run at its current PR (see "Run state").

## 1. Read the alerts

Run `scripts/codeql-alerts.sh list`. It prints the open CodeQL alerts on `main` as a table
(number, security severity, severity, rule, location), most urgent first, and never the raw API
response. `scripts/codeql-alerts.sh show <n>` prints one alert's state, location, message and
the rule's help. Read the list again at the start of every iteration; a merge can fix or add
alerts.

The Claude GitHub App cannot read code scanning alerts, so the script sends the fine-grained
personal access token in `NGR_CODE_SCANNING_TOKEN` when it is set: limited to this repository,
with only the "Code scanning alerts: Read-only" permission, stored in the cloud environment.
Never print the token or write it to a file. Exit statuses:

- 3 (401 or 403): the token is missing, expired or lacks the permission. Tell the maintainer
  that in one message and stop.
- 4 (404 from `list`): code scanning has not analysed `main` yet. Report and stop.
- 5: a rate limit, or another API or network error. Report the message it printed and stop.
- 6 (404 from `show`): no alert has that number. Report the number and stop.
- 0 with no rows: report "no open CodeQL alerts" and stop.

## 2. Check before each alert

Stop and go to "Report" if any of these holds:

- this run has fixed as many alerts as its maximum;
- every open alert in the list is on this run's skipped list;
- an open PR whose head branch is in this repository has a title starting with `[codeql]` or
  `[backlog-loop]`, or carries the `codeql` or `backlog-loop` label. Tasks run one at a time,
  across both loops;
- the latest CI run on `main` failed (a red `main` is P0 work, outside this loop);
- this run's previous PR was closed without being merged.

Then reset the branch's content to `origin/main` exactly as `backlog-loop` step 1 describes,
including its checks that nothing else needs the branch.

## 3. Pick

Take the first row of the list that is not on this run's skipped list; the script already
orders the rows by security severity, then severity, then creation time, oldest first.
`alert=<n>` goes first if it is still open.

## 4. Verify

Read the alert with `scripts/codeql-alerts.sh show <n>`, then the code at its location on
`main`. Decide which case holds:

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

When the PR is merged, record it in the run state, wait for CodeQL to analyse the new `main`
(the `Analyze` checks on the merge commit), then run `scripts/codeql-alerts.sh show <n>`: the
alert should now be `fixed`. If it is still open, report that and stop rather than retrying.
Otherwise go to step 1. When the PR is closed without merging, go to step 2, which stops the
run.

## Run state

The run's state is the run's maximum, the alerts fixed (with their PRs) and the skipped list
with reasons. Keep it in a "CodeQL run" section of the current PR's description and update it
whenever it changes; a merged PR's description can still be edited. To resume with `pr=<n>`,
read the state from that PR and subscribe to its activity, then go on by its state: open, run
`pr-review-loop` on it (step 5's last paragraph still applies); merged, step 6's merge handling
unless the state already records the merge, then step 1; closed without merging, "Report".

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
