# How rowdiet compares

The criteria that matter for a column-order tool: does it work statically (no database), does
it understand a migration *series* rather than single statements, is it honest about what is
byte-exact versus estimated, and can it gate CI. Snapshot below verified 2026-07-24;
corrections welcome.

| | static, no DB | migration-series | exact/estimate honesty | CI gate + baseline | embeddable |
|---|---|---|---|---|---|
| **rowdiet** | yes | yes (folds ALTERs in version order) | two tiers, labeled | yes (`--fail-over`, baseline files) | Rust lib, wasm, JVM |
| `pg_column_byte_packer` | yes | generation-time only | n/a (reorders, doesn't report) | no | Ruby/ActiveRecord |
| Atlas `PG110` | no (needs a dev database) | via schema diff | single verdict | warn-level | Atlas pipeline |
| `pg_column_tetris` (extension) | no (runs inside Postgres) | n/a (inspects live catalogs) | reports live layout | no | SQL |
| pgtableoptimizer.com | yes (in browser) | no (first statement only) | none — see notes | no | webpage |
| squawk | yes | per-statement | n/a | yes (no layout rules) | CLI/CI |

## Notes per tool

- **`pg_column_byte_packer`** (Braintree) — reorders columns while *generating* ActiveRecord
  migrations. Complementary: it prevents the problem at authoring time in one ORM; rowdiet
  lints any SQL migration set after the fact.
- **Atlas `PG110`** — flags inefficient order, but needs a dev database to diff against; not a
  static check.
- **`pg_column_tetris`** — reports from inside a live installation. Useful for auditing an
  existing database; no CI story, and `ALTER`-built layouts are out of scope.
- **pgtableoptimizer.com** — a paste-a-table webpage. Model caveats observed as of July 2026:
  every variable-length type (text, varchar, numeric, jsonb) is counted as a fixed 8 bytes and
  presented indistinguishably from exact numbers; `char(n)` is counted as 1 fixed byte (it is
  `bpchar`, a varlena); `timetz` as 8 bytes (it is 12); columns of unrecognized types —
  including `serial` — are silently excluded from the analysis; totals omit the tuple header,
  null bitmap, and final MAXALIGN rounding, so headline savings can overstate the real
  data-area delta. Only the first pasted statement is analyzed. rowdiet reports two tiers
  because varlena padding can't be known statically: estimates are labeled, not blended with
  exact numbers.
- **squawk** — the adjacent Postgres migration linter, focused on locking and downtime safety;
  it has no layout rules today. A column-order check is an open feature request there
  ([sbdchd/squawk#860](https://github.com/sbdchd/squawk/issues/860)), where the maintainer's
  stated preference — compute type sizes statically instead of asking Postgres — is the
  approach rowdiet implements. We intend to offer the rule upstream once rowdiet has proven
  itself in the open.
