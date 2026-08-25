# Adding framework-specific support (React/Angular/Vue-style)

A checklist and cookbook for layering framework awareness (component detection,
template-usage reachability, prop/binding mismatch checking) on top of an existing
`spectograph-js`/`spectograph-ts`-style analyzer, written from the three examples
that already exist: React (`spectograph-js/src/react.rs`), Angular
(`spectograph-ts/src/angular.rs`), and Vue (`spectograph-js/src/vue.rs`). No
framework is currently queued the way languages are in `docs/adding-a-language.md`
— use this when one is, not speculatively.

Read `CLAUDE.md`'s "Framework support is additive, not a separate analyzer" section
first for the one-paragraph version; this doc is the concrete how.

## The core idea

A framework component is never a new kind of thing in the IR — it's an ordinary
`Symbol` (`Function`/`Class`/`Const`), and framework support is *extra data*
collected during the same tree-sitter parse pass that already builds the base
symbol/reference graph, not a second parse or a separate analyzer. This matters
because it means:

- Existing reachability/bloat analysis applies to a component unchanged — no
  changes to `spectograph-core` are ever needed.
- Framework-specific usage (a JSX tag, an Angular template binding, a Vue template
  tag) that the *base* analyzer has no vocabulary for still becomes a reachability
  edge, by pushing ordinary `Reference`s into `program.references` — that's the
  single mechanism every framework below uses to make its own usage patterns count.

## Checklist

1. Decide whether the framework needs a **file-format preprocessing step** (§1).
2. Decide **where the shared code lives** — which crate, and why (§2).
3. Write the **detection pass**: interleaved into `collect_globals`, or a separate
   post-pass — depends on what the framework's markers need (§3).
4. Decide **whether a component needs a new symbol minted**, or reuses an existing
   one (§4).
5. Add new `FileResult` fields for whatever the framework's detection produces (§5).
6. Wire **template/usage-scanning reachability edges** as a pass appended after the
   base import-resolution pass in each crate's `analyze_impl` (§6).
7. Wire **mismatch checking** (optional second-order check) (§7).
8. Expose it via a dedicated `analyze_<framework>()` (and, if needed,
   `analyze_<framework>_mismatches()`) method — never bake it into
   `Analyzer::analyze()`'s return type (§8).
9. Write tests — tempdir-inline, `tests/<framework>.rs` (§9).
10. Document known non-goals in the module's own doc comment; update `README.md`/
    `DESIGN.md`/`CLAUDE.md` (§10).

## 1. Does the framework need its own file format?

Two shapes, depending on the answer:

- **No — the framework is embedded directly in ordinary `.js`/`.ts`/`.tsx` source**
  (JSX for React, decorators for Angular). Detection runs straight against the
  file's own tree-sitter tree; no preprocessing needed.
- **Yes — the framework has its own file extension that isn't valid host-language
  syntax on its own** (Vue's `.vue` Single-File Components: `<template>`/
  `<script>`/`<script setup>` blocks). You need a split step first: extract the
  script block(s) and materialize them into a single source string with each line
  pinned to its true position in the original file (blank-padded elsewhere), so the
  ordinary `parse_file` can run against the result *unchanged*, with correct spans.
  See `spectograph_js::vue::parse_sfc`/`materialize_script` for the reference
  implementation, and route the file to whichever analyzer (JS or TS) matches its
  script's own type signal (Vue: the `lang="ts"` attribute) — never guess by the
  container file's own extension, since that's exactly what doesn't tell you the
  host language here.

## 2. Where does the shared code live?

`spectograph-ts` depends on `spectograph-js` (never the reverse — see `CLAUDE.md`),
so:

- If the framework is **TS-only** (Angular): put everything in
  `spectograph-ts/src/<framework>.rs`. No sharing concern.
