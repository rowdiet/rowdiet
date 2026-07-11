//! Static column-tetris analysis for Postgres migration DDL.
//!
//! Postgres stores a row's columns in definition order, padding each value to its type's
//! alignment boundary — a bad order wastes bytes on every row, and the order is only cheap to fix
//! *before* the migration is applied. rowdiet parses `CREATE TABLE` / `ALTER TABLE` statements
//! (no database needed), folds them across a migration series in version order, and reports
//! wasted bytes per row plus a suggested column order.
//!
//! ```
//! use rowdiet_core::{analyze_sources, Config, SqlSource};
//! let migration = SqlSource {
//!     name: "V1__init.sql".into(),
//!     sql: "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);".into(),
//! };
//! let analysis = analyze_sources(&[migration], &Config::default());
//! let table = &analysis.tables[0];
//! assert_eq!(table.current.footprint, Some(56));
//! assert_eq!(table.suggested.footprint, Some(48));
//! assert_eq!(table.avoidable_bytes_per_row, 8);
//! ```

pub mod catalog;
pub mod extract;
#[cfg(feature = "pg-exact")]
pub mod extract_pgq;
pub mod fold;
#[cfg(feature = "fs")]
pub mod fs;
pub mod layout;
pub mod report;
pub mod split;
pub mod version;

pub use catalog::AssumedKind;
pub use fold::{Note, NoteKind, Origin};
pub use layout::{Align, ColumnKind, Tier};
pub use report::{Analysis, ColumnReport, OrderStats, TableReport};

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Storage assumptions for type names the analyzer cannot resolve from DDL (extension types).
    pub assume: BTreeMap<String, AssumedKind>,
}

#[derive(Debug, Clone)]
pub struct SqlSource {
    pub name: String,
    pub sql: String,
}

/// A statement whose text contains this marker is exempt from the gate (still listed, as ignored).
pub const IGNORE_MARKER: &str = "rowdiet:ignore";

/// Which parser produces the `DdlOp` stream. Both feed the identical fold/layout/report pipeline,
/// so backends are swappable per call — useful for hacking and for the differential oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParserBackend {
    /// Pure-Rust sqlparser (the default; the wasm-bindgen-compatible path).
    #[default]
    Sqlparser,
    /// The real PostgreSQL 17 grammar via libpg_query (native / wasip1 targets only).
    #[cfg(feature = "pg-exact")]
    PgExact,
}

/// Analyze SQL sources in the given order (callers pass migration files version-sorted; see
/// [`version::compare`] and [`fs::analyze_dir`]).
pub fn analyze_sources(sources: &[SqlSource], config: &Config) -> Analysis {
    analyze_sources_with(ParserBackend::Sqlparser, sources, config)
}

pub fn analyze_sources_with(backend: ParserBackend, sources: &[SqlSource], config: &Config) -> Analysis {
    let mut folder = fold::Folder::new(config.assume.clone());
    for source in sources {
        for raw in split::split(&source.sql) {
            let origin = Origin {
                source: source.name.clone(),
                line: raw.line,
            };
            let ignore_marker = raw.text.contains(IGNORE_MARKER);
            if is_do_statement(&raw.text) {
                scan_do_block(backend, &mut folder, &raw.text, &origin);
                continue;
            }
            match extract_with(backend, &raw.text) {
                Ok(ops) => folder.apply(ops, &origin, ignore_marker),
                Err(error) => folder.skipped(&origin, error, extract::sniff(&raw.text)),
            }
        }
    }
    let (tables, notes) = folder.finish();
    Analysis {
        tables: tables.into_iter().map(report::build).collect(),
        notes,
    }
}

fn extract_with(backend: ParserBackend, text: &str) -> Result<Vec<extract::DdlOp>, String> {
    match backend {
        ParserBackend::Sqlparser => extract::extract(&extract::preprocess(text)),
        #[cfg(feature = "pg-exact")]
        ParserBackend::PgExact => extract_pgq::extract(text),
    }
}

fn is_do_statement(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("do"))
}

