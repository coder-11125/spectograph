#!/usr/bin/env python3
"""Run `spectograph check` over a fixed set of real open-source projects and print a table.

    cargo build --release
    python3 scripts/bench.py                       # uses target/release/spectograph
    python3 scripts/bench.py path/to/spectograph   # or another binary
    BENCH_DIR=/tmp/bench python3 scripts/bench.py  # where repos are cloned (default target/bench)

Repositories are shallow-cloned once and reused. The point is to measure *false positives*:
every 🔴 "guaranteed bloat" finding in a healthy, actively used project deserves a look, so a
rising count after a change is a regression signal even when the test suite is green. See
docs/accuracy.md for how to triage a finding and for the recorded results.
"""

import json
import os
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

REPOS = [
    # (github path, what it exercises)
    ("expressjs/express", "CommonJS library"),
    ("tj/commander.js", "CommonJS/ESM library"),
    ("sindresorhus/got", "ESM TypeScript library, `./x.js` specifiers"),
    ("shadcn-ui/taxonomy", "Next.js app router, `@/` aliases"),
    ("mutoe/vue3-realworld-example-app", "Vue 3 + TypeScript, `<script setup>`"),
    ("gothinkster/react-redux-realworld-example-app", "Create React App, class components"),
    ("vuejs/core", "large TypeScript monorepo, tsconfig include globs"),
    ("facebook/react", "large Flow-typed JavaScript monorepo"),
]


def clone(repo: str, bench_dir: Path) -> Path:
    dest = bench_dir / repo.split("/")[1]
    if not dest.exists():
        subprocess.run(
            ["git", "clone", "--depth", "1", "--quiet", f"https://github.com/{repo}", str(dest)],
            check=True,
        )
    return dest


def main() -> int:
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/spectograph")
    if not binary.exists():
        print(f"binary not found: {binary} (run `cargo build --release` first)", file=sys.stderr)
        return 2
    bench_dir = Path(os.environ.get("BENCH_DIR", "target/bench"))
    bench_dir.mkdir(parents=True, exist_ok=True)

    rows = []
    for repo, note in REPOS:
        path = clone(repo, bench_dir)
        started = time.monotonic()
        run = subprocess.run(
            [str(binary), "check", str(path), "--format", "json"],
            capture_output=True,
            text=True,
        )
        elapsed = time.monotonic() - started
        if run.returncode >= 2:
            rows.append((repo, "ERROR", run.stderr.strip()[:80], "", "", "", "", "", ""))
            continue
        report = json.loads(run.stdout)
        tiers = Counter(f["tier"] for f in report["findings"])
        rows.append(
            (
                repo,
                report["language"],
                report["total_symbols"],
                tiers["guaranteed"],
                tiers["probable"],
                tiers["high_cost"],
                len(report["diagnostics"]),
                f"{elapsed:.1f}s",
                note,
            )
        )

    header = ("repo", "language", "symbols", "🔴", "🟠", "🟡", "diagnostics", "time", "exercises")
    widths = [max(len(str(r[i])) for r in [header, *rows]) for i in range(len(header))]
    for row in [header, *rows]:
        print("  ".join(str(c).ljust(w) for c, w in zip(row, widths)).rstrip())
    return 0


if __name__ == "__main__":
    sys.exit(main())
