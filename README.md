# Spectograph

Find where bloat lives. Per-language static + runtime analysis that attributes cost to
every symbol, ranks bloat as a cost-per-usage ratio, and drives a safety-isolated agent
loop that refactors a codebase lean, verified by smoke tests and CI.

See [`DESIGN.md`](DESIGN.md) for the full design contract, [`docs/accuracy.md`](docs/accuracy.md)
for measured accuracy and known limitations, [`docs/json-schema.md`](docs/json-schema.md) for
the JSON contract, and [`CHANGELOG.md`](CHANGELOG.md).

## Status

`spectograph check` is implemented for **Python**, **JavaScript** and **TypeScript**
(including React, Angular and Vue). JS/TS has been hardened against real projects (express,
commander, got, a Next.js app, Vue 3 apps, vuejs/core, facebook/react); see
`docs/accuracy.md` for the numbers. `spectograph fix` is **not implemented** and exits 2.

Implemented:
- Cargo workspace: `core`, `analyzer`, `cli`, `py`, `js`, `ts`, `tsserver`.
- Language-agnostic IR; linear-time reachability from entrypoints; tiered classification
  (🔴 guaranteed, 🟠 probable, 🟡 high-cost).
- **Entrypoints & public API:** `package.json` `main`/`module`/`bin`/`exports`/`scripts`
  (with `dist`→`src` remap), HTML `<script src>`, `angular.json`, monorepo packages, and
  Next/Nuxt/SvelteKit/Remix/Astro/Gatsby/Angular conventions. Exports of entry modules count
  as used from outside.
- **Resolution:** CommonJS and ESM, `tsconfig`/`jsconfig` `paths`/`baseUrl`/`extends`,
  `package.json` `imports`, workspace packages, `@/`-style aliases, TS `./x.js`→`x.ts`,
  `node_modules` entries, `browser` field remapping, tsconfig `include`/`exclude`.
- **Precision:** type refs, heritage clauses, default params, destructuring defaults,
  shorthand properties, member access, implicit uses (constructors, lifecycle hooks,
  externally-extended classes, `ProxyHandler` traps, computed-key members).
- **Frameworks** (additive on the same parse pass): React (components, JSX, hooks, prop
  mismatches), Angular (decorators, templates, DI, binding mismatches), Vue (SFCs, template
  usage and script bindings, prop/emit mismatches, route components, file-based routing
  plugins, `import.meta.glob`/`require.context`, Vue 2 patterns, `defineComponent`,
  `defineEmits<T>()` type forms, `defineModel()`, `resolveComponent('x')`, and unused
  `<style scoped>` classes). Mismatches and unused
  CSS appear as `diagnostics`; Vue limits are listed in `spectograph-js/src/vue.rs`'s module doc.
- **Python:** scope/import aliasing, class/method resolution, inheritance, `__init__.py`
  and `__all__`, optional basedpyright resolution.
- Optional type-aware resolution via `typescript-language-server`.
- **Parallel parsing (JS/TS):** files are parsed on a large-stack rayon pool, then symbol ids
  are relocated in input order, so output is identical to a sequential run.
- `.gitignore`-aware discovery; root-relative `/`-separated report paths; deterministic output.
- CI (`.github/workflows/ci.yml`) and tag-triggered release builds (`release.yml`) — written
  but not yet verified on GitHub.

Not yet implemented (see `DESIGN.md` checklist):
- `fix` (agent loop, worktree isolation, gates); runtime profiling; AI providers;
  incremental index.
- Flow syntax support; parallel parsing for Python (JS/TS parse in parallel).
- Go, Rust, Java, Kotlin, C, C++, Ruby.

## Usage

```sh
cargo run -p spectograph-cli -- check .                         # human report, language auto-detected
cargo run -p spectograph-cli -- check . --language typescript   # explicit language
cargo run -p spectograph-cli -- check . --format json           # versioned JSON for CI
cargo run -p spectograph-cli -- check . --fail-on guaranteed    # fail only on 🔴
cargo run -p spectograph-cli -- check . --fail-on-diagnostics   # also fail on framework mismatches
cargo run -p spectograph-cli -- check . --resolution pyright    # basedpyright (needs it on PATH)
cargo run -p spectograph-cli -- check . --resolution tsserver   # typescript-language-server
```

`--pyright-executable` / `--tsserver-executable` point at a specific server binary.

**Exit codes:** `0` clean · `1` findings at or above `--fail-on` (or diagnostics with
`--fail-on-diagnostics`) · `2` error (including `fix`, which is not implemented).

JSON output is versioned (`schema_version: 1`); see [`docs/json-schema.md`](docs/json-schema.md).

## Accuracy benchmark

```sh
cargo build --release
python3 scripts/bench.py
```

Clones eight real projects and prints findings per tier. See `docs/accuracy.md`.

## Fixtures

`fixtures/python/` and `fixtures/javascript/` hold older directory-based fixtures exercised
by `tests/fixtures.rs`. Don't add new ones: new tests write their sources inline via
`tempdir()` + `std::fs::write(...)` next to the assertions (as all of `spectograph-ts` does).
`.gitattributes` keeps fixtures out of GitHub's language statistics.

## Development

```sh
git config core.hooksPath .githooks   # pre-commit runs cargo fmt
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Workspace

| Crate | Purpose |
|---|---|
| `spectograph-core` | IR, cost model, reachability/bloat analysis, reporting, diagnostics |
| `spectograph-analyzer` | `Analyzer` trait + gitignore-aware discovery + ordered parallel map |
| `spectograph-cli` | `spectograph` binary (`check` / `fix` stub) |
| `spectograph-py` | Python analyzer (tree-sitter + optional basedpyright) |
| `spectograph-js` | JavaScript analyzer; shared resolution, entrypoints, exports, implicit uses, React/Vue |
| `spectograph-ts` | TypeScript analyzer; Angular; reuses `spectograph-js` shared modules |
| `spectograph-tsserver` | Shared `typescript-language-server` LSP client |

Each `spectograph-<lang>` crate emits a shared `Program`; all bloat logic lives in `core`.