- If the framework applies to **both JS and TS** (React, Vue): shared types and
  *pure* logic (data structures, naming-convention helpers, string/map-based
  matching that doesn't touch a tree-sitter `Node`) live in
  `spectograph-js/src/<framework>.rs`, re-exported from `spectograph-ts/src/lib.rs`
  (`pub use spectograph_js::<framework>;`) so callers don't need the JS crate as a
  separate explicit dependency. The actual tree-sitter **detection** code is
  **duplicated once per crate**, in each crate's own `parse.rs` — not shared —
  because the JS/TS/TSX grammars produce different node shapes for typed
  constructs (a destructured parameter vs. an interface-typed one, a plain call vs.
  a generic `defineProps<T>()`), and forcing one code path to handle both adds more
  complexity than the duplication costs. When the AST shape genuinely doesn't
  differ between grammars (Vue's template-tag regex scanner, its
  `has_vue_app_mount` bootstrap-call detector), it stays shared even though it
  takes a `tree_sitter::Node` — only duplicate what actually needs to.

## 3. The detection pass: interleaved or separate?

Two patterns, both real, pick based on what the framework's own markers need:

- **Interleaved into `collect_globals`** — when the framework's markers are
  attached to declarations `collect_globals` is walking anyway. Angular's
  decorator/selector/pipe extraction works this way: as each class declaration is
  visited, `extract_angular_selector`/`extract_angular_pipe` are called inline and
  their results pushed into out-parameters (`angular_selectors: &mut Vec<...>`,
  `angular_pipes: &mut Vec<...>`) threaded through the existing walk.
- **A separate post-pass**, called from `parse_file` after `collect_globals`/
  `collect_refs` finish, receiving the already-built `symbols` list. React's
  `detect_react` and Vue's `detect_vue_component` both work this way, for two
  different reasons:
  - React needs the full name index built first, to resolve a JSX-returning
    declaration's name back to the symbol `collect_globals` already created for it
    (`name_to_id: HashMap<String, SymbolId>` built from `symbols`).
  - Vue needs to look at facts that only make sense file-wide — e.g. whether *any*
    `defineProps`/`defineEmits` call exists anywhere at the top level, or whether
    both a plain `<script>` and `<script setup>` block contributed content (see
    §1) — which isn't naturally available mid-walk.

  Prefer the separate-pass style unless the framework's markers are genuinely
  attached to a declaration already being visited; it's simpler to reason about
  and keeps `collect_globals` from growing more out-parameters per framework.

## 4. Reuse an existing symbol, or mint a new one?

- **React**: a component is always an already-named declaration (`function Foo()`,
  `const Foo = () => ...`, `class Foo extends Component`) — `detect_react` looks it
  up in the name index and reuses that `SymbolId`. Never mints anything.
- **Vue**: both the Options API's `export default { ... }` object and a
  `<script setup>` block's implicit component are *anonymous* — there is no
  existing declaration to attach a `ReactComponent`-style struct to. So
  `detect_vue_component` mints a new `Const` symbol (`symbols.push(symbol(id,
  &name, SymbolKind::Const, ...))`), named from an explicit `name:`
  property/`defineOptions` if present, falling back to the file's own name (Vue's
  own devtools convention, and also what makes a same-named default import resolve
  through the ordinary import-fallback path for free).

If the new framework's components are always named declarations, reuse; if they can
be anonymous (a factory call, a default export, an implicit whole-file component),
mint, following Vue's pattern — pick a deterministic fallback name so imports still
resolve without extra plumbing.

## 5. Extend `FileResult`

Add one field per kind of fact the detection pass produces, following the existing
naming convention (`components`/`jsx_usages`/`hook_calls` for React;
`decorators`/`angular_selectors`/`angular_pipes` for Angular;
`vue_component`/`vue_global_registrations`/`vue_async_components`/`vue_app_mount`
for Vue). Thread each new field through `parse_file`'s return and every call site
that constructs a `FileResult` literal — the compiler will find them all.

## 6. Template/usage-scanning reachability edges

If the framework has a rendering artifact the base analyzer can't see into (a JSX
tree, an Angular template, a Vue template) that references *other* components, add
a pass — appended to `analyze_impl`'s fixed pass order, **after** the base
import-resolution pass, since it typically needs already-resolved `ReExport`
references to know what's actually in scope where. The shape, from Angular's
`collect_template_edges`/Vue's `collect_template_edges`:

