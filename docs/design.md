# rowdiet design notes

The decisions the code embodies and the reasoning that must survive refactors.

## Pipeline

```
split (tolerant, hand-rolled)  →  extract (sqlparser 0.62, the ONLY AST-touching module)
      →  fold (replay DDL over a schema model, version order)  →  layout (padding math)
      →  report (exact | estimate tiers)  →  CLI render (text | json | github)
```

- `split.rs` never fails: it understands PG quoting (dollar quotes with tags, E-string backslash
  escapes, doubled quotes, *nested* block comments) exactly enough to find top-level semicolons
  and 1-based line numbers. Whether a statement parses is decided per statement downstream.
- `extract.rs` is the swappable-parser boundary. If sqlparser's gap rate ever matters on a real
  corpus, `postgresql-cst-parser` (PG17-grammar-generated, ~190 KB gz wasm) slots in behind the
  same `DdlOp` output. Everything after `DdlOp` is parser-agnostic.
- Parse failure ladder: skip loudly (note) → sniff the target (`ALTER TABLE x` → mark table `x`
  incomplete; `CREATE TABLE y` → remember `y` as a ghost so later ALTERs say "its CREATE was
  skipped" instead of "unknown table").
- `DO` bodies (both backends, shared path in `lib.rs`): extract the dollar-quoted body, re-split
  it, parse each fragment from its first word-boundary CREATE/ALTER/DROP. Type-creating DDL is
  folded (idempotency-guard pattern — a column using the type implies it exists; wrong only if
  the guard would have *not* created it, in which case PG itself would have errored). Table DDL
  is never folded: conditional execution is unknowable, so it becomes a `do-block` note +
  incomplete flag. Dynamic `EXECUTE [format(]'…'` fragments get template classification: the
  literal template is parsed with placeholders substituted (`%I`/`%s` tried as identifier and as
  number — hash-partition `REMAINDER %s` needs the latter). `CREATE TABLE … PARTITION OF` a
  modeled parent is layout-inert (children inherit the parent verbatim) and earns silence — this
  covers the ubiquitous hand-rolled partition-creation loop; pg_partman's own
  `create_parent(...)` calls contain no DDL keywords and were always silent. Dynamic DDL against
  a concrete table becomes a targeted conditional note; placeholder-targeted or unparseable
  templates keep the loud summary note. DML-only DO bodies are silent — same as any other DML.
  A `rowdiet:ignore` marker inside a DO waives its scan entirely.

## The scenario model (what the numbers mean)

All row numbers are computed for the **canonical scenario**: every column non-NULL, every
varlena stored long-form, varlena payload bytes excluded from the walk (payloads are unknowable
from DDL and order-invariant; only headers, fixed data, and padding are counted). Proven-short
varlenas contribute a 1-byte header, unaligned.

Tiers:

- **Exact** — table has only fixed-width columns. Padding and footprint are byte-exact
  (fixed-width values are never toasted/compressed). Headline = MAXALIGN-rounded footprint delta;
  `avoidable == 0` whenever the reorder doesn't cross an 8-byte rung, even if raw padding drops.
- **Estimate** — any varlena present. Headline = scenario padding delta, labeled. Real rows
  differ (short-form/TOAST make varlena alignment data-dependent three ways), so no guarantee is
  claimed.

Why no middle tier: even for "fixed prefix + varlena tail preserved" the *total* delta is not
byte-guaranteed — payload lengths shift downstream pads in both orders. There is a useful
provable fact recorded here for a future refinement: with the tail sequence preserved, the
realized recovery is **never negative** (induction over the walk: a running delta `D ≥ 0` before
an item of alignment `a` becomes `D' = D + pad(x+D,a) − pad(x,a) ≥ 0`). So the current Estimate
label is conservative, not wrong. A tempting third metric — "guaranteed padding computed over
the fixed columns alone" — is deliberately absent: it only describes the *clustered* layout,
while in an interleaved table a fixed column's real offset rides on the preceding varlena's
actual length, so presenting that number as guaranteed for the as-written order would overclaim.

Null bitmap: present per-row when the row has a NULL, sized by table natts
(`t_hoff 24 → 32` at 9 columns, → 40 at 73). Order-invariant, so it never changes reorder
advice. The scenario (all non-NULL) uses `t_hoff = 24` — except after `DROP COLUMN`, where the
bitmap is unconditionally present in new rows (dropped attributes are stored as NULL forever),
so the scenario uses `t_hoff = null_thoff(original natts)`; see Folding semantics.

## Suggested order

Sort key: `(fixed=0 | varlena=1 | proven-short=2, alignment desc, irregular-last, original
index)`. Irregulars are fixed types whose size isn't a multiple of their own alignment — exactly
`timetz (12,d)` and `macaddr (6,i)` among built-ins (also `tid (6,s)`); putting them last in
their group keeps every following smaller-alignment column aligned. For all-regular schemas the
result is provably zero-padding under any NULL mask (a subsequence of a desc-aligned regular
sequence is still one). With ≥2 irregulars in one group a sorted order can leave padding an
interposed smaller column would absorb (two `timetz` pad 4 between; `timetz, int4, timetz` is
zero) — when the sorted fixed block still pads, `refine_fixed_block` finds the exact scenario
minimum with a memoized search over (alignment, len mod 8) classes × offset residue (ties prefer
the heuristic's class order; capped at 24 fixed columns / 12 classes, falling back to the sort).
The report never overclaims: both layouts are *computed*, never assumed, and a safety guard
keeps the original order whenever the suggestion doesn't strictly improve the metric.

## Type catalog provenance

`catalog.rs` marks the provenance of its built-in entries in two blocks:

1. **Verified against `pg_type.dat`** — the ~30 core entries (including the lint-loud
   surprises: `uuid (16,c)`, `timetz (12,d)` irregular, `macaddr (6,i)` irregular, `inet`/`cidr`
   varlena, `numeric` varlena, `char(1)` → varlena bpchar).
2. **Standard `pg_type.dat` values, not independently re-verified** — geometric types, `pg_lsn`,
   `tsvector`/`tsquery`, multiranges, `tid`-family omissions. If any is ever found wrong, fix the
   table, not the walk.

Derived rules (all from PostgreSQL source): enum → `(4,i)`; domain → base verbatim;
array/range/multirange → varlena, `d` iff element/subtype is `d`, else `i`; composite → `(-1,d)`;
serial family → int type + implicit NOT NULL. `varchar(n≤31)`/`char(n≤31)` are *proven short*
(≤ 4·31+1 = 125 B worst-case UTF-8, under the 127 B short-varlena cap) — stored unaligned.
Bare `char` is `char(1)`; bare `varchar` is unlimited. Quoted `"char"` is the 1-byte type
(catalog key `pgchar`).

Unknown types: (varlena, `i`), flagged, teachable — per `CREATE TYPE`'s documented defaults
(alignment defaults int4; varlena alignment must be ≥ 4). Never default to `d`: that fabricates
waste. No unverified extension entries are hardcoded; the curated extension entries that do
exist (pgvector, citext, hstore) come from their published `CREATE TYPE` definitions.

## Folding semantics

- **Tables keep their qualification as identity**: the fold key is the full dotted name with
  each part folded by its own quoting (`A.Things` → `a.things`, `a."Things"` → `a.Things`), so
  same-named tables in different schemas are distinct relations. Mixed qualified/unqualified
  references to one relation are not resolved (search_path is out of scope) — qualify
  consistently, as migrations should anyway. **Types** are keyed by their last
  name component (`pg_catalog.int4` resolves as `int4`; the type catalog is unqualified).
- `ADD COLUMN` appends — physically true in Postgres. `SET DATA TYPE` edits in place (a type
  change rewrites the table but keeps attnum order). `DROP COLUMN` removes the column from the
  walk but keeps a dropped-slot count: Postgres retains dropped attributes (attisdropped) and
  stores a NULL for each in every subsequent row, so the exact-tier footprint uses
  `t_hoff = null_thoff(original natts)` once anything was dropped (pageinspect-verified:
  10 int4 columns minus one = 72 B/row, 107 rows/page, not 64/120). Partition children inherit
  the parent's dropped slots. Renames tracked (renaming onto an existing table is noted, never
  silent); `DROP TABLE` removes; redefinition replaces with a note; `IF NOT EXISTS` duplicates
  are silent no-ops.
- `ADD PRIMARY KEY` (table-level or ALTER) forces NOT NULL on its columns; identity columns are
  implicitly NOT NULL; serial types likewise.
- CTAS (`CREATE TABLE … AS SELECT`) and `LIKE` clauses cannot be resolved statically → note +
  ghost/incomplete.
- `PARTITION OF parent`: the child inherits the parent's modeled columns verbatim (children
  cannot add columns), including the parent's incompleteness; an out-of-set parent leaves the
  child not modeled. Plain `INHERITS` stays incomplete (inherited-plus-own semantics
  are not modeled).

