# rowdiet — a static column-tetris linter for Postgres migrations

<!-- badges: added at publish (docs.rs, crates.io — they render as errors while the crates are unpublished) -->

**rowdiet** lints Postgres migration SQL for wasted alignment padding (the *column tetris*
problem) **statically, with no database**. It parses your `CREATE TABLE` / `ALTER TABLE`
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

## Why lint column order?

Postgres stores a row's columns in definition order and inserts invisible padding bytes so each
value starts on its type's alignment boundary (1/2/4/8 — [`typalign`][pgtype]; row layout per
the [storage docs][pgstorage]). A `boolean` before a `bigint` costs 7
dead bytes on **every row**. Reordering columns to descending alignment recovers it: published
fleet-wide runs report 10–21% of total disk ([2ndQuadrant, "On Rocks and Sand"][rocks]: 21%;
[Braintree/PayPal, across 100+ TB][braintree]: ~10%). Fewer bytes per row also means more rows
per 8 kB page, so the win compounds through cache and I/O.

The catch: Postgres cannot reorder columns in place — [the wiki's remedies][colpos] are all
rewrites, and decoupling logical from physical order was [prototyped on pgsql-hackers and
abandoned][lco]. So once a migration is applied the fix costs a table rewrite, which makes
column order a **pre-apply, CI-time** concern — exactly where a static linter fits.

## Highlights

- **Static & zero-DB** — parses migration files; nothing to install in Postgres.
- **Migration-series aware** — folds `CREATE TABLE` + later `ALTER TABLE ADD COLUMN` (and drops,
  renames, type changes) across files in version order (`V1__`, `V1_2__`, timestamps), so it
  lints the table's *final physical order*, not one statement at a time.
- **Two-tier reporting** — byte-exact for fixed-width tables, labeled estimates when varlena is
  involved (see below); it never claims savings that MAXALIGN rounding or varlena
  data-dependence can take away.
- **Embeddable** — a pure-Rust core crate (`rowdiet-core`, wasm32-clean) with a thin CLI; a
  numeric CI gate (`--fail-over`) no other tool offers.
- **Loud degradation** — statements the parser can't handle are skipped *visibly*, and tables
  they touch are flagged incomplete. A linter must never be silently wrong.

## Install & use

```sh
cargo install rowdiet            # or from a checkout: cargo install --path crates/rowdiet
# builds on Rust 1.88+ (rust-version, verified by CI)
cargo rowdiet migrations/        # installs a cargo subcommand too (cargo-rowdiet)
rowdiet migrations/                          # report
rowdiet migrations/ --fail-over 0            # CI gate: exit 1 on any avoidable byte/row
rowdiet migrations/ --fail-over 0 --fail-on-degraded   # also fail when statements were skipped
rowdiet migrations/ --format github          # GitHub Actions annotations
rowdiet migrations/ --format json | jq .     # full structured report
rowdiet - < schema.sql                       # stdin
rowdiet migrations/ --assume-type vector=varlena:d   # teach extension types
rowdiet migrations/ --parser pg-exact        # parse with the real PG17 grammar (libpg_query)
rowdiet migrations/ --baseline rowdiet-baseline.json                    # gate against accepted debt
rowdiet migrations/ --baseline rowdiet-baseline.json --fail-over 0 --update-baseline   # (re)write it
rowdiet migrations/ --baseline rowdiet-baseline.json --accept account   # accept one table's growth
```

Paths can mix directories, individual files, and `-` (stdin). A directory is scanned
recursively for `*.sql` (any case) and expanded in version order; dot-prefixed entries are
skipped (an explicitly passed dot-directory still works), and symlinked directories are not
entered — a symlinked `.sql` file is still read through its link. Explicit file arguments are
analyzed in the order given, never re-sorted — so prefer `rowdiet migrations/` over
`rowdiet migrations/*.sql`: a shell glob expands lexicographically, which puts `V10` before
`V2`. A directory argument that matches no SQL files gets a loud `empty-scan` note rather than
a silent pass.

Exit codes: `0` clean, `1` gate exceeded, `2` operational error. Exempt a deliberate layout with
a `-- rowdiet:ignore` comment inside the `CREATE TABLE` statement — the marker counts only in
comments (never in string literals), and a marker that ends up attached to no statement gets a
note instead of vanishing. Skipped statements, incomplete tables, and empty scans are reported
in the gate summary either way; `--fail-on-degraded` turns them into a failure (recommended
under `--parser pg-exact`, where skips should be zero).

`--format github` emits runner-safe annotations: property values and messages are
workflow-command-escaped, and output respects the runner's 10-annotations-per-severity cap with
a loud suppression notice instead of silent loss. When `$GITHUB_STEP_SUMMARY` is set (any
GitHub Actions job), the full uncapped report is also appended to the job summary.

### Brownfield adoption: the baseline

A zero-tolerance gate is useless on a schema that already carries debt — applied tables are
expensive to rewrite. `--update-baseline` freezes the current state into a reviewed JSON file:
one entry per table over the fail-over, recording its avoidable bytes and a layout signature
(the ordered column storage kinds). From then on, `--baseline` gates **new tables** at the
fail-over, **baselined tables** at their recorded allowance, and reports tables that improved as
ratchet opportunities — tightening is always an explicit act, never automatic.

Allowances stick to the layout, not just the name. `ADD COLUMN` appends, so an append keeps the
allowance alive and only the added waste can fail the gate (`grown since baseline`) — accept it
with `--accept <table>` (a one-entry, reviewable baseline diff) or reorder the columns in the
new migration before it ships. Any other layout change (reorder, drop, type change) expires the
allowance (`modified since baseline`): the table was rewritten anyway, so it either meets the
fail-over or gets re-accepted deliberately.

### As a library (refinery guard, five lines)

```rust
#[test]
fn migrations_are_byte_packed() {
    let analysis = rowdiet_core::fs::analyze_dir("migrations", &Default::default()).unwrap();
    assert!(analysis.notes.is_empty(), "statements were skipped: {:#?}", analysis.notes);
    let worst = analysis.tables.iter().filter(|t| !t.ignored).map(|t| t.avoidable_bytes_per_row).max();
    assert_eq!(worst.unwrap_or(0), 0, "column order wastes bytes: {analysis:#?}");
}
```

The notes assertion matters: a gate that only counts avoidable bytes stays green over statements
the parser could not read.

Flyway users: run the CLI on the migrations directory in CI (Flyway has no non-JVM callback
surface; a Java callback can shell out to `rowdiet` if you want runtime coupling).

## How it reports

Fixed-width columns (int/bigint/timestamp/uuid/bool/…) are **byte-exact from DDL alone** — they
are never TOASTed or compressed. That includes the null-bitmap residue after `DROP COLUMN`:
Postgres keeps dropped attribute slots and stores a NULL for each in every subsequent row, so a
post-drop table's footprint carries a bitmap sized by the original column count (verified
against pageinspect). Varlena columns (text/varchar/numeric/jsonb/bytea/inet/arrays/…)
are stored three data-dependent ways (short form ≤126 B unaligned / long form 4-byte header,
aligned / 18-byte TOAST pointer, unaligned), so their padding cannot be known statically.
rowdiet therefore reports per table:

