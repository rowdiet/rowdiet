# Route 3 plan: PG-exact parsing + the wasm webpage (wasip1)

**Decision (Sergey, 2026-07-23):** adopt route 3 — a single Rust-linked `wasm32-wasip1` module
carrying `libpg_query` (the real PG17 parser) — for the web tier. Decided after personally
driving the feasibility research; confirmed in-session. WASI was chosen over emscripten for
2026+: frozen ABI, pin-a-tarball toolchain, no rustc↔external-linker version pairing, exnref EH
in all major browsers since 2025. Accepted costs: hand-rolled ~100-line JS loader,
`browser_wasi_shim` dependency, no wasm-bindgen for the web build, wasi-sdk pinned in CI.

What does **not** change: `sqlparser-rs` stays the default parser and the native-primary path —
adopters of `rowdiet-core` never compile C unless they opt in. `extract.rs` already isolates the
parser behind the `DdlOp` boundary; route 3 slots a second backend behind the same boundary.

## Phase 1 — `pg-exact` feature in rowdiet-core — **DONE (2026-07-23)**

Landed same-day at Sergey's request ("swappable parsers while we are hacking"): everything below
is implemented, plus a CLI `--parser sqlparser|pg-exact` flag (the CLI builds with the feature by
default; `--no-default-features` skips the C compile). The differential suite runs a 30-statement
corpus op-for-op and a full-analysis comparison across both backends.

- Optional dependency `pg_query = "=6.1.1"` behind feature `pg-exact` (off by default,
  not in `default`).
- New module `extract_pgq.rs` (feature-gated): libpg_query protobuf AST → the same `DdlOp`
  vocabulary. Everything downstream (fold/layout/report) is untouched.
- Backend selection: `analyze_sources_with(backend, …)` taking a `ParserBackend` enum
  (`Sqlparser` | `PgExact`, the latter only compiled under the feature); `analyze_sources`
  keeps its signature and default.
- **This subsumes the "native oracle" roadmap item**: a differential test suite (native,
  `--features pg-exact`) parses the fixture corpus with both backends and asserts `DdlOp`
  equality. One mapping serves the oracle *and* the web build — divergence between parsers
  becomes a CI failure, and skipped-statement rates become measurable.
- Note: under `pg-exact` the tolerant splitter stays in front (line numbers + per-statement
  degradation); libpg_query itself parses whole scripts, but per-statement feeding preserves
  rowdiet's loud-skip contract and origin tracking.

## Phase 2 — `crates/rowdiet-wasm` (wasip1 reactor module) — **BUILT (2026-07-23)**

Landed: the reactor cdylib + a smoke command-bin, built by `wasm/build-wasip1.sh` (size-tuned
`wasm` cargo profile). Verified under wasmtime: the smoke bin runs the full pipeline with the
real PG17 parser in wasm (DO-block DDL parses, numbers match native), and the reactor's exports
answer via `--invoke`. Sizes: smoke 1.92 MB raw / 388 KB gzip (matches the spike measurement);
cdylib 3.65 MB raw / 857 KB gzip — a wasm-opt / export-pruning pass is a Phase-3 TODO. ABI is
provisional v1 (length-prefixed buffers) pending the ukb author's reactor-ABI research reply;
hosts must use the shim's `initialize()` path, never `start()`. Still open here: pin
`browser_wasi_shim`, CI job. Original plan follows.

- Library module exporting a C ABI: `rowdiet_alloc(len) -> ptr`, `rowdiet_free(ptr, len)`,
  `rowdiet_lint(ptr, len) -> ptr` where input/output are JSON (`{sources, config}` in, the
  serde `Analysis` envelope + gate fields out). Reactor shape, not bin+stdout (per the
  go-pgquery precedent; the bin shape was only the spike's proof vehicle).
- Built with `pg-exact` enabled via the recipe in `wasm/README.md` (stub headers already
  vendored at `wasm/stub-include/`).
- CI job: download the pinned wasi-sdk 33 tarball, build, smoke-run under wasmtime
  (parse-good + parse-error + reparse — the sigsetjmp path must stay covered).
- Pin a `browser_wasi_shim` version here; decide exnref vs legacy-EH browser floor at page
  launch (exnref default; legacy rebuild documented if an older floor is ever needed).

## Phase 3 — the webpage

- Static page loads the Phase 2 module via `browser_wasi_shim` + the hand-rolled loader.
- Paste DDL → per-table byte ruler (padding highlighted), current vs suggested side-by-side,
  bytes/row saved + "×N rows ≈ X MB", copy-reordered-DDL button — showing the exact numbers the
  CI gate enforces, now with PG-exact parsing (UX precedents: pgtableoptimizer.com,
  play.squawkhq.com).
- Perf is a non-issue: wasm parse ≈ 4–5× native (wasilibs datapoint) — irrelevant at
  paste-a-schema scale.

## Sequencing note

Phase 1 is pure Rust and independently valuable (the oracle). Phases 2–3 are deliverable
together. The two-module hybrid (npm `libpg-query` + a wasm-bindgen core) remains documented in
the verdict file as a contingency only — route 3 supersedes it as the chosen path.
