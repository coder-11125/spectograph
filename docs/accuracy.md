# Accuracy

The goal of `check` is **precision first**: a 🔴 "guaranteed bloat" finding in a healthy
project should be true. False positives are the thing that makes a tool like this unusable
in CI, so they are measured, not guessed at.

## Methodology

`scripts/bench.py` shallow-clones a fixed set of real open-source projects into
`target/bench` and runs `spectograph check --format json` over each:

```sh
cargo build --release
python3 scripts/bench.py
```

Every 🔴 finding in an actively used project deserves a look. A rising 🔴 count after a
change is a regression signal even when the test suite is green.

## Triaging a finding

1. Open the reported file/line. Is the symbol really unreferenced (grep the repo, check
   package `exports`, framework conventions, templates)?
2. If it is used: reduce the use to a minimal project and add a test **inline** (tempdir
   pattern) in `spectograph-{js,ts}/tests/accuracy.rs` or a focused file. Fix the analyzer,
   never special-case the repo.
3. If it is genuinely unused: it is a true positive — note it, move on.

Bugs found this way so far (most recent first): tsconfig `include: ["."]`/`"./x"` matched
nothing, dropping every `.ts` file of a nested project (vuejs/core `packages-private`);
files excluded by the root tsconfig but included by a nested one were dropped; undeclared attrs
and native listeners on components reported as Vue mismatches (fallthrough); `defineModel()`
not declaring its prop/event; type-level `*.test-d.ts` files and `*-test` dirs not treated as
test roots; entry-module exports reported dead; type-only uses, shorthand
properties, default parameters, `extends`/`implements`, arrow-expression bodies and member
access untracked; `check .` path normalization; nested Vue `<template>` truncation;
tsconfig `include` directory semantics (`"packages/*/src"`); `./x.js` → `x.ts`; default
imports linking to an unrelated same-named `import` in the target module.

## Results (latest run)

| Repo | Language | Symbols | 🔴 | 🟠 | 🟡 | Time |
|---|---|---|---|---|---|---|
| expressjs/express | javascript | 725 | 0 | 0 | 0 | 2.5s |
| tj/commander.js | javascript | 1078 | 0 | 0 | 3 | 0.8s |
| sindresorhus/got | typescript | 2225 | 2 | 0 | 6 | 2.4s |
| shadcn-ui/taxonomy | typescript | 1268 | 97 | 0 | 3 | 0.4s |
| mutoe/vue3-realworld-example-app | typescript | 800 | 5 | 0 | 5 | 0.8s |
| gothinkster/react-redux-realworld-example-app | javascript | 452 | 0 | 0 | 0 | 0.1s |
| vuejs/core | typescript | 10179 | 13 | 0 | 32 | 1.8s |
| facebook/react | javascript | 32290 | 3763 | 174 | 227 | 32.2s |

The two Vue rows were re-measured after the Vue-completeness work (different machine state, so
the times aren't comparable to the other rows); the others are from the earlier run. Of
vue3-realworld's 5 🔴, the 3 `playwright/` page-object methods were checked and are genuinely
unused (defined, never called); the 2 `HttpClient` methods in `src/services/api.ts` were not
audited. vuejs/core's remaining 13 are unused imports/members, also not audited. Both repos
report 0 Vue diagnostics. The `playwright/` files only appear because a nested `tsconfig.json`
now correctly brings them into the analysis.

Notes: taxonomy's 97 are dominated by shadcn/ui component files that no page imports (their
own imports are then dead too) — plausible true positives, not individually audited. The
react number is **not trustworthy** (see below).

## Known limitations

- **Flow syntax.** tree-sitter-javascript cannot parse Flow type annotations. Error
  recovery is slow (≈100% of the 32s on facebook/react is `parse_file`) and findings on
  Flow code are noisy. TypeScript and plain JavaScript are unaffected.
- **Member resolution is by name.** `x.run()` keeps every exported/class member named `run`
  alive. This hides some dead members (precision over recall).
- **Python parsing is single-threaded** (JS/TS parse in parallel). The timings in the table
  above predate parallel parsing and have not been re-measured.
- **Windows** is exercised in CI as experimental only; the end-to-end suite has not been
  run there.
- Dynamic constructs (`require(variable)`, `eval`, reflection) are never traversed and never
  prove anything dead.
