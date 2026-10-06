# rowdiet — a static column-tetris linter for Postgres migrations

[![crates.io](https://img.shields.io/crates/v/rowdiet.svg)](https://crates.io/crates/rowdiet) [![docs.rs](https://img.shields.io/docsrs/rowdiet-core)](https://docs.rs/rowdiet-core) [![CI](https://github.com/rowdiet/rowdiet/actions/workflows/ci.yml/badge.svg)](https://github.com/rowdiet/rowdiet/actions/workflows/ci.yml)

**rowdiet** lints Postgres migration SQL for wasted alignment padding (the *column tetris*
problem) **statically, with no database**. It parses your `CREATE TABLE` / `ALTER TABLE`
statements, computes the on-disk row layout Postgres will actually use (postgres column padding,
alignment, and ordering), and reports the bytes per row you can recover by reordering columns —
**before** the migration is applied, while reordering is still free.

**Try it in your browser: [rowdiet.dev](https://rowdiet.dev)** — the same engine compiled to
WebAssembly, with draggable byte rulers; nothing leaves the page.

```
$ rowdiet migrations/ --rows 10000000 --suggest
■ account (V1__init.sql:1) — 6 columns — estimate — gates on deterministic and dominance-proven padding over every storage form, payload length, and NULL; expected values are display-only (short-form, uniform-offset, no-NULL model)
  current  : 14.5 B/row expected padding (13 B deterministic, range 13-16, data-dependent)
  suggested: 0.0 B/row expected padding (0 B deterministic, range 0-1, data-dependent) → 16.0 B/row avoidable (13 B deterministic + 3 B dominance-proven; saves 12-16 B/row in every realization)
  order    : id, balance, flags, kind, active, note
  × 10000000 rows ≈ 120.0 MB to 160.0 MB
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
- **Two-tier reporting** — byte-exact for fixed-width tables; with varlenas involved the gate
  and the advice use deterministic and dominance-proven padding only (see below), so it never
  claims savings that MAXALIGN rounding or varlena data-dependence can take away, and it hands
  you the decision boundary when the winner genuinely depends on your payloads, with a query
  that settles it on your own rows.
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
rowdiet migrations/ --fail-over 0            # CI gate: exit 1 on any avoidable byte/row (fractions allowed)
rowdiet migrations/ --fail-over 0 --fail-on-degraded   # also fail when statements were skipped
rowdiet migrations/ --fail-over 0 --fail-on-budgeted   # also fail when a dominance search hit its budget
rowdiet migrations/ --settle-exact          # print the pageinspect replay under each frontier (superuser)
rowdiet migrations/ --format github          # GitHub Actions annotations
rowdiet migrations/ --format json | jq .     # full structured report
rowdiet - < schema.sql                       # stdin
rowdiet migrations/ --assume-type vector=varlena:d   # teach extension types
rowdiet migrations/ --parser pg-exact        # parse with the real PG17 grammar (libpg_query)
rowdiet migrations/ --baseline rowdiet-baseline.json                    # judge only appended blocks of applied tables
rowdiet migrations/ --baseline rowdiet-baseline.json --fail-over 0 --update-baseline   # (re)write it
rowdiet migrations/ --baseline rowdiet-baseline.json --accept account   # commit one table's appended block
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
under `--parser pg-exact`, where skips should be zero). Tables whose dominance search hit its
budget are counted too: their findings stand, but a clean verdict there covers only the orders
searched, and `--fail-on-budgeted` turns them into a failure. Budgets bind on many ordinary
tables of eight or more columns, which is why that flag is separate.

`--format github` emits runner-safe annotations: property values and messages are
workflow-command-escaped, and output respects the runner's 10-annotations-per-severity cap with
a loud suppression notice instead of silent loss. When `$GITHUB_STEP_SUMMARY` is set (any
GitHub Actions job), the full uncapped report is also appended to the job summary.

### Brownfield adoption: the baseline

A zero-tolerance gate is useless on a schema whose applied tables already carry debt: reordering
them needs a rewrite. `--update-baseline` records each analyzed table's committed layout (the
storage kind of each attribute slot, and the column names for whoever reviews it) in a reviewed
JSON file. From then on a committed layout is never
judged again. What a later migration appends to it with `ADD COLUMN` is: while that migration is
unapplied the block's order is still free, so the gate asks whether the block as written is
dominance-optimal among orders of the block, starting from wherever the committed columns can
leave the offset. When some block order dominates it, the gate fails with
`block not dominance-optimal`, prints the block order to write, and points at the appending
statements; the committed prefix is never reordered. Write the block that way, or accept it as
written with `--accept <table>` (a one-entry, reviewable baseline diff naming the committed
columns), after which the next migration's block is judged against the new prefix. New tables
get the full search against `--fail-over`. `DROP COLUMN` rewrites nothing: PostgreSQL keeps the
dropped attribute's slot, so a committed column dropped since still marks the committed prefix,
and columns added after it are the block. Any other change to a committed column (a type
change, or a table rebuilt in another order) expires the entry (`modified since baseline`), and
the table then has to meet the fail-over or be re-accepted deliberately. Baseline files from
older releases load; their per-table byte allowances are ignored, said so in the output, and
dropped by the next write, and the first `--update-baseline` after upgrading writes an entry
for every table.

### As a library (refinery guard, four lines)

```rust
#[test]
fn migrations_are_byte_packed() {
    let analysis = rowdiet_core::fs::analyze_dir("migrations", &Default::default()).unwrap();
    assert!(analysis.notes.is_empty(), "statements were skipped: {:#?}", analysis.notes);
    assert_eq!(analysis.worst_avoidable(), 0.0, "column order wastes bytes: {analysis:#?}");
}
```

`worst_avoidable()` is the largest avoidable-bytes figure among non-ignored tables;
`Analysis::gated_tables()` iterates exactly the tables the gate considers, for hand-rolled
policies that must not drift from `baseline::evaluate`'s filter.

The notes assertion matters: a gate that only counts avoidable bytes stays green over statements
the parser could not read. Pointing at a directory with no SQL is itself a note — `analyze_dir`
records an `empty_scan` — so the `notes.is_empty()` check above also catches a mistyped path or a
glob that matched nothing. If you narrow that check to specific note kinds, keep `EmptyScan` in the
set, or an empty scan slips through silently.

Discover the directories to scan rather than hardcoding them: a fixed list cannot flag a *new*
`crates/<name>/migrations` that nobody added to it — rowdiet only sees the paths you pass it. Glob
the directories, assert the discovered set is non-empty, then analyze each.

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
  reports **0 avoidable bytes** by design (raw padding is still shown). With nullable columns
  the tier is exact per NULL pattern, and a reorder is recommended only when no row of any NULL
  pattern gets larger.
- **estimate tier** (any varlena): the gate and the reorder advice rest only on
  realization-independent facts. A reorder is recommended when it **dominates** the current
  order (total padding never worse in any storage form, payload length, or NULL pattern,
  strictly better in at least one), and the gated number is the deterministic padding it removes plus the
  dominance-proven worst-case waste, shown with its guaranteed-to-maximum range. When neither
  order dominates — say a text and a `float8[]` competing for the one guaranteed-aligned slot,
  where payload sizes decide the winner — the table shows a **frontier** instead: both orders,
  both worst cases, and the storage-form band each one wins, plus a query that settles it: any
  role that can read the table runs it, it sizes every stored value with `pg_column_size` and
  friends, lays each row out in both orders, and counts the rows each order stores smaller and
  the bytes the switch saves (`--settle-exact` prints a pageinspect replay of the stored bytes
  instead, for a superuser). Frontiers never gate. Expected
  values and ranges are still shown for orientation: the min/max bounds hold for every storage
  form; the expectation is a display-only figure under a stated model (varlena pads scored at
  the short/TOAST form, which stores unaligned; offset residues taken uniform) and decides
  nothing. `varchar(n≤5)` is *proven short, unaligned*: the typmod bounds the payload at 20
  bytes, under both the short-varlena limit and the 24-byte size the toaster needs before it
  compresses anything, so the header form is pinned while the payload length still varies.
  From `varchar(6)` on, a value can pass 20 bytes in 4-byte UTF-8 and a wide row's toaster can
  compress it in line behind an aligned 4-byte header, so it is modeled like any other varlena
  (measured: `varchar(10)` compressed by pglz and `varchar(6)` by lz4 on PostgreSQL 16.15).
  `STORAGE PLAIN` takes the proof away as well: PLAIN does not make the 1-byte header on the way
  in, so a `COPY` or an `UPDATE` stores even a two-character value behind the aligned 4-byte
  header (measured on a PLAIN `varchar(5)`: `(s, c)` pads 2.000 B/row against 0.500 written).
  `--parser pg-exact` reads `STORAGE` on column definitions and in `ALTER COLUMN ... SET
  STORAGE` and models such a column with every header form; the default parser cannot parse the
  clause, skips the statement, and says so.

NULLs count as realizations too. A row that holds NULL in a nullable column stores neither the
value nor its pad, so every later column starts somewhere else in that row: a reorder that is
better when every column is stored can lose bytes in rows with NULLs, and a layout that pads
nothing when every column is stored can pad in them. rowdiet compares orders over every NULL
pattern, prints the rows without NULLs beside the range where they differ, and names the
columns whose NULLs move later offsets; `NOT NULL` removes such a variable, which can turn a
frontier into a proven reorder. A NULL text is a 7-byte text as far as offsets go, so nullable
texts add nothing here; a NULL `numeric` or array is a step no stored value of those types
takes, so they are listed. Fixed-width tables with nullable columns report the row size of
NULL-carrying rows separately, header included: past eight columns the null bitmap takes the
header from 24 to 32 bytes.

The suggested order starts from: fixed columns before varlena, alignment descending,
irregular-size types (`timetz`, `macaddr`) at the end of their group, varlenas
alignment-descending with proven-short ones last. For all-regular schemas this yields zero
padding in every realization under any NULL mask. When the heuristic still pads, an exact
search minimizes deterministic padding and the worst-case bound (within budgets it names in
the output when they bind), and a reorder is recommended only when it dominates the order you
wrote. Where the sweep completes over types whose stored lengths are verified, the verdict is
exhaustive: a clean table there provably has no dominating reorder. Measured on 40,733 random
five-column tables and 6,000 random tables of 6 to 12 columns, the sweep completes for every
table of up to six columns with at most three varlenas, and for seven-column tables with at most
two. Budgeted tables, which say "found" and name the budget: 36% of five-column tables with four
varlenas and all with five, any all-`text` table of five or more columns, 26% of seven-column
tables with three varlenas, and most tables of eight or more columns of mixed fixed types (49 to
97% at eight, 94 to 100% from nine), where the candidate space outgrows the sweep. A type whose
stored lengths depend on the database encoding or were not checked here (`char(n)`, `varchar(n)`
below 134 characters, `inet`, `tsvector`, ranges, composites) turns a completed sweep into
"found" as well, and names the types. The search can beat plain fixed-first packing:
`(text, boolean, bigint)` reorders to `(bigint, text, boolean)`, where the text sits on its
alignment boundary in every storage form and the boolean never pads, reaching zero padding in
every realization.

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