/// Best-effort scan of a `DO $$ … $$` body — full plpgsql semantics are out of scope, but the
/// common migration patterns are statically recoverable: type-creating DDL behind idempotency
/// guards is folded (layout-safe — a column using the type implies it exists), table DDL is
/// surfaced as a conditional-execution note (never folded), and DDL-looking fragments that
/// resist parsing (dynamic `EXECUTE format(...)`) produce one loud summary note.
fn scan_do_block(backend: ParserBackend, folder: &mut fold::Folder, text: &str, origin: &Origin) {
    let Some(body) = split::dollar_quoted_body(text) else {
        folder.do_block_note(
            origin,
            "DO block without a dollar-quoted body — not analyzed".to_string(),
        );
        return;
    };
    let mut unanalyzable = 0u32;
    for fragment in split::split(body) {
        match scan_fragment(backend, &fragment.text) {
            FragmentOutcome::NoDdl => {}
            FragmentOutcome::Ops(ops) => {
                for op in ops {
                    dispatch_do_op(folder, op, origin);
                }
            }
            FragmentOutcome::Unanalyzable => unanalyzable += 1,
        }
    }
    if unanalyzable > 0 {
        let detail = format!("DO block: {unanalyzable} DDL-like fragment(s) not statically analyzable (dynamic SQL?)");
        folder.do_block_note(origin, detail);
    }
}

enum FragmentOutcome {
    NoDdl,
    Ops(Vec<extract::DdlOp>),
    Unanalyzable,
}

/// plpgsql fragments carry control-flow prefixes (`BEGIN`, `IF … THEN`), so parse from each
/// word-boundary CREATE/ALTER/DROP until one parses.
fn scan_fragment(backend: ParserBackend, fragment: &str) -> FragmentOutcome {
    let mut from = 0usize;
    let mut saw_ddl_keyword = false;
    while let Some(relative) = find_ddl_keyword(&fragment[from..]) {
        saw_ddl_keyword = true;
        let start = from + relative;
        if let Ok(ops) = extract_with(backend, &fragment[start..]) {
            return FragmentOutcome::Ops(ops);
        }
        from = start + 1;
    }
    if saw_ddl_keyword {
        FragmentOutcome::Unanalyzable
    } else {
        FragmentOutcome::NoDdl
    }
}

fn find_ddl_keyword(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut best: Option<usize> = None;
    for keyword in [b"create".as_slice(), b"alter".as_slice(), b"drop".as_slice()] {
        let mut i = 0usize;
        while i + keyword.len() <= b.len() {
            let boundary_before = i == 0 || !split::ident_byte(b[i - 1]);
            let boundary_after = b.get(i + keyword.len()).is_none_or(|&c| !split::ident_byte(c));
            if boundary_before && boundary_after && b[i..i + keyword.len()].eq_ignore_ascii_case(keyword) {
                if best.is_none_or(|m| i < m) {
                    best = Some(i);
                }
                break;
            }
            i += 1;
        }
    }
    best
}

fn dispatch_do_op(folder: &mut fold::Folder, op: extract::DdlOp, origin: &Origin) {
    use extract::DdlOp;
    match op {
        DdlOp::CreateEnum { .. }
        | DdlOp::CreateComposite { .. }
        | DdlOp::CreateRange { .. }
        | DdlOp::CreateBase { .. }
        | DdlOp::CreateDomain { .. }
        | DdlOp::DropTypes { .. } => folder.apply(vec![op], origin, false),
        DdlOp::CreateTable { name, .. } => folder.conditional_table_ddl(&name, "CREATE TABLE", origin),
        DdlOp::AddColumn { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (add column)", origin),
        DdlOp::DropColumns { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (drop column)", origin),
        DdlOp::RenameColumn { table, .. } => {
            folder.conditional_table_ddl(&table, "ALTER TABLE (rename column)", origin)
        }
        DdlOp::RenameTable { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (rename)", origin),
        DdlOp::SetColumnType { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (set type)", origin),
        DdlOp::SetNotNull { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (nullability)", origin),
        DdlOp::DropTables { names, .. } => {
            for name in names {
                folder.conditional_table_ddl(&name, "DROP TABLE", origin);
            }
        }
        DdlOp::Irrelevant => {}
    }
}

#[cfg(test)]
mod tests;
