//! wasip1 reactor module: JSON in → analysis JSON out behind a minimal C ABI.
//!
//! Built as a *reactor* (cdylib, no `_start`): hosts must instantiate through the WASI shim's
//! `initialize()` path — never `start()` — and call `_initialize` (when exported) before any
//! `rowdiet_*` export.
//!
//! ABI (v1):
//! - `rowdiet_alloc(len) -> ptr` — allocate an input buffer, write UTF-8 JSON into it.
//! - `rowdiet_lint(ptr, len) -> u64` — analyze; returns `(out_ptr << 32) | out_len` (a wasm32
//!   contract — pointers are 32-bit there; JS receives a BigInt: `Number(v >> 32n)` /
//!   `Number(v & 0xffffffffn)`). The output is `out_len` bytes of UTF-8 JSON, unframed. The
//!   input buffer is NOT consumed.
//! - `rowdiet_free(ptr, len)` — free either buffer: the input (its alloc len) and the output
//!   (`out_len`).
//!
//! Loader gotchas: re-derive views from `memory.buffer` AFTER `rowdiet_lint` returns
//! (the parser may grow memory); wrap export calls in try/catch for the shim's `WASIProcExit`.
//!
//! Input JSON: `{"sources": [{"name": "...", "sql": "..."}], "assume": ["vector=varlena:d"],
//! "fail_over": 0, "fail_on_degraded": false, "baseline": {"fail_over": 0, "tables": {"t":
//! {"bytes": 8, "layout": "f4i,f8d"}}}}` (`assume`/`fail_over`/`fail_on_degraded`/`baseline`
//! optional; an explicit `fail_over` overrides the baseline's recorded one; `fail_on_degraded`
//! also fails the gate when statements were skipped, tables are incomplete, or an empty scan
//! was noted). Output JSON
//! mirrors the CLI's `--format json` envelope — the gate outcome under `"gate"` (per-table
//! verdicts, orphaned/expired entries) plus the `"gate_exceeded"` shorthand — with an extra
//! `"parser"` field; errors come back as `{"error": "..."}`.

use rowdiet_core::catalog::parse_assume_spec;
use rowdiet_core::{Baseline, Config, ParserBackend, SqlSource, analyze_sources_with, baseline};

#[derive(serde::Deserialize)]
struct Input {
    sources: Vec<InputSource>,
    #[serde(default)]
    assume: Vec<String>,
    #[serde(default)]
    fail_over: Option<u64>,
    #[serde(default)]
    baseline: Option<Baseline>,
    #[serde(default)]
    fail_on_degraded: bool,
}

#[derive(serde::Deserialize)]
struct InputSource {
    name: String,
    sql: String,
}

/// Analyze input JSON (crate-doc schema) and return the output JSON. Total: every failure —
/// malformed input, a bad assume spec — comes back as `{"error": "…"}`, never a panic, so the
/// ABI needs no separate error channel.
pub fn lint_json(input: &str) -> String {
    match lint(input) {
        Ok(output) => output,
        Err(message) => serde_json::json!({ "error": message }).to_string(),
    }
}

fn lint(input: &str) -> Result<String, String> {
    let input: Input = serde_json::from_str(input).map_err(|e| format!("bad input JSON: {e}"))?;
    let mut config = Config::default();
    for spec in &input.assume {
        let (name, kind) = parse_assume_spec(spec)?;
        config.assume.insert(name, kind);
    }
    let sources: Vec<SqlSource> = input
        .sources
        .into_iter()
        .map(|s| SqlSource {
            name: s.name,
            sql: s.sql,
        })
        .collect();
    let analysis = analyze_sources_with(backend(), &sources, &config);
    let gate = baseline::evaluate(
        &analysis,
        input.fail_over,
        input.fail_on_degraded,
        input.baseline.as_ref(),
    );
    let shown_fail_over = input.fail_over.or_else(|| input.baseline.as_ref().map(|b| b.fail_over));
    let value = serde_json::json!({
        "rowdiet": env!("CARGO_PKG_VERSION"),
        "parser": parser_name(),
        "fail_over": shown_fail_over,
        "gate_exceeded": gate.exceeded,
        "gate": serde_json::to_value(&gate).map_err(|e| e.to_string())?,
        "analysis": serde_json::to_value(&analysis).map_err(|e| e.to_string())?,
    });
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

fn backend() -> ParserBackend {
    #[cfg(feature = "pg-exact")]
    {
        ParserBackend::PgExact
    }
    #[cfg(not(feature = "pg-exact"))]
    {
        ParserBackend::Sqlparser
    }
}

fn parser_name() -> &'static str {
    if cfg!(feature = "pg-exact") {
        "pg-exact"
    } else {
        "sqlparser"
    }
}

/// Allocate a zero-filled `len`-byte input buffer for the host to write UTF-8 JSON into.
/// [`rowdiet_lint`] does not consume it — release it with [`rowdiet_free`] and the same `len`.
#[unsafe(no_mangle)]
pub extern "C" fn rowdiet_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast::<u8>()
}

/// Callers must pass a pointer previously returned by this module together with the exact length
/// it was created with (alloc len for inputs, `out_len` for lint outputs).
#[unsafe(no_mangle)]
pub extern "C" fn rowdiet_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // Reconstructs the exact boxed slice produced by rowdiet_alloc / rowdiet_lint.
    unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))) };
}

/// Free the returned buffer with `rowdiet_free(out_ptr, out_len)`.
///
/// # Safety
/// `ptr..ptr+len` must be a live, initialized buffer in this module's memory (the ABI
/// contract); anything else is host error.
pub unsafe fn lint_raw(ptr: *const u8, len: usize) -> (*mut u8, usize) {
    let input = unsafe { std::slice::from_raw_parts(ptr, len) };
    let output = lint_json(&String::from_utf8_lossy(input))
        .into_bytes()
        .into_boxed_slice();
    let out_len = output.len();
    (Box::into_raw(output).cast::<u8>(), out_len)
}

/// High 32 bits = pointer, low 32 = length. Meaningful on wasm32 only (pointers fit in 32 bits);
/// native callers use [`lint_raw`].
fn pack(ptr: u32, len: u32) -> u64 {
    (u64::from(ptr) << 32) | u64::from(len)
}

/// # Safety
/// Same contract as [`lint_raw`]: the host passes a live buffer previously obtained from
/// `rowdiet_alloc` and filled with the input JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rowdiet_lint(ptr: *const u8, len: usize) -> u64 {
    let (out_ptr, out_len) = unsafe { lint_raw(ptr, len) };
    pack(out_ptr as usize as u32, out_len as u32)
}

#[cfg(test)]
mod tests;
