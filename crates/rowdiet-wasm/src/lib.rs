//! wasip1 reactor module: JSON in → analysis JSON out behind a minimal C ABI.
//!
//! Built as a *reactor* (cdylib, no `_start`): hosts must instantiate through the WASI shim's
//! `initialize()` path — never `start()` — and call `_initialize` (when exported) before any
//! `rowdiet_*` export.
//!
//! ABI (provisional v1, may be revised before the page ships):
//! - `rowdiet_alloc(len) -> ptr` — allocate an input buffer, write UTF-8 JSON into it.
//! - `rowdiet_lint(ptr, len) -> out` — analyze; `out` points at a 4-byte little-endian length
//!   followed by that many UTF-8 JSON bytes. The input buffer is NOT consumed.
//! - `rowdiet_free(ptr, len)` — free any buffer from this module: the input (its alloc len) and
//!   the output (`4 + json_len`).
//!
//! Input JSON: `{"sources": [{"name": "...", "sql": "..."}], "assume": ["vector=varlena:d"],
//! "fail_over": 0}` (`assume`/`fail_over` optional). Output JSON mirrors the CLI's `--format
//! json` envelope, plus a `"parser"` field; errors come back as `{"error": "..."}`.

use rowdiet_core::catalog::parse_assume_spec;
use rowdiet_core::{analyze_sources_with, Config, ParserBackend, SqlSource};

#[derive(serde::Deserialize)]
struct Input {
    sources: Vec<InputSource>,
    #[serde(default)]
    assume: Vec<String>,
    #[serde(default)]
    fail_over: Option<u64>,
}

#[derive(serde::Deserialize)]
struct InputSource {
    name: String,
    sql: String,
}

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
    let gate_exceeded = match input.fail_over {
        Some(limit) => analysis
            .tables
            .iter()
            .any(|t| !t.ignored && t.avoidable_bytes_per_row > limit),
        None => false,
    };
    let value = serde_json::json!({
        "rowdiet": env!("CARGO_PKG_VERSION"),
        "parser": parser_name(),
        "fail_over": input.fail_over,
        "gate_exceeded": gate_exceeded,
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

#[no_mangle]
pub extern "C" fn rowdiet_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}

/// Callers must pass a pointer previously returned by this module together with the exact length
/// it was created with (alloc len for inputs, `4 + json_len` for lint outputs).
#[no_mangle]
pub extern "C" fn rowdiet_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // Reconstructs the exact boxed slice produced by rowdiet_alloc / rowdiet_lint.
    unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))) };
}

#[no_mangle]
pub extern "C" fn rowdiet_lint(ptr: *const u8, len: usize) -> *mut u8 {
    // The pointer/length pair must describe a live buffer in this module's memory (the ABI
    // contract); anything else is host error.
    let input = unsafe { std::slice::from_raw_parts(ptr, len) };
    let output = lint_json(&String::from_utf8_lossy(input));
    let json = output.as_bytes();
    let mut framed = Vec::with_capacity(4 + json.len());
    framed.extend_from_slice(&(json.len() as u32).to_le_bytes());
    framed.extend_from_slice(json);
    Box::into_raw(framed.into_boxed_slice()) as *mut u8
}

#[cfg(test)]
mod tests;
