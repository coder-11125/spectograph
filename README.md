# Spectograph

Find where bloat lives. Per-language static + runtime analysis that attributes cost to
every symbol, ranks bloat as a cost-per-usage ratio, and drives a safety-isolated agent
loop that refactors a codebase lean, verified by smoke tests and CI.

See [`DESIGN.md`](DESIGN.md) for the full design contract.

## Status

This is a working **vertical slice**: a Rust workspace where each language analyzer parses
source with tree-sitter into a shared IR, and `spectograph check` reports bloat as
human-readable CLI output or versioned JSON. Three languages are implemented —
**Python**, **JavaScript**, and **TypeScript**.

Implemented:
- Cargo workspace with `core`, `analyzer`, `cli`, `py`, `js`, `ts`, `tsserver` crates.
- Language-agnostic IR: symbols, references (with uncertainty), entrypoints, exports,
  runtime environment (`Universal`/`Node`/`Browser`) inference.
- Cost/usage model and tiered bloat classification.
- Reachability from entrypoints (per-language conventions: Python's `__main__` guards,
  `__init__.py` package roots, `pyproject.toml` console scripts; JS/TS's `package.json`
  `main`/`bin`/`browser`/`exports`, conventional `index`/`main` files; Vue's
  `createApp(...).mount(...)` bootstrap convention).
- **Python** (`spectograph-py`): scope/shadowing, import aliasing, dynamic constructs
  marked uncertain (star imports, `getattr`, `__import__`), class method/attribute
  resolution (`self.method()`, `obj.method()`, constructor `__init__`), relative-import
  resolution, inheritance (`super().method()`, subclassing), `__init__.py` re-exports and
  `__all__` as public API. Optional **type-aware resolution** via basedpyright
  (`--resolution pyright`): shells out to the LSP server for find-references, resolving
  `obj.method()` by real type instead of name.
- **JavaScript** (`spectograph-js`): CommonJS (`require`/`module.exports`) and ES module
  resolution (default/named/namespace imports and re-exports), `node_modules`
  package-entry resolution (`exports`/`main` fields), bundler `browser`-field remapping,
  npm package recognition, environment-guard inference.
- **TypeScript** (`spectograph-ts`): everything JS has, plus `tsconfig.json`
  `baseUrl`/`paths` alias resolution and `include`/`exclude` filtering, decorator
  metadata capture, constructor-parameter dependency-injection edges. Optional
  **type-aware resolution** via `typescript-language-server` (`--resolution tsserver`,
  also usable for plain JavaScript with `allowJs`/`checkJs`) through the shared
  `spectograph-tsserver` client.
- **Framework support**, layered additively on the JS/TS symbol graph (a component is
  still an ordinary symbol; framework facts are extra data collected in the same parse
  pass) — library APIs only, not yet wired into the `check` CLI output:
  - **React** (JS + TS): function/class component detection, JSX usage sites, hook
    calls, prop-mismatch checking, `forwardRef`/`memo` unwrapping.
  - **Angular** (TS only): `@Component`/`@NgModule` decorator metadata, template
    scanning (component/directive/pipe usage, inline or `templateUrl`), DI edges,
    input/output binding-mismatch checking.
  - **Vue** (JS + TS): `.vue` Single-File Component parsing (`<script>`/
    `<script setup>` splitting with line-accurate reassembly), Options API and
    Composition API component detection (`defineProps`/`defineEmits`, including TS's
    generic form), template-usage reachability edges (kebab/PascalCase tag matching,
    `<component :is>`, global `app.component(...)` registration,
    `defineAsyncComponent` lazy-loading), prop/emit mismatch checking, and
    `createApp(...).mount(...)` entrypoint detection.
- `.gitignore`-aware file discovery.
- `check` with `--format human|json`, `--fail-on <tier>`, and `--language` auto-detection.

