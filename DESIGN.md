# Spectograph

Find where bloat lives. Per-language static + runtime analysis that attributes cost to
every symbol, ranks bloat as a cost-per-usage ratio, and drives a safety-isolated agent
loop that refactors a codebase lean — verified by the project's own smoke tests and CI.

---

## 0. Status checklist (roadmap)

- [x] Shared IR, linear reachability, tiered classification, deterministic output
- [x] `check` CLI: human + versioned JSON, `--fail-on`, `--fail-on-diagnostics`, exit codes 0/1/2
- [x] Python, JavaScript, TypeScript analyzers (tree-sitter)
- [x] JS/TS: entrypoint + public-API detection, tsconfig/package.json/workspace resolution
- [x] JS/TS: React, Angular, Vue support; framework diagnostics surfaced in `check`
- [x] Vue completeness: route components, file-based routing plugins, `import.meta.glob` /
      `require.context`, Vue 2 (`Vue.extend`, `new Vue({ el })`, mixins), `defineComponent`,
      `defineEmits<T>()` type forms, directive modifiers, unused `<style scoped>` classes
      `defineModel`, `resolveComponent`, fallthrough-aware mismatch checks; validated on
      vue3-realworld and vuejs/core (remaining limits: `vue.rs` module doc)
- [x] Accuracy benchmark on real repos (`scripts/bench.py`, `docs/accuracy.md`)
- [x] CI + release workflows (written; **not yet verified on GitHub / Windows**)
- [ ] `fix` agent loop, worktree isolation, smoke/CI gate (currently exits 2)
- [ ] Runtime profiling cost signal (static estimate only today)
- [ ] AI providers (`BloatAi`), uncertain-reference disambiguation
- [x] Parallel parsing for JS/TS (ordered, deterministic id relocation); Python still sequential
- [ ] Incremental SQLite index for large repos
- [ ] Flow syntax support (facebook/react-style code is noisy)
- [ ] Go, Rust, Java, Kotlin, C, C++, Ruby analyzers

---

## 1. Mission

Identify code that is "clearly taxing the system or app," regardless of whether it happens
to still work. Bloat is not only *dead code* (unreachable / unused) but any **high-cost,
low-usage** code: heavy modules pulled in for one cold call, import chains that drag the
whole app down, orphaned functions, unused imports, pullable dependencies.

- **`spectograph check`** — read-only. Builds the cost/usage model and reports bloat.
- **`spectograph fix`** — mutating (planned). Refactors in an isolated worktree, gated by
  smoke tests + CI, iterating until lean.

---

## 2. Design contract (locked)

| Concern | Decision |
|---|---|
| Tool language | Rust workspace |
| Analyzed languages | Python, JS, TS, Go, Rust, Java, Kotlin, C, C++, Ruby (10) |
| Language depth | Uneven: Python/JS/TS deep first; C/C++ static-only first pass |
| Bloat definition | **cost / usage** ratio (not just reachability) |
| Cost signal | **Runtime profiling** (entry commands + sampling, CPU first); static fallback |
| Dynamic refs | Static pass first; deterministic refactoring first; AI for ambiguous cases only |
| AI providers | Anthropic, OpenAI, Gemini, Bedrock, Vertex, Mistral, opencode zen/go — all equal |
| Scope | Everything except `.gitignore`; **per-language** only, no cross-language reachability |
| Output | CLI (human, tiered) + versioned JSON; nonzero exit for CI; `--fail-on <tier>` |
| Accuracy | **Precision first**: a 🔴 finding in a healthy project should be true |
| `fix` safety | Separate git worktree; asks before merging |
| Correctness gate | smoke tests → full suite → green, within time budget + score threshold |
| Scale | 10k → 1M LOC: batch for small, incremental index for large |

---

## 3. Workspace layout

