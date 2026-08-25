# GIT.md

Commit message conventions for this repository, extracted from its own history
(`git log`). Follow these when writing a commit message here.

## Title line

`<Imperative verb> <what changed, concretely>` — no period, no conventional-commit
type prefix (`feat:`, `fix:`, `chore:` — one very early commit used `feat(js):`; every
commit since has dropped it, so don't reintroduce it).

Real examples from this repo:

```
Add Vue SFC support to JS/TS analyzers
Fix ES6 import and dotted-filename resolution; add Angular reachability support
Add class component, forwardRef/memo, and namespaced-tag support for React
Extract shared LSP client into spectograph-tsserver; wire up JS resolution
Add TypeScript analyzer with type-aware resolution and CLI wiring
Add runtime-environment detection to JS analyzer; add fmt pre-commit hook
```

Rules that fall out of the actual history:
- Start with a verb: `Add`, `Fix`, `Extract`, `Implement`, `Remove`, `Improve`,
  `Move`, `Isolate`. Name the concrete thing that changed (crate, feature, file) —
  never a vague `Update stuff` or `Misc changes`.
- If the commit bundles a necessary prerequisite fix with the feature it unblocks,
  say both, joined by `;` (`Fix ES6 import and dotted-filename resolution; add
  Angular reachability support`) — don't hide the fix inside a feature-only title,
  and don't split them into two commits if the fix has no standalone reason to exist
  other than unblocking the feature.
- Name real identifiers where it helps (`spectograph-tsserver`, `forwardRef/memo`,
  `CLAUDE.md`) rather than paraphrasing them away.

## Body

Blank line after the title, then prose and/or a `-` bulleted list. Every commit of
real substance in this repo has a body — one-line title-only commits are the
exception (`Reformat pyright.rs to match current rustfmt output`, itself still
explained by two lines of body), not the norm.

**Lead with why, not just what.** The body's job is to carry context a diff can't:
why this shape, what it unblocks, what broke and how it was found. Compare:

> Adds Angular-specific reachability support to the TS analyzer, since Angular wires
> most of an app together through decorator metadata and templates rather than
> ordinary imports and calls:

against a changelog-style "Added AngularSelector struct and collect_template_edges
function" — this repo's commits consistently do the former.

**Bullet style:**
- One bullet per distinct piece of the change, each self-contained.
- Backtick every code identifier: function/type names, file paths, flags
  (`` `analyze_vue_mismatches()` ``, `` `spectograph-js/src/vue.rs` ``,
  `` `--resolution tsserver` ``).
- Use `--` (double hyphen) as an em dash for an inline aside — this repo's commits
  do this constantly (`"...most of a normal Angular app -- isn't flagged as dead
  code"`, `"...the same category of fix as the TS analyzer's Angular DI edges"`).
- Describe the resulting behavior, present tense (`"...are now detected via..."`,
  `"...is layered on top of..."`), not a changelog past-tense list.

**Call out incidental bugs explicitly**, don't bury them in an unrelated bullet.
Say what was broken, how it was noticed, and the fix:

> Fix a real, previously-undiscovered bug: `import_clause` was looked up via
> `child_by_field_name`, but it isn't a named field in the installed grammar, so
> every ES6 `import ... from` binding... silently created no symbol at all. This
> blocked hook imports... from ever resolving; fixed by finding the child by kind
> instead.

**Call out what turned out not to need a fix**, when you went looking and found
nothing wrong — it's a useful record that the case was checked:

> Fragments (`<>...</>`) turned out to already work -- there's no distinct
> `jsx_fragment` node in this grammar, so the existing `jsx_element` check already
> covered them; added a regression test rather than a fix.

**Mention test coverage added**, by file and count, as its own bullet or closing
line:

> Add `spectograph-js/tests/react.rs` (17 tests) covering JSX/component detection,
> prop capture and mismatch detection, hook attribution, and a regression test
> proving hook imports now resolve through the normal reference graph.

> 76 new tests across `spectograph-js/src/vue.rs` and both crates' `tests/vue.rs`.

**Document known limitations or deferred scope** when the commit deliberately
doesn't cover something, so it isn't mistaken for an oversight later:

> Known limitations (Vue 2 quirks, scoped CSS, Vuex/Pinia, Vue Router,
> non-identifier `:is` expressions, `defineEmits<T>()`'s call-signature form) are
> documented in `vue.rs`'s module doc comment.

## What this repo's history does *not* do (early conventions, now abandoned)

The earliest JS/Python commits used a heavier `Phase 1 / Phase 2 / Phase 3` +
`Total Changes:` structure with file-by-file diff summaries and raw test-count
tallies (`f36b470`, `8bf8f1f`, `9086d21`, `1f6d97c`, `b9448ce`). Every commit since
`spectograph-ts` landed has replaced that with the leaner why-first bulleted style
above. Don't revive the `Phase N:` / `Total Changes:` format — the diff already
shows what files changed; the message should carry what the diff can't.

## Mechanics

- Write multi-line messages via `git commit -F -` or a `git commit -m "$(cat <<'EOF'
  ...
  EOF
  )"` heredoc — not repeated `-m` flags, which turn each into a separate paragraph
  with no control over spacing.
- Stage specific files (`git add <path>...`), not `-A`/`.`, so an unrelated
  untracked file never rides along silently.
- One commit per cohesive change. This repo does not squash small related fixes
  into a feature commit's history after the fact, but it also doesn't split a
  feature and the prerequisite fix it needed into two commits — see the title-line
  rule above.
- Never `--amend` a commit that's already been pushed/shared, and never use
  `--no-verify` to skip the `cargo fmt` pre-commit hook (`.githooks/pre-commit`,
  enabled via `git config core.hooksPath .githooks`) — let it reformat and
  re-stage, then commit normally.
