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

- AFK follow-up batch (everything not gated on Sergey): partition children now inherit the
  parent's modeled layout (both backends carry PARTITION OF's parent; ukb's h00–h15 analyze for
  real); exact minimum-padding search for multi-irregular fixed blocks (residue-class DP, two
  timetz + int4 → 0 pad); wasm-opt -Oz integrated into build-wasip1.sh (cdylib 857→757 KB gz,
  optimized module re-validated via node smoke); scripts/ci.sh = the full local matrix (fmt,
  clippy -D warnings, tests, no-default builds, wasm32 check, wasip1+smoke — becomes the GH
  workflow at publish); clippy strictness fixes (unsafe fn contracts on the wasm ABI).

- Standalone single-file page (Sergey asked why a server at all): `node web/build-standalone.mjs`
  emits rowdiet-standalone.html (3.7 MB — esbuild-inlined JS + base64-embedded wasm), works from
  file:// double-click; the multi-file page stays the hosted variant (a "server" was only ever a
  static file host — browsers block module imports + wasm fetch on file:// origins).

- Production-scale validation (2026-07-23, agent sweep over two production Kotlin/Flyway
  codebases, corpus kept privately outside this repo): 49 migration dirs / 1,552 files / ~8,400
  statements — zero crashes, zero pg-exact skips, sqlparser skip-rate 0.8% (DO blocks, LIKE
  INCLUDING, a few PG-only ALTERs), and ZERO numeric divergences between backends on every
  shared table. Real findings: 140 tables with avoidable padding across the two schemas (max
  28 B/row on a 55-column table); recurring anti-pattern = enums/booleans interleaved between
  8-byte columns. Web page verified against CLI line-for-line on a partman-grade file; its one
  cosmetic issue (labels illegible on 30+-column bars) fixed same day. Known modeling gap
  promoted to backlog: DDL executed inside DO bodies is invisible to the fold on both backends
  (pg-exact parses the DO silently; sqlparser at least notes the skip).
- Parallel research (agents): domains — every rowdiet TLD unregistered; lean rowdiet.dev +
  rowdiet.com (~$19/yr1), spike research/domains.md. Gradle plugin — GO via Chicory pure-JVM
  wasm (empirical: even the exnref-EH pg-exact module runs; v1 = sqlparser-only wasm compiled
  to classes at build time, Java 11+, zero native artifacts, ~2-4 days), spike
  research/gradle-plugin-verdict.md.

- DO-body DDL visibility closed (Sergey: "if doable without a rabbit hole"): shared best-effort
  scan on both backends — type DDL behind guards folds (fixes the corpus's 3 unknown enum
  types), table DDL becomes a conditional `do-block` note + incomplete flag, dynamic SQL is
  flagged; DML-only DO bodies now silent on both backends (sqlparser's per-DO skip-note noise is
  gone). Full plpgsql semantics deliberately out of scope.

- Corpus extended to a third production codebase (agent): three-repo corpus now 80 dirs /
  1,950 files. Third repo: 120 tables (no table-set drift at all), 69 with shallow avoidable
  padding (max 12 B/row), zero unknown types. Rename-type folding (bf21e45) + DO-body scan
  validated: unknown types across the whole corpus now 0; table-level numbers bit-identical
  across binary versions. FIRST genuine numeric divergence found (reproducer R5 in the private
  corpus results): a column typed with the SQL-standard `ARRAY` suffix on a custom type —
  sqlparser cannot parse the ARRAY keyword form, skips loudly, and reports the table
  natts-short/incomplete while pg-exact models it fully. A textual preprocess is unsafe
  (`ARRAY[...]` constructor in DEFAULTs is token-identical to `type ARRAY[n]`), so the honest
  state stands: pg-exact handles it, sqlparser degrades visibly; the real fix is contributing
  ARRAY-suffix support upstream to sqlparser-rs.

- Repo split for publishing (Sergey's call): this repo is now pure Rust (core + CLI +
  rowdiet-wasm + the wasm recipe — the wasm module stays here because its artifact feeds BOTH
  downstream repos); the page moved to ../rowdiet-web (fresh history, builds the module via
  ROWDIET_REPO, becomes a release-artifact download at publish); ../rowdiet-jvm is the JVM
  umbrella (Sergey's second-thought upgrade: a reusable core library + thin gradle-plugin /
  flyway-callback / maven-plugin adapters, Flyway/ArchUnit-style, instead of a Gradle-only
  plugin); the spike bundle stays put, now under git. ci.sh's wasm leg swapped from the web
  loader smoke to a wasmtime smoke (the loader smoke lives in rowdiet-web).
- sqlparser ARRAY-suffix upstream (agent): NOT reported as an issue; already fixed in open PR
  #2356 (CI-green, unreviewed ~8 weeks) — verified against our production shapes on a local
  branch, two real gaps found (GenericDialect gating, missing custom-type/ALTER tests) with a
  local supplement commit prepared; nothing pushed. Sergey's move: support #2356 with the
  verification + suggested changes rather than filing a duplicate. Detail:
  rowdiet-spike/research/sqlparser-upstream.md.

Next (gated on Sergey): publish-time bundle (remotes for all three repos, CI workflows,
crates.io, prebuilt binaries, pre-commit hook, GH Action, hosted page), rowdiet-jvm
implementation, sqlparser PR #2356 support/review, squawk upstream offer, domain purchases.
Ungated backlog: grow the extension map (PostGIS, …).