```
spectograph/
├── DESIGN.md  README.md  CLAUDE.md  CHANGELOG.md
├── docs/                       json-schema.md, accuracy.md, cookbooks
├── scripts/bench.py            accuracy benchmark on real repos
├── .github/workflows/          ci.yml, release.yml
├── fixtures/                   legacy directory fixtures (js, python)
├── spectograph-core/           IR, cost model, reachability, reporting, diagnostics
├── spectograph-cli/            the binary: check / fix(stub)
├── spectograph-analyzer/       analyzer trait + discovery
├── spectograph-py/             Python analyzer
├── spectograph-js/             JS analyzer + shared JS/TS modules (see below)
├── spectograph-ts/             TS analyzer; Angular; reuses spectograph-js
└── spectograph-tsserver/       shared typescript-language-server client
```

Shared JS/TS modules in `spectograph-js`: `aliases` (project resolver), `tsconfig` +
`jsonc`, `pathutil`, `entrypoints`, `exports`, `implicit`, `template_bindings`,
`diagnostics`, `react`, `vue`, `globs`, `vue_css`. Framework support is additive on the same parse pass, not a
crate per framework.

Not yet started: `spectograph-go`, `-rs`, `-java`, `-kotlin`, `-c`, `-cpp`, `-ruby`.

---

## 4. Shared IR

```rust
pub struct Program { symbols, references, entrypoints, language, root, reachable }
pub struct Symbol  { id, name, kind, file, span, reachable, environment, is_external }
pub struct Reference { from, to, kind: RefKind, environment }
```

`RefKind`: Static, ReExport, Dynamic, Reflection, StringKeyed (only the first two are
walked). Parsers emit `RefTarget`s (`Symbol`, `Name`, `Member`, `Module`, `ModuleNamespace`)
that the analyzer resolves into edges. `core` only ever sees `Program`.

---

## 5. Analysis pipeline

parse → resolve in-file refs → resolve imports (+ module edges) → framework passes →
entrypoint/API-root detection → export edges (API roots, whole-module uses) → implicit
edges (constructors, lifecycle, external bases) → `core` reachability + cost + tiers →
report.

### 5.1 Cost model
Static estimate today (LOC, import weight); runtime profiling is planned.

### 5.2 Uncertainty & AI providers (planned)
Uncertain refs are never walked and never prove anything dead. `BloatAi` will normalize
providers; deterministic refactoring first, AI only for ambiguous high-impact cases.

### 5.3 Bloat score
`bloat = cost / usage`; tiers: 🔴 guaranteed (zero usage), 🟠 probable (only uncertain
usage), 🟡 high cost / low usage.

---

## 6. Agent loop (`spectograph fix`) — planned

Targeted context (top-N candidates), tiered models, rerank each iteration, incremental
re-analysis.

### 6.1 Thinking-loop detection
Abort on oscillation, stall, no-op diff, revisited candidate, or budget exhaustion; keep the
best state and report why.

### 6.2 Safety & worktree isolation
Edits in a separate worktree; correctness gate (smoke → full suite); ask before merging;
monotonic guard; per-iteration audit trail.

---

## 7. Output & CI

- `--format human` (default) and `--format json` (`schema_version` 1, see
  `docs/json-schema.md`; additive changes don't bump it).
- Exit codes: 0 clean, 1 findings at/above `--fail-on` (or diagnostics with
  `--fail-on-diagnostics`), 2 error.
- Report paths are root-relative, `/`-separated; output is deterministic.

---

## 8. Scale

Reachability is linear (200k-symbol test in CI). JS/TS parsing is parallel (file-local ids
relocated in input order, so output is deterministic); Python parsing is still sequential.
Large repos (facebook/react, ~32k symbols) took ~30s before parallelism, dominated by
tree-sitter error recovery on Flow syntax (not re-measured since). Planned: incremental
SQLite index above ~50k LOC, Python parallel parsing.

---

## 9. Config surface (initial)

`.gitignore` honored; entry/test/CI commands per repo; bloat budget + `--fail-on`; provider
selection and credentials; scale thresholds, time budgets, `--max-iterations`.
