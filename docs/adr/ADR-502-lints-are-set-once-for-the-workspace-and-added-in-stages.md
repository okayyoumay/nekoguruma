# ADR-502: Lints Are Set Once for the Workspace and Added in Stages

**Date:** 2026-11-22
**Status:** Accepted
**Affects:** `Cargo.toml` (`[workspace.lints]`), every `crates/*/Cargo.toml` (`[lints]`), `CLAUDE.md` ("Rust code"), `.claude/agents/implementer.md`, `.claude/skills/pr-review-loop/SKILL.md`, `.github/copilot-instructions.md`, `.github/instructions/rust.instructions.md`, `docs/worker-crates.md`

## Context

Most of the code in this workspace is written by coding agents. Their typical Rust failures are
known: a `.clone()` added only to get past the borrow checker (an audit of `main` found the IR
VM copying a whole byte value on every load for that reason), a warning silenced with a bare
`#[allow]` instead of fixed, `unsafe` added outside the layers meant to hold it, tests that
restate the implementation, and documentation that narrates how a change was designed.

The repository already had rules for some of this (`#[expect]` instead of `#[allow]` for code
unused until later work lands; `unsafe` only in the FFI layers; panics reachable from external
input are a P1 review finding), but only as prose that a reviewer had to catch. There was no
lint configuration at all: each crate used the compiler's and Clippy's defaults, and the
crate-level `#![allow(unsafe_code)]` in the FFI crates had no effect, since nothing denied
`unsafe_code`. Clippy does not run in CI (it would add a build to every pull request, and the
maintainer's aim is fewer Actions minutes); it runs locally before a pull request.

External guidance points the same way. The Microsoft Pragmatic Rust Guidelines (version 2026.6)
recommend common settings in the workspace manifest (M-CARGO-WORKSPACE), a recommended set of
compiler and Clippy lints (M-STATIC-VERIFICATION), and `#[expect]` with a reason for every lint
override (M-LINT-OVERRIDE-EXPECT). Their AI chapter names the failure modes above
(M-TAUTOLOGICAL-TESTS, M-NO-META-DESIGN-DOCUMENTATION, M-SINGLE-ITEM-PATH). The rust-lang
project's own agent file keeps agent instructions a summary of rules written for people, not
their only source. Applying the whole recommended lint set (every major Clippy group plus the
listed restriction lints) to `main` gave 12,112 hits, most of them style or documentation lints
(`doc_markdown` alone gave 3,297, many on standard terms such as service names), so adopting it
in one step was not possible.

## Decision

1. **One lint table.** The lints live in the root `Cargo.toml` as `[workspace.lints.rust]` and
   `[workspace.lints.clippy]`. Every crate opts in with `[lints] workspace = true` and has no
   `[lints]` table of its own (Cargo cannot merge the two). Levels are `warn`, except
   `unsafe_code`, which is `deny`. CI builds with `-D warnings`, so a compiler lint fails CI; a
   Clippy lint is enforced by the local `cargo clippy --workspace --all-targets --locked` run
   that `CLAUDE.md` and `pr-review-loop` require before a pull request and its test plan
   records. Clippy is not added to CI.
2. **Overrides.** A lint is overridden on the narrowest item that needs it, with a reason
   (`clippy::allow_attributes_without_reason` enforces the reason). `#[expect]` is the default,
   so the override fails once the code no longer triggers the lint. `#[allow]` is used where
   `expect` would be wrong: when the lint fires only on some targets, cfgs or features, and for
   generated code. The C-ABI crates (the two mocks and `sim-vci`) may also hold a crate-wide
   override, with a reason, for a lint that their exported C names and platform-dependent C
   types trigger throughout (`non_snake_case`, `clippy::too_many_arguments`). A lint's level is never lowered, and an override never added or
   widened, to get a change through; the code is fixed, or the reason says why the lint is wrong
   in that place.
3. **`unsafe`.** `unsafe_code` is denied. The crates whose role is FFI or exporting a C ABI
   (`iso22900-sys`, `j2534-0404-sys`, `iso22900`, `j2534-0404`, `iso22900-mock`,
   `j2534-0404-mock`, `sim-vci`) grant it crate-wide with a reason. Every other crate grants it
   only on the item or small module that makes the operating-system or mock-library call, with
   a reason naming the call. Generated bindings get it, and the other overrides they need, on
   the hand-written module that includes them (`crates/*-sys/src/bindings.rs`,
   `crates/diag-ir/src/declaration.rs`, `crates/vci-service-interface/src/lib.rs`); the generated
   files are never edited for it, and the attributes their generators emit stay as they are.
4. **Stage 1 (this decision's change).** Compiler: `unsafe_code`, `ambiguous_negative_literals`,
   `missing_debug_implementations`, `redundant_lifetimes`, `unused_lifetimes`. Clippy: `allow_attributes_without_reason`, `redundant_clone`, and the
   recommended restriction lints that `main` almost never triggered (`disallowed_script_idents`,
   `empty_drop`, `empty_enum_variants_with_brackets`, `empty_structs_with_brackets`,
   `fn_to_numeric_cast_any`, `if_then_some_else_none`, `redundant_type_annotations`,
   `renamed_function_params`, `unnecessary_safety_comment`, `unnecessary_safety_doc`,
   `unused_result_ok`). The code is fixed so the workspace is clean. A type whose fields hold
   secret material gets a hand-written `Debug` that leaves them out. The recommended
   `clippy::unneeded_field_pattern` is not taken: it replaces `field: _` with `..`, and this
   code destructures some structs field by field on purpose, so that a new field fails to
   compile until the place that settles the struct handles it.
5. **Later stages.** A lint added later goes into the table at once. Each crate that does not
   pass it yet carries a crate-level `#![expect(<lint>, reason = "staged adoption, ADR-502")]`,
   removed when the crate is fixed; the `expect` itself warns once the crate is clean, so the
   opt-out cannot outlive its need. The planned stages, each as its own pull request:
   - stage 2: `clippy::undocumented_unsafe_blocks` (every `unsafe` block states why it is
     sound in a `// SAFETY:` comment), first in the crates that already document most blocks,
     with the mocks, the `*-sys` crates, `sim-vci` and the safe wrappers opted out until fixed;
   - stage 3, one pull request per lint: `clippy::map_err_ignore` (an error mapped without its
     cause, which hides whether a failure came from the vehicle side or the system side),
     `clippy::cast_possible_truncation` together with `clippy::cast_sign_loss` and
     `clippy::cast_possible_wrap` (length and counter fields of the protocols),
     `clippy::needless_pass_by_value` (a parameter taken by value that the function does not
     keep, which makes callers clone), and `clippy::assertions_on_result_states` (an
     `assert!(r.is_ok())` that hides the error when it fails).
   Whether to add the mostly cosmetic, auto-fixable lints of the recommended set
   (`redundant_imports`, `clippy::clone_on_ref_ptr` and the like) is decided after stage 3.
6. **Not adopted.** Whole Clippy groups beyond the defaults: `pedantic` (style and
   documentation lints such as `doc_markdown`, `missing_errors_doc`, `must_use_candidate`,
   `too_many_lines`, `unreadable_literal`; the useful ones are picked individually above),
   `cargo` (crates.io metadata for crates this workspace does not publish) and `nursery`
   (false positives; only `redundant_clone` is taken). `clippy::unwrap_used`, `expect_used`,
   `indexing_slicing` and `panic` (thousands of hits, and a detected programming bug is right
   to panic, M-PANIC-ON-BUG); the review rule on panics reachable from external input covers
   the case that matters. `clippy::missing_assert_message` (it also fires in tests, where a
   message is not needed); the rule is written as prose instead. `trivial_numeric_casts`: the
   D-PDU API and J2534 C types change width and signedness between targets (`c_int` or `u32`
   enums on msvc and elsewhere, `c_char` signed on x86 and unsigned on ARM, J2534 `unsigned long`
   32 or 64 bits), so the same cast is trivial on one CI target and needed on another, and the
   lint would need a target-dependent override at every such cast.
7. **Prose rules.** `CLAUDE.md` ("Rust code") states what a lint cannot check: no clone to get
   past the borrow checker, and what to do instead; a message on an intentional panic in
   non-test code; tests that check behaviour, not a restatement of the implementation; crate
   docs and doc comments that describe the current behaviour, with the design reasoning in an
   ADR; one public path per item. The review guidelines carry all of them; `implementer` names the
   ones agents break most and points to `CLAUDE.md` for the rest.

## Consequences

- An agent's bare `#[allow]` or new `unsafe` outside its layer fails a local
  Clippy run or CI instead of waiting for review; the review guidelines still flag the parts a
  lint cannot see (an override whose reason does not hold, a lowered level).
- A Clippy-only lint can still reach `main` when the local run is skipped, as one
  `collapsible_match` warning did before this change. The pull request template and
  `pr-review-loop` name the run; adding Clippy to CI remains possible if that keeps happening.
- Each later stage changes many lines in a few crates; the crate-level `expect` keeps the
  workspace building while the stage is under way.
- Stages 2 and 3 are backlog items in `work/`, prioritized as development process.