- **exact tier** (only fixed-width columns): the headline is the **MAXALIGN-rounded footprint
  delta** and rows-per-8kB-page. A reorder that removes padding but doesn't cross an 8-byte rung
  reports **0 avoidable bytes** by design (raw padding is still shown).
- **estimate tier** (any varlena): numbers describe the all-non-NULL, long-form scenario and are
  labeled as such — never guaranteed savings. `varchar(n≤31)` is upgraded to *proven short,
  unaligned* (typmod bounds the payload under the short-varlena limit).

The suggested order is: fixed columns before varlena, alignment descending, irregular-size types
(`timetz`, `macaddr`) at the end of their group, varlenas alignment-descending with proven-short
ones last. For all-regular schemas this provably yields zero padding under any NULL mask.

Non-obvious type facts it models: `uuid` is char-aligned (16 B, never pads);
`inet`/`cidr` are varlena; `numeric(p,s)` is varlena regardless of precision; `char(1)` is
varlena (`bpchar`); an enum value is 4 bytes.

## Limitations

- 64-bit Postgres assumed (`MAXALIGN` 8) — the near-universal case.
- Unknown/extension types default to (varlena, int-aligned), flagged, and teachable via
  `--assume-type` / `Config::assume`. Types defined *in the migration set* (`CREATE TYPE … AS
  ENUM/RANGE/…`, `CREATE DOMAIN`) resolve exactly by replay.