Not yet implemented (roadmap, per `DESIGN.md`):
- `fix` (agent loop + worktree isolation + smoke/CI gate).
- Runtime profiling, AI providers, incremental SQLite index for large repos.
- Wiring React/Angular/Vue framework findings (prop/binding mismatches) into `check`'s
  CLI output — currently only reachable as library APIs (`analyze_react`,
  `analyze_angular`, `analyze_vue`/`analyze_vue_mismatches`).
- Remaining languages: Go, Rust, Java, Kotlin, C, C++, Ruby.

## Usage

```sh
# Human-readable report (default; language auto-detected from the root)
cargo run -p spectograph-cli -- check .

# Explicit language
cargo run -p spectograph-cli -- check . --language typescript

# Versioned JSON for CI
cargo run -p spectograph-cli -- check . --format json

# Fail the build only on guaranteed bloat
cargo run -p spectograph-cli -- check . --fail-on guaranteed

# Type-aware resolution via basedpyright (needs basedpyright on PATH)
cargo run -p spectograph-cli -- check . --resolution pyright

# Point at a specific basedpyright LSP server executable
cargo run -p spectograph-cli -- check . --resolution pyright --pyright-executable /path/to/basedpyright-langserver

# Type-aware resolution via typescript-language-server (TypeScript, or JavaScript with allowJs/checkJs)
cargo run -p spectograph-cli -- check . --resolution tsserver
```

Exit code is nonzero when bloat at or above `--fail-on` tier is found.

## Fixtures

Real Python and JavaScript sample projects live under `fixtures/python/` and
`fixtures/javascript/`, exercised by `tests/fixtures.rs` in `spectograph-py` and
`spectograph-js` respectively, which assert which symbols are reachable and which are dead:

- `fixtures/python/python-sample` — entrypoint + dead code / unused imports
- `fixtures/python/python-pkg` — `__init__.py` re-exports and `__all__`
- `fixtures/python/python-scripts` — `pyproject.toml` console-script entrypoints
- `fixtures/python/python-classes` — method/attribute resolution
- `fixtures/python/python-relative` — relative imports
- `fixtures/python/python-inheritance` — `super()`/inheritance
- `fixtures/javascript/js-sample`, `js-pkg`, `js-commonjs`, `js-node-modules`,
  `js-package-main`, `js-relative`, `js-index`, `js-classes`, `js-inheritance`,
  `js-dynamic` — import resolution, CommonJS/ESM interop, `node_modules`/`package.json`
  entry resolution, inheritance, dynamic-import uncertainty.

Keeping these as versioned, test-backed fixtures ensures the sample code does not inflate
the repo's language breakdown while still being load-bearing. A `.gitattributes` file marks
the fixtures as generated code to further prevent them from affecting GitHub's language
statistics.

`spectograph-ts` has no equivalent `fixtures/typescript/` directory by design: its test
suite (and every framework-support suite added since — React, Angular, Vue, dependency
injection, decorators) writes each test's source files inline via `tempdir()` +
`std::fs::write(...)` right next to the assertions, rather than as committed fixtures.
Prefer that pattern for new tests.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Workspace

| Crate | Purpose |
|---|---|
| `spectograph-core` | IR, cost model, bloat analysis, reporting |
| `spectograph-analyzer` | `Analyzer` trait + gitignore-aware discovery |
| `spectograph-cli` | `spectograph` binary (`check` / `fix`) |
| `spectograph-py` | Python analyzer (tree-sitter + optional basedpyright) |
| `spectograph-js` | JavaScript analyzer (tree-sitter); shared React/Vue support |
| `spectograph-ts` | TypeScript analyzer (tree-sitter); Angular support; reuses React/Vue support from `spectograph-js` |
| `spectograph-tsserver` | Shared `typescript-language-server` LSP client, used by both `spectograph-js` and `spectograph-ts` |

Each `spectograph-<lang>` crate emits a shared `Program`; all bloat logic lives in `core`.
