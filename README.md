# rowdiet — a static column-tetris linter for Postgres migrations

**rowdiet** lints Postgres migration SQL for wasted alignment padding — the *column tetris*
problem — **statically, with no database**. It parses your `CREATE TABLE` / `ALTER TABLE`
statements, computes the on-disk row layout Postgres will actually use (postgres column padding,
alignment, and ordering), and reports the bytes per row you can recover by reordering columns —
**before** the migration is applied, while reordering is still free.

```
$ rowdiet migrations/ --rows 10000000 --suggest
■ account (V1__init.sql:1) — 6 columns — estimate — long-form varlena scenario
  current  : 13 B padding/row (long-form scenario)
  suggested: 1 B padding/row (long-form scenario) → 12 B/row avoidable
  order    : id, balance, flags, kind, active, note
  × 10000000 rows ≈ 120.0 MB
  -- rowdiet suggestion (column order only — re-attach defaults/constraints/options):
  CREATE TABLE account (
      id BIGINT NOT NULL,
      balance BIGINT NOT NULL,
      flags INTEGER NOT NULL,
      kind SMALLINT NOT NULL,
      active BOOLEAN NOT NULL,
      note TEXT
  );
1 table(s) analyzed — 1 with avoidable waste, 0 statement(s) skipped
```

## Why

Postgres stores a row's columns in definition order and inserts invisible padding bytes so each
value starts on its type's alignment boundary (1/2/4/8). A `boolean` before a `bigint` costs 7
dead bytes on **every row**. Reordering columns to descending alignment recovers it — 10–21% of
total disk in published fleet-wide runs ([2ndQuadrant, "On Rocks and Sand"][rocks] shows 21%;
[Braintree/PayPal ran it across 100+ TB][braintree] for ~10%) — and fewer bytes per row also
means more rows per 8 kB page, so the win compounds through cache and I/O.

The catch: Postgres cannot reorder columns in place. After a migration is applied, the fix is a
table rewrite. That makes column order a **pre-apply, CI-time** concern — exactly where a static
linter fits and where existing tools don't reach (Atlas's `PG110` needs a dev database, the
`pg_column_tetris` extension needs a live install and skips `ALTER`, pgtableoptimizer.com is a
closed webpage with no CI story).

## What makes rowdiet different

- **Static & zero-DB** — parses migration files; nothing to install in Postgres.
- **Migration-series aware** — folds `CREATE TABLE` + later `ALTER TABLE ADD COLUMN` (and drops,
  renames, type changes) across files in version order (`V1__`, `V1_2__`, timestamps), so it
  lints the table's *final physical order*, not one statement at a time.
- **Honest numbers** — see the two-tier reporting contract below; it never claims savings that
  MAXALIGN rounding or varlena data-dependence can take away.
- **Embeddable** — a pure-Rust core crate (`rowdiet-core`, wasm32-clean) with a thin CLI; a
  numeric CI gate (`--fail-over`) no other tool offers.
- **Loud degradation** — statements the parser can't handle are skipped *visibly*, and tables
  they touch are flagged incomplete. A linter must never be silently wrong.

## Install & use

```sh
cargo install rowdiet            # or from a checkout: cargo install --path crates/rowdiet
cargo rowdiet migrations/        # installs a cargo subcommand too (cargo-rowdiet)
rowdiet migrations/                          # report
rowdiet migrations/ --fail-over 0            # CI gate: exit 1 on any avoidable byte/row
rowdiet migrations/ --format github          # GitHub Actions annotations
rowdiet migrations/ --format json | jq .     # full structured report
rowdiet - < schema.sql                       # stdin
rowdiet migrations/ --assume-type vector=varlena:d   # teach extension types
rowdiet migrations/ --parser pg-exact        # parse with the real PG17 grammar (libpg_query)
```

Exit codes: `0` clean, `1` gate exceeded, `2` operational error. Exempt a deliberate layout with
a `-- rowdiet:ignore` comment inside the `CREATE TABLE` statement.

### As a library (refinery guard, five lines)