## Baseline gate (brownfield adoption)

Real schemas arrive with debt, and applied tables are exactly the ones a linter cannot ask
anyone to rewrite. The baseline file freezes that debt per table so the gate can still be strict
about everything new. Design points, in the order they were decided:

- **Core owns the gate.** The cross-implementation parity of table reports (native CLI, both
  parser backends, the wasm module and everything built on it) is the project's strongest
  correctness property; if wrappers implemented their own gate arithmetic, parity would stop
  covering the pass/fail decision itself. `baseline::evaluate` is the single implementation;
  wrappers only read and write the file.
- **An entry overrides the fail-over; it never joins it.** The effective limit for a baselined
  table is `entry.bytes`, full stop — not `max(fail_over, bytes)`, which would loosen every
  allowance whenever the global gate is relaxed and silently break the ratchet promise.
- **Allowances are pinned to a layout signature, not just a name.** A number-only ceiling is a
  budget: drop 20 B/row of legacy columns, add 18 B/row of new sloppy ones, still "under
  baseline" — while the same new columns would fail on any fresh table. Entries record
  `{bytes, layout}` where `layout` is the ordered resolved-kind sequence (`f{len}{align}` per
  fixed column, `v{align}` + `p` for proven-short varlena, comma-joined: `f8d,f4i,vi,vip`) —
  exactly the inputs of the avoidable computation, so the pin expires precisely when those
  change. Column names and nullability are excluded on purpose: renames and `SET/DROP NOT NULL`
  do not move a single reported byte, so they must not expire an allowance. The signature is
  stored as that readable string rather than a hash: a baseline diff then *shows* what changed,
  and there is no hash-stability liability across releases.
