# rowdiet design notes

Distilled from the spike research (three max-effort research axes: parsers/WASM, padding
consistency, prior art; artifacts live outside this repo in the spike bundle). This file records
the decisions the code embodies and the reasoning that must survive refactors.

## Pipeline

```
split (tolerant, hand-rolled)  →  extract (sqlparser 0.62, the ONLY AST-touching module)
      →  fold (replay DDL over a schema model, version order)  →  layout (padding math)
      →  report (tiered honesty)  →  CLI render (text | json | github)
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
label is conservative, not wrong. The spike's research author confirmed this reading on the
bridge (2026-07-23): AXIS 2's "guaranteed component over fixed columns alone" was a hypothetical
for the *clustered* layout — in an interleaved table a fixed column's real offset rides on the
preceding varlena's actual length — so the two-tier shape is a correction that **supersedes the
research letter**, not a compromise against it.

Null bitmap: present per-row only when the row has a NULL, sized by table natts
(`t_hoff 24 → 32` at 9 columns, → 40 at 73). Order-invariant, so it is display information only
(`layout::null_thoff`), never part of reorder advice. The scenario (all non-NULL) uses
`t_hoff = 24`.

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
The report stays honest because both layouts are *computed*, never assumed, and a safety guard
keeps the original order whenever the suggestion doesn't strictly improve the metric.

## Type catalog provenance

`catalog.rs` has two clearly-marked blocks:

1. **Verified in the spike research against `pg_type.dat`** — the ~30 core entries (including
   the lint-loud surprises: `uuid (16,c)`, `timetz (12,d)` irregular, `macaddr (6,i)` irregular,
   `inet`/`cidr` varlena, `numeric` varlena, `char(1)` → varlena bpchar).
2. **Standard `pg_type.dat` values, not re-verified in the spike** — geometric types, `pg_lsn`,
   `tsvector`/`tsquery`, multiranges, `tid`-family omissions. If any is ever found wrong, fix the
   table, not the walk.

Derived rules (all from PG source, cited in the spike): enum → `(4,i)`; domain → base verbatim;
array/range/multirange → varlena, `d` iff element/subtype is `d`, else `i`; composite → `(-1,d)`;
serial family → int type + implicit NOT NULL. `varchar(n≤31)`/`char(n≤31)` are *proven short*
(≤ 4·31+1 = 125 B worst-case UTF-8, under the 127 B short-varlena cap) — stored unaligned.
Bare `char` is `char(1)`; bare `varchar` is unlimited. Quoted `"char"` is the 1-byte type
(catalog key `pgchar`).

Unknown types: (varlena, `i`), flagged, teachable — per `CREATE TYPE`'s documented defaults
(alignment defaults int4; varlena alignment must be ≥ 4). Never default to `d`: that fabricates
waste. No unverified extension entries are hardcoded (curated map is roadmap).

## Folding semantics

- Tables and types are keyed by their **last name component**, lowercased unless quoted
  (search_path resolution is out of scope; collisions across schemas are accepted and
  deterministic).
- `ADD COLUMN` appends — physically true in Postgres. `SET DATA TYPE` edits in place (a type
  change rewrites the table but keeps attnum order). `DROP COLUMN` removes from the model with a
  note that applied rows keep the natts/bitmap residue. Renames tracked; `DROP TABLE` removes;
  redefinition replaces with a note; `IF NOT EXISTS` duplicates are silent no-ops.
- `ADD PRIMARY KEY` (table-level or ALTER) forces NOT NULL on its columns; identity columns are
  implicitly NOT NULL; serial types likewise.
- CTAS (`CREATE TABLE … AS SELECT`) and `LIKE` clauses cannot be resolved statically → note +
  ghost/incomplete.
- `PARTITION OF parent`: the child inherits the parent's modeled columns verbatim (children
  cannot add columns), including the parent's incompleteness; an out-of-set parent leaves the
  child honestly not-modeled. Plain `INHERITS` stays incomplete (inherited-plus-own semantics
  are not modeled).

## Version ordering

`version.rs`: optional `V/U/B` prefix, digit segments separated by `_` or `.` (sub-versions like
`V1718984460_1__` sort between their base and the next version), unversioned names (Flyway
`R__` repeatables) after all versioned, lexicographic tiebreak. Directories are walked
recursively; ordering applies per directory.

## Parser decision (and the libpg_query question, settled)

sqlparser-rs 0.62 is primary: typed AST, Apache governance, wasm32-unknown-unknown-clean
(~400 KB gz measured in the spike). Its known statement-level gaps are contained by the
splitter. `libpg_query` (the real PG parser) was empirically re-verified for wasm shipping at
Sergey's request: it **builds and runs end-to-end** on `wasm32-unknown-emscripten` (374 KB gz +
glue) and `wasm32-wasip1` + wasi-sdk 33 (388 KB gz, runs under wasmtime and Node's WASI), with
PG's sigsetjmp error handling demonstrably working on both — but it is **structurally blocked**
from `wasm32-unknown-unknown` (no libc, no setjmp/longjmp runtime) and wasm-bindgen supports no
other wasm target. So it cannot be the one pure-Rust core behind the planned webpage; it remains
the native differential-test oracle (roadmap), and a two-module hybrid (pganalyze's emscripten
JS package + the Rust core wasm) is the recorded fallback if PG-exact web parsing is ever
required. Full verdict with recipes: `research/libpg-query-wasm-verdict.md` in the spike bundle.

**Decision update (2026-07-23, Sergey — confirmed in-session after driving the researcher
directly):** route 3 is adopted for the web tier — a single Rust-linked `wasm32-wasip1` module
with `libpg_query` behind an off-by-default `pg-exact` feature, WASI preferred over emscripten
for toolchain-ownership reasons. sqlparser-rs remains the default parser and native-primary
path; the differential oracle merges into the `pg-exact` backend's test suite. Plan:
`docs/wasm-plan.md`; rowdiet-owned build recipe + stub headers: `wasm/`.

## Platform assumption

64-bit Postgres: `d` alignment = 8, `MAXALIGN` = 8. The docs hedge that `d` is "by no means all"
machines; a 32-bit knob is deliberately out of scope for v1 and would be a `layout.rs` parameter,
not a redesign.
