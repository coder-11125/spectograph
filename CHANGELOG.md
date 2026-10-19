# Changelog

## Unreleased — JS/TS production hardening

### Added
- Framework diagnostics (React/Angular/Vue mismatches) in `check` output and JSON
  (`diagnostics`, `--fail-on-diagnostics`); `language` and per-finding `kind` in JSON.
- Public-API detection: entry-module exports (package `main`/`module`/`bin`/`exports`,
  HTML `<script>`, `angular.json`, framework conventions for Next, Nuxt, SvelteKit,
  Remix, Astro, Gatsby, Angular, monorepo packages) count as used.
- Project resolution: tsconfig/jsconfig `paths`/`baseUrl`/`extends`/`references` (JSONC),
  `package.json` `imports`, workspace packages, `@/`, `~/`, `$lib/`, `src/`.
- TS ESM `./x.js` → `x.ts` resolution; tsc-accurate `include`/`exclude` globs.
- Implicit uses: constructors, language protocol methods, Angular/React lifecycle,
  externally-extended classes, JSX `React` import, `.d.ts` ambient declarations.
- Vue template → script binding edges; depth-aware nested `<template>` parsing.
- Linear-time reachability (200k-symbol test), deterministic output, root-relative
  `/`-separated paths.
- CI (fmt/clippy/test), tag-triggered release workflow, `scripts/bench.py`.
- 10 end-to-end CLI tests; accuracy suites for JS and TS.

### Fixed
- `--fail-on` tier comparison was inverted.
- Auto language detection picked the wrong language for TS repos.
- Type refs, shorthand properties, default params, heritage clauses, arrow bodies and
  member access were not tracked as uses.
- Default imports linked to a same-named `import` binding inside the target module.

### Changed
- `fix` now exits 2 with an explicit "not implemented" error.
- `Cargo.lock` is committed.

### Known limitations
See `docs/accuracy.md` (Flow syntax, name-based member resolution, single-threaded parse).
