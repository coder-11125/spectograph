# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Spectograph is a Rust CLI (`spectograph`) that finds "bloat" in a codebase — not just dead
code, but any high-cost/low-usage code (heavy modules pulled in for one cold call, unused
imports, orphaned functions). It parses source with tree-sitter into a shared,
language-agnostic IR, walks reachability from entrypoints, and scores every symbol as
`cost / usage`. See `DESIGN.md` for the full design contract (mission, bloat tiers, the
planned `fix` agent loop) — note it documents the target end-state for 10 languages; only
Python, JavaScript, and TypeScript are actually implemented right now (see workspace
members in `Cargo.toml`). `README.md` has usage examples but its "Status" section is stale
relative to the actual code (written when only the Python analyzer existed).

## Commands

```sh
cargo build --workspace                          # build everything
cargo test --workspace                            # full test suite
cargo test -p spectograph-ts                       # one crate
cargo test -p spectograph-ts --test vue             # one integration test file
cargo test -p spectograph-ts --test vue some_test_name   # one test
cargo test -p spectograph-js --lib vue::             # unit tests inside src/ (e.g. vue.rs's own #[cfg(test)] mod)
cargo clippy --workspace --all-targets              # lint (must be clean)
cargo fmt --all                                     # format

# Run the CLI directly
cargo run -p spectograph-cli -- check <path> [--format human|json] [--language python|javascript|typescript] \
    [--fail-on guaranteed|probable|high_cost] [--resolution static|pyright|tsserver]
```

One-time setup per clone: `git config core.hooksPath .githooks` enables a pre-commit hook
that runs `cargo fmt --all` and re-stages the affected files.

`spectograph fix` (the mutating agent-loop refactor command) is a stub — only `check` does
anything currently.

## Architecture

### The IR is the contract

Every language analyzer implements `Analyzer` (`spectograph-analyzer/src/lib.rs`) and turns
a source tree into a `Program` (`spectograph-core/src/ir.rs`): `Vec<Symbol>` +
`Vec<Reference>` + entrypoints. **All bloat/reachability logic lives once, in
`spectograph-core`** (`analysis.rs`: BFS reachability from entrypoints + exports, then
tiered classification; `cost.rs`: the cost/usage model; `report.rs`: human/JSON rendering).
Adding a language means writing one crate that produces a `Program` — nothing in `core`
changes. `spectograph-analyzer::discover::discover_files` provides `.gitignore`-aware file
discovery keyed by `Language::extensions()`.

Two conventions used everywhere in resolution code:
- **"First definition wins"** ("vertical slice"): when a name is ambiguous across the
  project, the first symbol found is what everything resolves to.
- **`RefKind`** (`Static`/`ReExport`/`Dynamic`/`Reflection`/`StringKeyed`) marks confidence.
  Only `Static`/`ReExport` are walked for reachability (`RefKind::is_uncertain()`); dynamic
  refs (`require(x)` with a non-literal `x`, `eval`, reflection) don't count as "used" but
  also don't prove something dead.
- **`Environment`** (`Universal`/`Node`/`Browser`) is inferred from guard patterns
  (`typeof window !== 'undefined'`) or framework-specific signals (e.g. Vue's
  `createApp(...).mount(...)` tags a file `Browser`). `is_external` marks
  `node_modules`/third-party symbols, which are parsed (so imports resolve) but excluded
  from bloat findings.

### Per-language crates

- `spectograph-py`: tree-sitter Python. Optional type-aware resolution via basedpyright
  (`pyright.rs`, its own embedded LSP client over stdio).
- `spectograph-js` / `spectograph-ts`: tree-sitter JS / TS. `spectograph-ts` depends on
  `spectograph-js` (for shared React support — see below), so it can never be a dependency
  *of* `spectograph-js`. Optional type-aware resolution via `typescript-language-server`
  lives in its own crate, `spectograph-tsserver` — pulled out separately (not into
  `spectograph-py`-style per-crate `pyright.rs`) specifically because both `spectograph-js`
  and `spectograph-ts` need the same client and neither can depend on the other for it.
