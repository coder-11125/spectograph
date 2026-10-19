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

Bugs found this way so far: entry-module exports reported dead; type-only uses, shorthand
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
| mutoe/vue3-realworld-example-app | typescript | 684 | 2 | 0 | 4 | 0.2s |
| gothinkster/react-redux-realworld-example-app | javascript | 452 | 0 | 0 | 0 | 0.1s |
| vuejs/core | typescript | 9734 | 26 | 0 | 25 | 5.0s |
| facebook/react | javascript | 32290 | 3763 | 174 | 227 | 32.2s |

Notes: taxonomy's 97 are dominated by shadcn/ui component files that no page imports (their
own imports are then dead too) — plausible true positives, not individually audited. The
react number is **not trustworthy** (see below).

## Known limitations

- **Flow syntax.** tree-sitter-javascript cannot parse Flow type annotations. Error
  recovery is slow (≈100% of the 32s on facebook/react is `parse_file`) and findings on
  Flow code are noisy. TypeScript and plain JavaScript are unaffected.
- **Member resolution is by name.** `x.run()` keeps every exported/class member named `run`
  alive. This hides some dead members (precision over recall).
- **Parsing is single-threaded.**
- **Windows** is exercised in CI as experimental only; the end-to-end suite has not been
  run there.
- Dynamic constructs (`require(variable)`, `eval`, reflection) are never traversed and never
  prove anything dead.
