# ADR-283: CodeQL Runs as a Repository Workflow, and the Windows DLL Check Uses the Runner's System Directory

**Date:** 2026-10-13
**Status:** Accepted
**Affects:** `.github/workflows/codeql.yml`, `.github/workflows/codeql-alert-handoff.yml`, `.github/workflows/ci.yml` (`worker-windows`, `abi-roundtrip-windows`), `.claude/skills/backlog-loop/SKILL.md`, ADR-243 Decision item 4, ADR-279 Decision item 4

## Context

A review of the Actions run times from 2026-10-08 to 2026-10-13 found two costs that the CI
layout itself caused.

CodeQL ran as GitHub's default setup, which analyses every commit pushed to a pull request
whatever it changes. About 30% of those analyses (about six minutes each) were of pull requests
whose whole diff touched only documentation, `work/` or `.claude/`, the pull requests whose build
jobs `ci.yml` already skips. Default setup has no path filter.

The release builds of the Windows workers were checked for the DLLs they import by matching
each name against a fixed list of Windows system DLLs, in `worker-windows`, which runs on `main`
only. In those five days the check turned `main` red three times, each time because the code had
started to use a further system DLL (`combase.dll`, then two more) that the list lacked. None was
a real problem, and pull requests could not show it, since the check never runs there. The check
exists to catch a DLL that only the llvm-mingw toolchain ships (such as `libunwind.dll` or
`libc++.dll`), which a Windows machine would not have (ADR-227).

## Decision

1. **CodeQL runs from `.github/workflows/codeql.yml`** instead of default setup, which the
   maintainer disables in the repository's code security settings (GitHub refuses a workflow's
   results while default setup is on). The workflow keeps what default setup did: the name
   `CodeQL`, the `actions` and `rust` languages with no build, the default query suite, one
   analysis per language with the category `/language:<language>` (so existing alerts carry
   over), pushes to `main`, pull requests and a weekly run. It leaves out pull requests whose
   whole diff against the base touches only Markdown files, `docs/`, `work/` or `.claude/` (as
   `ci.yml` judges them), and pushes to `main` that touch only those paths. A documentation-only
   commit added to a pull request that also changes code is still analysed.
   CodeQL is not a required check, so a pull request may skip the whole workflow.
2. **The hand-off follows the workflow.** `codeql-alert-handoff.yml` still runs when a workflow
   named `CodeQL` finishes on `main`; ADR-243 Decision item 4's trigger now refers to this
   workflow rather than to default setup. Its other rules are unchanged.
3. **The DLL check asks the Windows it runs on.** `worker-windows` uploads the release
   executables with the debug builds it already hands to `abi-roundtrip-windows`, and that job,
   on a Windows runner, accepts an imported DLL only when it is an API set name (`api-ms-*`,
   `ext-ms-*`) or a file in the system directory for the binary's bitness (`System32` for x64,
   `SysWOW64` for x86). There is no list of names to maintain. The check stays on `main` only,
   so it adds no minutes to pull requests. The backlog loop's check of a Windows dependency
   update (ADR-279 Decision item 4) accordingly looks for a DLL Windows lacks rather than for
   names missing from a list.

## Consequences

- Documentation-only pull requests no longer start CodeQL, which would have saved about 130
  runner minutes a day in the period measured. A change to Markdown alone inside a crate is not analysed
  either; CodeQL does not analyse Markdown, so nothing is lost.
- The repository now owns the CodeQL configuration: a new language, query suite or schedule is a
  change to `codeql.yml`, and the action version is kept current by Dependabot's Actions group.
- The runner's Windows is Windows Server, which carries a few DLLs a desktop Windows lacks. A
  binary that started to import such a DLL would pass the check. That is unlikely for the APIs
  the workers use, and the check still catches the toolchain DLLs it exists for.
- A system DLL that the code newly uses no longer turns `main` red.