- **Appends keep the allowance alive (the prefix rule).** `ADD COLUMN` appends physically, so
  after one the old signature survives as a comma-boundary prefix of the new one — structurally
  distinguishable from reorders, drops, and type changes. Expiring the allowance there would
  demand a full-table rewrite of an applied table, the very cost this tool exists to avoid; so
  the allowance stays in force and only the appended waste can fail the gate, as
  `grown since baseline` — actionable while the appending migration is still unapplied. All
  non-prefix changes expire the entry (`modified since baseline`) and force a deliberate
  re-accept. (Not every such change paid for a rewrite — `DROP COLUMN` and binary-coercible
  type changes are metadata-only in Postgres — but each is a conscious layout edit, and
  re-accepting is a one-line reviewed diff, so the expiry stays.)
- **Improvements never auto-tighten.** A table now beating its allowance is reported as a
  ratchet opportunity; recording the better number is an explicit maintenance act —
  `--update-baseline` rewrites the whole file from the current analysis, `--accept <table>`
  refreshes exactly one entry (the reviewable one-line diff for accepting one table's growth,
  and the same mechanism prunes an entry once its table comes clean).

The gate also carries degradation: counts of skipped statements and incomplete tables ride in
the outcome (a bytes-only gate would stay green over an unparseable migration set), and
`fail_on_degraded` turns them into a failure — off by default so sqlparser users are not
punished for known parser gaps, cheap to enable under pg-exact where skips should be zero.
Reports and baselines key on the fold key (lowercased unless quoted), which is identical across
parser backends; the as-written spelling is carried separately as `display`. Verdicts per
non-ignored table: `pass`, `new_violation` (no entry, over fail-over),
`regression` (over its allowance), `grown_since_baseline`, `modified_since_baseline`,
`ratchet_opportunity`; entries with no matching table are listed as `orphaned`, expired-but-
passing ones as `expired`. Ignored tables stay outside both gate and baseline.

## Version ordering

`version.rs`: optional `V/U/B` prefix, digit segments separated by `_` or `.` (sub-versions like
`V1718984460_1__` sort between their base and the next version), unversioned names (Flyway
`R__` repeatables) after all versioned, lexicographic tiebreak. Directories are walked
recursively; ordering applies per directory. That per-directory scope is a deliberate
divergence from Flyway, which orders by version *globally* across its locations: on a tree
whose version numbers interleave across sibling subdirectories, rowdiet folds in a different
order than Flyway applies. Timestamp-versioned and year-partitioned layouts are monotonic per
directory, so they are unaffected.

## Discovery policy

The walk (shared verbatim by the CLI and the JVM adapters — the only policy both can honor
identically, since Gradle's input fingerprinting cannot follow links) skips dot-prefixed
entries and does not enter symlinked directories, so a link cycle cannot collect a file once
per traversal depth; a symlinked `.sql` file is still collected and read through its link. An
explicitly passed root is exempt from the dot filter. A scanned path that matches no SQL files
produces an `empty_scan` note and counts as degradation under `fail_on_degraded` — a typo'd
migrations path must not gate green forever having analyzed nothing.

## Parser decision

sqlparser-rs 0.62 is the default parser: typed AST, Apache governance,
wasm32-unknown-unknown-clean (~400 KB gzipped, measured). Its known statement-level gaps are
contained by the splitter. `libpg_query` (the real PG parser) was tested empirically for wasm
shipping: it builds and runs end-to-end on `wasm32-unknown-emscripten` and on `wasm32-wasip1` +
wasi-sdk 33 (~390 KB gzipped, runs under wasmtime and Node's WASI), with PG's sigsetjmp error
handling working on both — but it is structurally blocked from `wasm32-unknown-unknown` (no
libc, no setjmp/longjmp runtime), and wasm-bindgen supports no other wasm target.

The adopted design: the web tier ships a single Rust-linked `wasm32-wasip1` module with
`libpg_query` behind the off-by-default `pg-exact` feature (WASI over emscripten for
toolchain-ownership reasons: frozen ABI, pin-a-tarball toolchain, no rustc↔linker version
pairing). sqlparser-rs remains the default parser and the native-primary path, and the
differential oracle lives in the `pg-exact` backend's test suite. Build recipe + stub headers:
`wasm/`.

## Platform assumption

64-bit Postgres: `d` alignment = 8, `MAXALIGN` = 8. The Postgres docs hedge that `d` means
8 bytes "on many machines, but by no means all"; a 32-bit knob is deliberately out of scope for
v1 and would be a `layout.rs` parameter, not a redesign.