- `ALTER TABLE` against tables created outside the analyzed files is noted, not modeled.
- Migration files are version-ordered per directory. Flyway orders all configured locations
  globally by version; if two directories share one version sequence, merge them (or lint them
  together) so the fold order matches.
- Temporary tables are skipped with a note (session-lived, no storage debt).
- Known sqlparser gaps (`integer ARRAY` keyword form, `LIKE … INCLUDING`) are skipped
  per-statement with a note; `CREATE UNLOGGED TABLE` is handled by keyword strip. The `pg-exact`
  backend (`--parser pg-exact`) parses all of these natively.
- `DO $$…$$` bodies get a best-effort scan on both backends: type-creating DDL behind
  idempotency guards is folded (the common enum-guard pattern); table DDL inside a DO is never
  folded — it surfaces as a conditional-execution note and marks the table incomplete. Dynamic
  `EXECUTE format(...)` is classified by its literal template: partition-creation loops against
  a modeled parent (the hand-rolled hash-partition idiom; pg_partman setups need nothing at all)
  are recognized as layout-inert, dynamic DDL on a concrete table gets a targeted note, and only
  genuinely opaque templates are flagged as not statically analyzable.
- `--suggest` prints a reordered `CREATE TABLE` skeleton; it never rewrites files (editing an
  applied migration breaks Flyway checksums / refinery divergence checks).

## Prior art

- [`pg_column_byte_packer`][packer] — Braintree's Ruby gem from the article above; reorders
  columns at generation time inside ActiveRecord migrations. Ruby-only, generation-side.
- Atlas's `PG110` check — flags inefficient order, but needs a dev database to diff against.
- The `pg_column_tetris` extension — reports from inside a live Postgres install; no CI story.
- pgtableoptimizer.com — a paste-a-table webpage; single-statement, and its numbers blend
  estimates into exact figures (details in the comparison).
- [squawk] — the adjacent Postgres migration linter (locking and downtime rules). No layout
  rules today — a column-order check is an [open request there][squawk-860], where the
  maintainer favors exactly the static approach rowdiet implements.

Feature-by-feature table and dated accuracy notes: [docs/comparison.md](docs/comparison.md).

[squawk-860]: https://github.com/sbdchd/squawk/issues/860

[packer]: https://github.com/braintree/pg_column_byte_packer
[squawk]: https://github.com/sbdchd/squawk

## License

MIT OR Apache-2.0.

[rocks]: https://www.enterprisedb.com/blog/rocks-and-sand
[braintree]: https://medium.com/braintree-product-technology/postgresql-at-scale-saving-space-basically-for-free-d94483d9ed9a
[pgstorage]: https://www.postgresql.org/docs/current/storage-page-layout.html
[pgtype]: https://www.postgresql.org/docs/current/catalog-pg-type.html
[colpos]: https://wiki.postgresql.org/wiki/Alter_column_position
[lco]: https://www.postgresql.org/message-id/flat/20150227182303.GH2384%40alvh.no-ip.org
