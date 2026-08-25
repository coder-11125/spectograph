# Adding a language analyzer

A checklist and cookbook for adding a new `spectograph-<lang>` crate — the next
milestone per `DESIGN.md` §3 (Go, Rust, Java, Kotlin, C, C++, Ruby are queued).
Written from the three existing analyzers (`spectograph-py`, `spectograph-js`,
`spectograph-ts`), which all follow the same shape closely enough that adding a
fourth is mostly filling in this template with the new language's own grammar and
resolution rules.

Read this alongside `CLAUDE.md` ("The IR is the contract" / "Per-language crates")
for the *why*; this doc is the concrete *how*.

## Checklist

1. Add the crate to `Cargo.toml`'s `[workspace] members` and `[workspace.dependencies]`.
2. Add a `Language` variant + extensions to `spectograph-core/src/ir.rs`.
3. Scaffold the crate: `Cargo.toml`, `src/lib.rs`, `src/parse.rs`, `src/entrypoints.rs`.
4. Implement `Analyzer` (`language()` + `analyze()`), following the fixed pass order
   below.
5. Wire entrypoint detection for the language's own conventions.
6. (Optional, later) Add a type-aware resolution backend.
7. Wire the CLI: `spectograph-cli/src/run.rs`'s `analyzer_for` and `detect_language`.
8. Write tests — tempdir-inline, not a `fixtures/<lang>/` directory (see
   `CLAUDE.md`'s "Testing convention").
9. Update `README.md`'s Status section and `DESIGN.md`'s workspace-layout list.

## 1–2. Workspace and `Language` plumbing

`Cargo.toml`:

```toml
[workspace]
members = [
    # ...
    "spectograph-<lang>",
]

[workspace.dependencies]
spectograph-<lang> = { path = "spectograph-<lang>", version = "0.1.0" }
```

`spectograph-core/src/ir.rs`'s `Language` enum already lists all ten target
languages (`Go`, `Rust`, `Java`, `Kotlin`, `C`, `Cpp`, `Ruby` are declared but
unimplemented) — check `extensions()`/`name()` already have the right entries; if
not, add them there. Nothing else in `core` changes: `core` only ever consumes a
`Program`, never a specific `Language`.

## 3–4. Crate skeleton and the `Analyzer` impl

`Cargo.toml` dependencies, following the existing three crates:

```toml
[dependencies]
spectograph-core.workspace = true
spectograph-analyzer.workspace = true
tree-sitter = "0.25"
tree-sitter-<lang> = "..."      # pick the grammar's current version
serde_json.workspace = true     # only if you'll read a package-manifest file
thiserror.workspace = true      # only if the crate needs its own error type

[dev-dependencies]
tempfile = "3"
```

`src/lib.rs` implements `spectograph_analyzer::Analyzer`
(`spectograph-analyzer/src/lib.rs`):

```rust
pub struct <Lang>Analyzer;

impl Analyzer for <Lang>Analyzer {
    fn language(&self) -> Language { Language::<Lang> }
    fn analyze(&self, root: &Path) -> Result<Program, AnalyzeError> { /* below */ }
    // cost_model() has a default impl (spectograph-analyzer/src/lib.rs:41) --
    // only override it if the language needs different cost heuristics.
}
```

`analyze()` (or an `analyze_impl` it delegates to, if you'll also want an
`analyze_with_resolution` variant — see §6) runs a **fixed pass order**, identical
across all three existing analyzers:

```
Pass 1: discover files, parse each one (spectograph_analyzer::discover::discover_files)
        -> per-file symbols + unresolved refs + import declarations
        -> build indices: global_names (name -> id, first-definition-wins),
           def_index (Function/Class name -> id, for import fallback resolution),
           module_paths (this language's module-path string -> module symbol id),
           name_in_module ((module id, name) -> symbol id)
Pass 2: resolve in-file references (UnresolvedRef -> Reference) against the indices
Pass 3: resolve imports to the definitions they re-export (RefKind::ReExport)
Pass 4: detect and mark entrypoints (see §5)
```

Copy this shape directly from whichever existing analyzer is structurally closest
to the new language (`spectograph-py/src/lib.rs` for anything with Python-style
package/module semantics; `spectograph-ts/src/lib.rs` for anything with a project
manifest like `tsconfig.json`/`Cargo.toml`/`go.mod` driving resolution).

**"First definition wins"**: when a name is declared in more than one place across
the project, `global_names`/`def_index` keep the *first* symbol found
(`.entry(name).or_insert(id)`) — don't overwrite. Every existing analyzer relies on
this "vertical slice" convention; keep it for consistency even though a more
precise resolver could do better.

## The `parse.rs` skeleton

Single-file parsing lives in its own module, called once per discovered file. The
shape is the same in all three crates:

```rust
struct Scopes { stack: Vec<HashMap<String, Binding>> }  // lexical scope stack
enum Binding { Global(SymbolId), Local, Dynamic }        // how a name resolves

pub struct FileResult {
    pub module_id: SymbolId,
    pub symbols: Vec<Symbol>,
    pub refs: Vec<UnresolvedRef>,
    pub imports: Vec<ImportDecl>,
    // + whatever language-specific extras this language needs (Python's `exports`
    //   for `__all__`; JS's `components`/`jsx_usages`/`hook_calls` for React, etc.)
}

pub struct ImportDecl {
    pub symbol_id: SymbolId,
    // + whatever the language needs to resolve the source module: a dotted path
    //   and relative depth for Python, a specifier string for JS/TS, ...
}

pub enum RefTarget { Symbol(SymbolId), Name(String) }
pub struct UnresolvedRef { pub from: SymbolId, pub target: RefTarget, pub kind: RefKind, ... }

pub fn parse_file(path: &Path, src: &str, module_id: SymbolId, next_id: &mut usize) -> FileResult {
    // 1. parse with tree-sitter
    // 2. push a Module symbol (module_id) — always first in `symbols`
    // 3. collect_globals(root, ...): register top-level functions/classes/imports
    // 4. collect_refs(root, ...): walk the tree, push/pop scopes for
    //    function/class bodies, bind params/assignments as Local, and for every
    //    name reference push an UnresolvedRef from the enclosing symbol
}

fn symbol(id: SymbolId, name: &str, kind: SymbolKind, path: &Path, node: Node, ...) -> Symbol { ... }
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str { node.utf8_text(src).unwrap_or("").trim() }
fn span(node: Node) -> SourceSpan { /* 1-based line/col from node.start_position()/end_position() */ }
```

Mark uncertain resolution explicitly rather than guessing: a star import, `eval`,
reflection-style dynamic lookup, or anything else that can't be resolved to one
specific symbol gets `RefKind::Dynamic`/`RefKind::Reflection`/`RefKind::StringKeyed`
— never `RefKind::Static`. `core`'s reachability walk only trusts `Static`/
`ReExport` (`RefKind::is_uncertain()`), so an over-eager `Static` edge produces a
false "not bloat" and an under-eager one produces a false "dead code" — mark
honestly rather than picking whichever is more convenient.

Everything under a third-party/vendor directory convention for this language
(Python's — none, actually, it doesn't have one; JS/TS's `node_modules`) should get
`is_external = true` on every symbol — those still get parsed (so imports resolve),
but are excluded from bloat findings in `core`.

If the language has an unambiguous "runtime restricted to one environment" signal
(the way JS/TS infer `Environment::Browser`/`Node` from `typeof window`/`process`
guards), infer it the same way; otherwise leave every symbol/reference at the
default `Environment::Universal`.

## 5. Entrypoint detection

`src/entrypoints.rs` returns the language's own entrypoint symbol ids, layered on
top of (not replacing) whatever the base `analyze()` pass already found. Match the
language's real-world conventions — see the three existing implementations for the
range of what "entrypoint" means per ecosystem:

- Python (`spectograph-py/src/entrypoints.rs`): `__main__` guards, `__init__.py`
  package roots, `pyproject.toml` `[project.scripts]`/`[project.gui-scripts]`.
- JS (`spectograph-js/src/entrypoints.rs`): `package.json` `main`/`bin`/`browser`/
  `exports`, falling back to every non-external module (permissive fallback for a
  project with no recognizable convention).
- TS (`spectograph-ts/src/entrypoints.rs`): the same `package.json` fields plus
  conventional `index.ts`/`main.ts` filenames, falling back to every module only
  when *none* of those resolve to anything (deliberately more restrictive than JS's
  fallback — see that file's own doc comment for why).

Whatever the new language's package manager/build tool considers "this is what
runs" (a `go.mod` binary, a Rust `[[bin]]`, a Java `main` class, ...) is the
equivalent signal to key off here.

## 6. Optional: type-aware resolution

Not required for a first pass — every existing analyzer's static resolver works
standalone. If the language has a mature LSP server, follow `spectograph-py`'s
`pyright.rs` (a self-contained JSON-RPC-over-stdio client) or `spectograph-tsserver`
(pulled into its *own* crate because two analyzers — `spectograph-js` and
`spectograph-ts` — needed to share one client without a circular dependency). Wire
it as `<Lang>Analyzer::analyze_with_resolution(root, ResolutionMode, ...)`, which
runs the static pass first and then *replaces* the call-graph edges between
queryable symbols with the LSP-resolved ones — never skip the static pass.

## 7. CLI wiring

`spectograph-cli/src/run.rs`:

- `analyzer_for(lang: &str)`: add a match arm mapping the language's name(s) (full
  and short form, e.g. `"typescript" | "ts"`) to `Box::new(<Lang>Analyzer)`.
- `detect_language`'s `candidates` array already lists all ten `Language` variants
  for auto-detection — nothing to add there, it starts working the moment
  `discover_files` can find the new extension.
- If you added a resolution backend in step 6, add its CLI flags to
  `spectograph-cli/src/cli.rs`'s `CheckArgs` and thread them through `run_check`,
  mirroring `--resolution pyright`/`--pyright-executable`/`--pyright-timeout` or
  `--resolution tsserver`/`--tsserver-executable`/`--tsserver-timeout`.

## 8. Tests

Write tests **inline** via `tempdir()` + `std::fs::write(...)` in the test
function, not as a committed `fixtures/<lang>/` directory — see `CLAUDE.md`'s
"Testing convention" for why (`fixtures/python`/`fixtures/javascript` predate this
convention and are being left as-is, not extended). A minimal starting set, mirrored
across every existing crate's `tests/`:

- `resolution.rs` — symbol/reference extraction for the language's core constructs.
- `imports.rs` (or folded into `resolution.rs`) — cross-file import resolution,
  aliasing, re-exports.
- `entrypoints.rs` — the conventions from step 5.
- `discovery.rs` — if the language has an include/exclude config file worth testing
  (tsconfig-style); otherwise skip.

## 9. Update the docs

Per `CLAUDE.md`'s "Do's and Dont's": after landing the new analyzer, update
`README.md`'s Status section (new language bullet, workspace table row) and
`DESIGN.md`'s §3 workspace-layout list (move the crate from "not yet started" to
"implemented"). If you also add framework-specific support (the way React/Angular/
Vue sit on top of JS/TS), that's a separate concern — not covered by this doc.