```rust
#[test]
fn migrations_are_byte_packed() {
    let analysis = rowdiet_core::fs::analyze_dir("migrations", &Default::default()).unwrap();
    let worst = analysis.tables.iter().filter(|t| !t.ignored).map(|t| t.avoidable_bytes_per_row).max();
    assert_eq!(worst.unwrap_or(0), 0, "column order wastes bytes: {analysis:#?}");
}
```

Flyway users: run the CLI on the migrations directory in CI — that is the honest integration
(Flyway has no non-JVM callback surface; a Java callback can shell out to `rowdiet` if you want
runtime coupling).

## How it reports (the honesty contract)

Fixed-width columns (int/bigint/timestamp/uuid/bool/…) are **byte-exact from DDL alone** — they
are never TOASTed or compressed. Varlena columns (text/varchar/numeric/jsonb/bytea/inet/arrays/…)
are stored three data-dependent ways (short form ≤126 B unaligned / long form 4-byte header,
aligned / 18-byte TOAST pointer, unaligned), so their padding cannot be known statically.
rowdiet therefore reports per table:

- **exact tier** (only fixed-width columns): the headline is the **MAXALIGN-rounded footprint
  delta** and rows-per-8kB-page. A reorder that removes padding but doesn't cross an 8-byte rung
  honestly reports **0 avoidable bytes** (raw padding is still shown).
- **estimate tier** (any varlena): numbers describe the all-non-NULL, long-form scenario and are
  labeled as such — never guaranteed savings. `varchar(n≤31)` is upgraded to *proven short,
  unaligned* (typmod bounds the payload under the short-varlena limit).

The suggested order is: fixed columns before varlena, alignment descending, irregular-size types
(`timetz`, `macaddr`) at the end of their group, varlenas alignment-descending with proven-short
ones last. For all-regular schemas this provably yields zero padding under any NULL mask.

Surprises it knows about so you don't have to: `uuid` is char-aligned (16 B, never pads);
`inet`/`cidr` are varlena; `numeric(p,s)` is varlena regardless of precision; `char(1)` is
varlena (`bpchar`); an enum value is 4 bytes.

## Limitations (v1)

- 64-bit Postgres assumed (`MAXALIGN` 8) — the near-universal case.
- Unknown/extension types default to (varlena, int-aligned), flagged, and teachable via
  `--assume-type` / `Config::assume`. Types defined *in the migration set* (`CREATE TYPE … AS
  ENUM/RANGE/…`, `CREATE DOMAIN`) resolve exactly by replay.
- `ALTER TABLE` against tables created outside the analyzed files is noted, not modeled.
- Dropped columns are removed from the model; already-applied rows keep paying their
  natts/null-bitmap residue.
- Known sqlparser gaps (`DO $$…$$` bodies, `integer ARRAY` keyword form, `LIKE … INCLUDING`)
  are skipped per-statement with a note; `CREATE UNLOGGED TABLE` is handled by keyword strip.
  The `pg-exact` backend (`--parser pg-exact`) parses all of these natively.
- `--suggest` prints a reordered `CREATE TABLE` skeleton; it never rewrites files (editing an
  applied migration breaks Flyway checksums / refinery divergence checks).

## Status & roadmap

Shipped: **route 3 end to end** (`docs/wasm-plan.md` — the off-by-default `pg-exact` parser
feature doubling as a differential oracle, the Rust-linked `wasm32-wasip1` module with the real
PG17 parser, and the static paste-your-DDL page in `web/`: `./web/build.sh`, then serve `web/`);
partition children inheriting the parent's modeled layout; a source-verified extension map
(pgvector, citext, hstore); an exact minimum-padding search when a table has several
irregular-size columns; the `cargo rowdiet` subcommand; a wasm-opt size pass; and the full local
verification matrix (`scripts/ci.sh` — becomes the CI workflow at publish time).

Remaining: publish-time distribution (crates.io, prebuilt binaries, pre-commit hook, GitHub
Action, hosted webpage), a grown extension map (PostGIS, …), and offering the rule upstream to
squawk once rowdiet has proven out publicly.

## License

MIT OR Apache-2.0.

[rocks]: https://www.enterprisedb.com/blog/rocks-and-sand
[braintree]: https://medium.com/paypal-tech/postgresql-at-scale-saving-space-basically-for-free-d94483d5d725