1. Build a name → target-symbol map (or several, checked in priority order — Vue
   checks per-file local scope, then project-wide global registrations, then a
   canonical-name fallback).
2. Scan the rendering artifact for candidate usage names (a lightweight regex
   scanner is fine here — neither Angular's nor Vue's template scanner is a real
   HTML parser, deliberately; matching *usage*, not validating markup, doesn't need
   one).
3. For each candidate that resolves, push `Reference { from: <using symbol>, to:
   <target>, kind: RefKind::Static, environment: Environment::Universal }`.
4. Leave self-edges in (a recursive component referencing its own tag) — harmless
   for a reachability walk, not worth filtering.

If building these maps takes real work, factor it into a shared private helper
(`build_angular_selectors`, `build_vue_scope_maps`) called from both this pass and
the mismatch-checking method in §7, so the two can't silently drift apart.

## 7. Mismatch checking (optional)

A second-order check: at each usage site, also capture *what was passed* (props,
bindings, attributes) and compare against the target's *declared* shape. Both
Angular's `find_binding_mismatches` and React's `find_prop_mismatches`/Vue's
`find_binding_mismatches` share the same two safety rules — keep both when writing
a new one:

- **Skip framework-reserved names** that can legitimately appear on any usage site
  and are never a real declared prop (React: `key`/`ref`/`children`; Angular:
  `ngIf`/`ngFor`/`ngModel`/etc.; Vue: `key`/`ref`/`class`/`style`).
- **Skip a target whose declared shape isn't statically enumerable at all** (an
  empty declared-props list because nothing could be determined, e.g. `props:
  someExternalVariable`) — an empty list here is indistinguishable from "no props
  declared", and checking it anyway produces nothing but false positives. Only
  check targets where something was actually found.

## 8. Expose it: `analyze_<framework>()`

Add a method on `<Lang>Analyzer` (not a change to `Analyzer::analyze()`'s own
return type — plain non-framework callers must be unaffected) that calls the
crate's private `analyze_impl`, extracts the framework's data from each file's
`FileResult`, and returns `(Program, <Framework>Analysis)`. Two sub-patterns,
either is fine — pick whichever matches how the data will actually be consumed:

- **One method for everything** (Angular's `analyze_angular()` →
  `(Program, Vec<BindingMismatch>)`): fine when there's one obvious consumer.
- **Split by concern** (Vue's `analyze_vue()` → `(Program, VueAnalysis)` for
  components, `analyze_vue_mismatches()` → `(Program, Vec<VueBindingMismatch>)`
  separately): worth it when mismatch-checking needs to rebuild extra scope maps
  (§6) that the plain component-listing consumer doesn't need to pay for.

None of these are wired into `spectograph-cli`'s `check` output yet for any
framework (see `README.md`'s Status) — they're library APIs only. Wiring one in is
a separate task from adding the framework support itself.

## 9. Tests

Tempdir-inline, in `tests/<framework>.rs` in whichever crate(s) support the
framework — see `spectograph-ts/tests/angular.rs` and either crate's
`tests/vue.rs` for the density of coverage expected: naming-convention unit tests,
per-shape detection tests (every syntax variant the framework allows for declaring
a component/its props), template-edge tests (tag-name matching in every form the
templating language allows), mismatch tests (undeclared binding flagged, declared
binding not flagged, target-with-nothing-enumerable skipped), and at least one
full-app end-to-end test proving a component used only through the framework's own
mechanism is reachable while a genuinely unused one is still flagged as bloat.

## 10. Document limitations, then update the other docs

Add a "Known limitations" section to the framework module's own top-level doc
comment — see `spectograph_js::vue`'s for the pattern — listing what's
deliberately out of scope (an older/legacy version of the framework, a
sub-ecosystem like routing or state-management libraries, an edge case in the
templating syntax) so it reads as a decision, not an oversight. Then, per
`CLAUDE.md`'s "Do's and Dont's": update `README.md`'s Status section and
`CLAUDE.md`'s own "Framework support is additive" section to list the new
framework alongside React/Angular/Vue.
