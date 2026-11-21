# `check --format json` schema (version 1)

The JSON report is a stable contract for CI and for the future `fix` loop. Pinned by
`spectograph-core/tests/report.rs`.

**Compatibility policy:** additive changes (new optional fields) do **not** bump
`schema_version`; removing or retyping a field does. Consumers must ignore unknown fields.

## Top level

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | integer | Always `1` for now. |
| `language` | string | `python`, `javascript` or `typescript` — the language actually analyzed. |
| `total_symbols` | integer | Symbols in the program (external/third-party included). |
| `findings` | array | Bloat findings, sorted deterministically (tier, then file, then line). |
| `diagnostics` | array | Framework mismatches (not bloat). |

## Finding

| Field | Type | Meaning |
|---|---|---|
| `tier` | `guaranteed` \| `probable` \| `high_cost` | See `DESIGN.md` §7. |
| `name` | string | Symbol name (`Class.member` for members). |
| `kind` | string | Symbol kind (`function`, `class`, `import`, `const`, ...). |
| `file` | string | Root-relative path, always `/`-separated. |
| `line` | integer | 1-based start line. |

## Diagnostic

`code`, `message`, `file`, `line`. `code` is one of `react-prop-mismatch`,
`angular-binding-mismatch`, `vue-prop-mismatch`, `vue-event-mismatch`,
`vue-unused-css-class` (a `<style scoped>` class selector no template or script uses;
additive, so `schema_version` stays `1`).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Clean (no finding at or above `--fail-on`; no diagnostics when `--fail-on-diagnostics`). |
| 1 | Findings at or above the tier given by `--fail-on`. |
| 2 | Error (bad path, no sources, unsupported command such as `fix`). |
