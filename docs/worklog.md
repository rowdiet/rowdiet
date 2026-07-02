# worklog

## 2026-07-23 — v1 built from the spike handoff

- Repo scaffolded from the rowdiet spike bundle (GO decision, architecture, verified type table,
  build order all inherited from the spike's three-axis research).
- `rowdiet-core`: tolerant splitter, sqlparser-0.62 extraction behind the `DdlOp` boundary,
  version-ordered folding (CREATE/ADD/DROP/RENAME/SET TYPE/SET NOT NULL, session types via
  CREATE TYPE/DOMAIN replay), pg_type-verified alignment catalog with proven-short typmod
  upgrade, exact/estimate two-tier reporting with MAXALIGN footprint honesty and the
  suggested-order heuristic (align desc, irregulars last, varlena tail).
- `rowdiet` CLI: files/dirs/stdin, `--format text|json|github`, `--fail-over` gate (exit 1),
  `--suggest`, `--rows`, `--assume-type`; `-- rowdiet:ignore` exemption.
- 90 tests green (76 core unit, 6 render, 7 CLI integration, 1 doctest); clippy clean;
  `rowdiet-core` checks on `wasm32-unknown-unknown` with `--no-default-features` (the wasm
  boundary holds: fs behind the default `fs` feature, serde optional).
- libpg_query wasm feasibility re-verified empirically on Sergey's mid-session request
  (background agent): prior blanket "not feasible" overturned — builds AND runs on emscripten
  and wasip1 targets (novel result, recipes recorded) — but wasm-bindgen integration is
  structurally blocked, so sqlparser-rs stays primary. Verdict:
  `~/projs/rowdiet-spike/research/libpg-query-wasm-verdict.md`.
- Bridges armed: ukb author session (research context) and bringmeto (first adopter, co-design)
  via `/tmp/agent-bridge/rowdiet/`.
- ukb author signed off post-ship: two-tier honesty confirmed as a correction superseding AXIS 2
  (recorded in design.md), libpg_query verdict framing endorsed, architecture stands.

- Route 3 planned in (Sergey's call, confirmed in-session after driving the wasm researcher
  directly): wasip1 + libpg_query behind a `pg-exact` feature for the web tier; plan in
  docs/wasm-plan.md, proven build recipe + stub headers vendored in wasm/. The native-oracle
  roadmap item merges into the pg-exact differential tests.

- Phase 1 landed same-day (Sergey: "swappable parsers while we are hacking"): `pg-exact` core
  feature (optional pg_query =6.1.1), `extract_pgq` backend mapping the PG17 raw parse tree onto
  the same DdlOp boundary, `ParserBackend` + `analyze_sources_with`, CLI `--parser` flag (CLI
  default-features include pg-exact; lib stays off-by-default). Differential oracle: 30-statement
  corpus op-equality + full-analysis agreement across backends. 106 tests green; the pg-exact
  path parses every known sqlparser gap natively (DO $$, integer ARRAY, UNLOGGED, LIKE INCLUDING,
  PARTITION OF).

- Near-term batch (same day): curated extension types verified from source (pgvector
  vector/halfvec/sparsevec, citext, hstore — none declare ALIGNMENT → int4 default, confirming
  the never-guess-`d` rule); `cargo-rowdiet` subcommand (CLI logic moved to a lib shared by both
  bins); assume-spec parsing moved into core (`catalog::parse_assume_spec`) for wasm reuse.
- First real-corpus validation: bringmeto (11 tables / 4 crates) fully clean — 0 avoidable,
  0 unknown types (vector resolves verified); ukb: 3 real findings (tmp_credentials 4 B/row,
  ledger account 8 B/row, ledger_transaction 4 B/row), partition children honestly flagged
  incomplete, drop-column residue note fired on a real migration. Parser comparison on real DDL:
  identical numbers; sqlparser's only miss is one DO-block skip (pg-exact parses it).

- Phase 2 built (same day): crates/rowdiet-wasm — reactor cdylib (rowdiet_alloc/free/lint,
  provisional length-prefixed ABI) + rowdiet-smoke command bin, wasm/build-wasip1.sh with a
  size-tuned `wasm` profile. Verified under wasmtime: full pipeline with the real PG17 parser in
  wasm (DO-block DDL parses; numbers match native); reactor exports answer via --invoke. Sizes:
  smoke 1.92 MB raw / 388 KB gz (matches spike); cdylib 857 KB gz (wasm-opt pass = Phase-3 TODO).
  Adoption asks with real corpus numbers posted to bringmeto and ukb (cargo-rowdiet plugin).

- ABI finalized same day on the ukb author's verified research (#5): packed-u64 return replaces
  the length prefix; browser_wasi_shim pinned 0.4.2; the five loader gotchas recorded in
  wasm/README.md (initialize-not-start even without _initialize, fresh views after memory.grow,
  WASIProcExit try/catch, go-pgquery = wazero/wasix shape-only precedent, lazy libpg_query init
  verified sufficient single-threaded).

- Phase 3 shipped (same day): web/ static page — vendored shim 0.4.2, loader with all five
  gotchas, dataviz-validated byte rulers (two-slot palette; padding = neutral hatch, alarm lives
  in the headline stat), both tiers verified in Chrome + node headless smoke (7 checks). ADOPTIONS
  LANDED: bringmeto padding gate green (11 tables, dedicated bridge channel live); ukb reordered
  its 3 flagged tables (id-first variant, equally zero-pad), wired the soft-skip cargo-rowdiet
  gate ("34 tables, 0 avoidable" in-gate), documented its canonical column order. Correction
  posted to ukb: partition children still model column-list-incomplete (gate-pass is vacuous
  exactness) — parent-layout inheritance promoted to roadmap.

Next: wasm CI job, wasm-opt/export pruning, partition-child parent-layout inheritance, in-group
exact search, squawk upstream offer.
