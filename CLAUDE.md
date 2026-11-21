# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Spectograph is a Rust CLI (`spectograph`) that finds "bloat" in a codebase — not just dead
code, but any high-cost/low-usage code (heavy modules pulled in for one cold call, unused
imports, orphaned functions). It parses source with tree-sitter into a shared,
language-agnostic IR, walks reachability from entrypoints, and scores every symbol as
`cost / usage`. See `DESIGN.md` for the full design contract (mission, bloat tiers, the
planned `fix` agent loop, and a status checklist) — it documents the target end-state for 10
languages; only Python, JavaScript, and TypeScript are implemented (see workspace members in
`Cargo.toml`). `README.md` has usage; `docs/accuracy.md` has measured results and known
limitations; `docs/json-schema.md` pins the JSON contract.

## Commands

```sh
cargo build --workspace                          # build everything
cargo test --workspace                            # full test suite
cargo test -p spectograph-ts                       # one crate
cargo test -p spectograph-ts --test vue             # one integration test file
cargo test -p spectograph-ts --test vue some_test_name   # one test
cargo test -p spectograph-js --lib vue::             # unit tests inside src/
cargo clippy --workspace --all-targets              # lint (must be clean)
cargo fmt --all                                     # format

cargo run -p spectograph-cli -- check <path> [--format human|json] [--language python|javascript|typescript] \
    [--fail-on guaranteed|probable|high_cost] [--fail-on-diagnostics] [--resolution static|pyright|tsserver]

cargo build --release && python3 scripts/bench.py   # accuracy benchmark on 8 real repos
```

One-time setup per clone: `git config core.hooksPath .githooks` enables a pre-commit hook
that runs `cargo fmt --all` and re-stages the affected files.

`spectograph fix` is a stub that exits 2 — only `check` does anything.
Exit codes: 0 clean, 1 findings at/above `--fail-on`, 2 error.

## Architecture

### The IR is the contract

Every language analyzer implements `Analyzer` (`spectograph-analyzer/src/lib.rs`) and turns
a source tree into a `Program` (`spectograph-core/src/ir.rs`): `Vec<Symbol>` +
`Vec<Reference>` + entrypoints. **All bloat/reachability logic lives once, in
`spectograph-core`** (`analysis.rs`: linear adjacency-list BFS from entrypoints, then tiered
classification, deterministic sort; `cost.rs`: the cost/usage model; `report.rs`:
human/JSON rendering, `SCHEMA_VERSION = 1`, additive changes don't bump it;
`diagnostic.rs`: framework mismatch diagnostics). Adding a language means writing one crate
that produces a `Program`. `spectograph-analyzer::discover::discover_files` provides
`.gitignore`-aware discovery keyed by `Language::extensions()`.

Conventions used everywhere in resolution code:
- **Scoped resolution; "first definition wins" only as the by-name fallback.** Imports link
  to the definition *inside the target module* — never a same-named symbol elsewhere. A
  guessed default-export definition must be a declaration, never an `Import` binding.
- **`RefKind`** (`Static`/`ReExport`/`Dynamic`/`Reflection`/`StringKeyed`) marks confidence.
  Only `Static`/`ReExport` are walked (`RefKind::is_uncertain()`).
- **`RefTarget`** (parser output): `Symbol`, `Name`, `Member` (property access by short name,
  resolved to class/enum members and exports), `Module` (side-effect import / `const x =
  require()`), `ModuleNamespace` (`import()` / bare `require()`).
- **`Environment`** (`Universal`/`Node`/`Browser`) inferred from guards or framework signals.
  `is_external` marks third-party symbols: parsed, never reported.
- **`BloatTier` ordering** is Guaranteed < Probable < HighCost, so `--fail-on` uses
  `f.tier <= min_tier`.

### Per-language crates

- `spectograph-py`: tree-sitter Python; optional basedpyright (`pyright.rs`).
- `spectograph-js` / `spectograph-ts`: tree-sitter JS / TS. `spectograph-ts` depends on
  `spectograph-js`, never the reverse. Shared JS-side modules (used by both): `aliases.rs`
  (`ProjectResolver`: tsconfig/jsconfig paths, package.json imports, workspaces, `@/`),
  `tsconfig.rs` + `jsonc.rs`, `pathutil.rs`, `entrypoints.rs` (`EntryDetection {entrypoints,
  api_roots, fallback}`; frameworks' conventions; in fallback every module is an entrypoint
  and `api_roots` is empty), `exports.rs` (API-root rule + whole-module-use rule),
  `implicit.rs` (constructors, protocol methods, lifecycle hooks, external bases, JSX
  `React`, `.d.ts`), `template_bindings.rs`, `diagnostics.rs`. Type-aware resolution lives in
  `spectograph-tsserver`.
- **Parallel parsing (JS/TS).** Pass 1 parses files via `spectograph_analyzer::parallel::par_map`
  (rayon, 32 MiB worker stacks since tree walks are recursive, sequential below 16 files) with
  file-local ids (module = 0, members from 1), then `FileResult::shift_ids` (`shift.rs` in each
  crate) relocates each result to its slot in input order, so ids are identical to a sequential
  parse. **Any new `SymbolId`-bearing field added to `FileResult` (or a type it holds) must be
  added to that crate's `shift_ids`**; `tests/parallel.rs` covers it. Python is still sequential.
- Each `analyze_impl` runs fixed passes: parse every file → resolve in-file refs (with an
  `EdgeSink` dedupe) → resolve imports (plus a module edge) → Vue/Angular passes → entry
  detection → export edges → implicit edges. `analyze_with_resolution` swaps static call
  edges for LSP ones afterwards. `analyze_full` also returns diagnostics.
- Module paths are normalized so `check .` works; report paths are root-relative with `/`.

### Framework support is additive, not a separate analyzer

React/Angular/Vue detection is layered on the same tree-sitter parse. Pure data/helpers live
once (`react.rs`, `vue.rs` in `spectograph-js`; `angular.rs` in `spectograph-ts`);
grammar-specific tree walking is duplicated per crate's `parse.rs`. Vue: `parse_sfc` does
depth-aware nested `<template>` matching; `materialize_script` pins each script line to its
true position. Known gaps are in `vue.rs`'s module doc. Match the existing split when
extending.

### Testing convention

New tests write sources **inline** via `tempdir()` + `std::fs::write(...)`. Don't add to
`fixtures/` (legacy). Accuracy regressions go in `spectograph-{js,ts}/tests/accuracy.rs`
(or a small focused file); CLI behavior in `spectograph-cli/tests/cli.rs`; JSON contract in
`spectograph-core/tests/report.rs`; scaling in `spectograph-core/tests/scale.rs`.
`.gitattributes` keeps fixtures out of language stats.

### Known limitations

Flow syntax can't be parsed by tree-sitter-javascript (slow + noisy on e.g. facebook/react);
member resolution is by name (precision-first); Python parsing is single-threaded. See
`docs/accuracy.md`.

## Do's and Dont's
- Always use the native Read and Write tools instead of using `grep`, `cat`, or `mkdir`
- After every new change to the codebase, always update the `README.md`, `CLAUDE.md`, and `DESIGN.md` files to reflect the changes.
- Use the `DESIGN.md` file as a checklist or a roadmap you can reflect to.
- If asked to commit changes, follow the commit message conventions in `GIT.md`.
- Never make surgical edits on any file; always read the complete file and understand the context before making one big change.
