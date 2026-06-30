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

Next (per README roadmap): rowdiet-wasm reactor module + webpage (Phases 2–3), curated extension
type map, in-group exact search for multi-irregular tables, squawk upstream offer.
