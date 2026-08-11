# Spectograph

Find where bloat lives. Per-language static + runtime analysis that attributes cost to
every symbol, ranks bloat as a cost-per-usage ratio, and drives a safety-isolated agent
loop that refactors a codebase lean, verified by smoke tests and CI.

See [`DESIGN.md`](DESIGN.md) for the full design contract.

## Status

This is a working **vertical slice**: a Rust workspace where the Python analyzer
(`spectograph-py`) parses source with tree-sitter into a shared IR, and `spectograph check`
reports bloat as human-readable CLI output or versioned JSON.

Implemented:
- Cargo workspace with `core`, `analyzer`, `cli`, `py` crates.
- Language-agnostic IR: symbols, references (with uncertainty), entrypoints, exports.
- Cost/usage model and tiered bloat classification.
- Reachability from entrypoints (`__main__` guards, `__init__.py` package roots, and
  `pyproject.toml` `[project.scripts]` / `[project.gui-scripts]` console scripts).
- Python resolution: scope/shadowing, import aliasing, dynamic constructs marked
  uncertain (star imports, `getattr`, `__import__`), class method/attribute resolution
  (`self.method()`, `obj.method()`, constructor `__init__`), relative-import resolution
  (`from .sub import x`, `from .. import y`), and inheritance (`super().method()` reaches
  parent methods; subclassing keeps base classes reachable).
- Package handling: `__init__.py` re-exports as package roots, and `__all__` names as
  public API (never flagged as bloat).
- `.gitignore`-aware file discovery.
- `check` with `--format human|json` and `--fail-on <tier>` (exit codes for CI).

Not yet implemented (roadmap, per `DESIGN.md`):
- `fix` (agent loop + worktree isolation + smoke/CI gate).
- Runtime profiling, AI providers, incremental SQLite index for large repos.
- Remaining languages: JS, TS, Go, Rust, Java, Kotlin, C, C++, Ruby.

## Usage

```sh
# Human-readable report (default)
cargo run -p spectograph-cli -- check .

# Versioned JSON for CI
cargo run -p spectograph-cli -- check . --format json

# Fail the build only on guaranteed bloat
cargo run -p spectograph-cli -- check . --fail-on guaranteed

# Pick the language explicitly (else auto-detected)
cargo run -p spectograph-cli -- check . --language python
```

Exit code is nonzero when bloat at or above `--fail-on` tier is found.

## Fixtures

Real Python sample projects live under `fixtures/python/` and are exercised by
automated tests in `spectograph-py/tests/fixtures.rs`, which assert which symbols are
reachable and which are dead:

- `fixtures/python/python-sample` — entrypoint + dead code / unused imports
- `fixtures/python/python-pkg` — `__init__.py` re-exports and `__all__`
- `fixtures/python/python-scripts` — `pyproject.toml` console-script entrypoints
- `fixtures/python/python-classes` — method/attribute resolution
- `fixtures/python/python-relative` — relative imports
- `fixtures/python/python-inheritance` — `super()`/inheritance

Keeping these as versioned, test-backed fixtures ensures the Python sample code does not
inflate the repo's language breakdown while still being load-bearing.

## Tests

```sh
cargo test              # full workspace
cargo clippy --all-targets
```

## Workspace

| Crate | Purpose |
|---|---|
| `spectograph-core` | IR, cost model, bloat analysis, reporting |
| `spectograph-analyzer` | `Analyzer` trait + gitignore-aware discovery |
| `spectograph-cli` | `spectograph` binary (`check` / `fix`) |
| `spectograph-py` | Python analyzer (tree-sitter) |

Each `spectograph-<lang>` crate emits a shared `Program`; all bloat logic lives in `core`.