- Each `analyze_impl` runs a fixed pass order: parse every file (symbols + unresolved refs)
  → resolve in-file refs → resolve imports across files → framework-specific passes (see
  below) → detect entrypoints. `analyze_with_resolution(..., ResolutionMode::Tsserver/Pyright)`
  swaps the static call-graph edges for LSP-resolved ones after the static pass, rather than
  replacing resolution entirely.

### Framework support is additive, not a separate analyzer

React/Angular/Vue detection is layered on top of the *same* tree-sitter parse pass that
builds the base symbol/reference graph — a component is still an ordinary `Symbol`
(`Function`/`Class`/`Const`), and framework facts (props, template-usage edges, prop/emit
mismatches) are extra data collected alongside it, not a separate parse:

- **React** (`spectograph-js::react`, re-exported by `spectograph-ts`): shared data types
  and naming-convention helpers (`is_component_name`, `is_hook_name`,
  `find_prop_mismatches`), but the actual tree-sitter walk (`detect_react`,
  `scan_jsx_and_hooks`) is duplicated once per crate in each `parse.rs`, since JS/TS/TSX
  grammars produce slightly different node shapes for typed parameters etc. Exposed via
  `analyze_react()`.
- **Angular** (`spectograph-ts::angular`, TS-only): scans `@Component`/`@NgModule`
  decorator metadata and templates (inline or resolved `templateUrl`) with a lightweight
  regex scanner (not a full HTML parser) to find component/directive/pipe usage.
  `analyze_angular()`.
- **Vue** (`spectograph-js::vue`, re-exported by `spectograph-ts`): the most involved case,
  because a `.vue` Single-File Component isn't valid JS/TS on its own.
  `vue::parse_sfc`/`materialize_script` split `<template>`/`<script>`/`<script setup>`
  blocks and reassemble the script(s) into one source string with each line pinned to its
  true position in the original file (blank-padded elsewhere), so the ordinary
  `parse_file` runs against it unchanged with correct spans. A `.vue` file routes to
  whichever analyzer matches its script's `lang` attribute (untyped → JS, `lang="ts"` →
  TS). As with React, pure data/matching logic (template tag scanning, kebab↔Pascal/camel
  conversion, `has_vue_app_mount`) lives once in `vue.rs`; the tree-sitter detection itself
  (`detect_vue_component`, `defineAsyncComponent` handling, global-registration scanning)
  is duplicated per crate in each `parse.rs`. Exposed via `analyze_vue()` and
  `analyze_vue_mismatches()`. Known gaps are documented in `vue.rs`'s module doc comment
  (Vue 2 quirks, scoped CSS, Vuex/Pinia, Vue Router, non-identifier `:is` expressions).

When extending one of these, match the existing split: framework-agnostic types/pure
helpers in the shared module, grammar-specific tree walking duplicated per crate.

### Testing convention

Newer test suites (everything in `spectograph-ts/tests/`, and all Vue tests in both JS and
TS) write each test's source files **inline** via `tempdir()` + `std::fs::write(...)`
directly in the test function, rather than committed fixture directories — the test's setup
is visible right next to its assertions, with no separate file to go find. `fixtures/javascript`
and `fixtures/python` still hold committed directory-based fixtures from an older
convention, exercised by `tests/fixtures.rs` in those two crates; don't add new fixtures
there — follow the tempdir-inline pattern instead.

## Do's and Dont's 
- Always use the native Read and Write tools instead of using `grep`, `cat`, or `mkdir`
- After every new change to the codebase, always update the `README.md`, `CLAUDE.md`, and `DESIGN.md` files to reflect the changes.
- Use the `DESIGN.md` file as a checklist or a roadmap you can reflect to.
- If asked to commit changes, follow the commit message conventions in `GIT.md`.
- Never make surgical edits on any file; always read the complete file and understand the context before making one big change.