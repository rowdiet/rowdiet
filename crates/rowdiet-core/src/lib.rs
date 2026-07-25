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
//! let migration = SqlSource::new(
//!     "V1__init.sql",
//!     "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);",
//! );
//! let analysis = analyze_sources(&[migration], &Config::default());
//! let table = &analysis.tables[0];
//! assert_eq!(table.current.footprint, Some(56));
//! assert_eq!(table.suggested.footprint, Some(48));
//! assert_eq!(table.avoidable_bytes_per_row, 8);
//! assert_eq!(analysis.worst_avoidable(), 8);
//! ```

pub mod baseline;
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

pub use baseline::{Baseline, BaselineEntry, GateOutcome, TableVerdict};
pub use catalog::AssumedKind;
pub use fold::{Note, NoteKind, Origin};
pub use layout::{Align, ColumnKind, Tier};
pub use report::{Analysis, ColumnReport, OrderStats, TableReport};

use std::collections::BTreeMap;

/// Analysis configuration, shared by every entry point. `Config::default()` assumes nothing.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Storage assumptions for type names the analyzer cannot resolve from DDL (extension types).
    pub assume: BTreeMap<String, AssumedKind>,
}

/// One SQL input — typically a migration file. Callers pass sources already in apply order.
#[derive(Debug, Clone)]
pub struct SqlSource {
    /// Where the SQL came from; appears verbatim in origins, reports, and notes (a path, for
    /// file-based callers).
    pub name: String,
    /// The complete SQL text — any number of statements; rowdiet splits it itself.
    pub sql: String,
}

impl SqlSource {
    /// A source from any pair of string-likes — spares callers the field-by-field `.into()`
    /// dance when the values are `&str`s (tests, adapters, embedded DDL).
    pub fn new(name: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sql: sql.into(),
        }
    }
}

/// A `CREATE TABLE` whose text contains this marker is exempt from the gate (still listed, as
/// ignored); a `DO` statement carrying it is not scanned for DDL at all.
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

/// [`analyze_sources`] with an explicit parser backend.
pub fn analyze_sources_with(backend: ParserBackend, sources: &[SqlSource], config: &Config) -> Analysis {
    let mut folder = fold::Folder::new(config.assume.clone());
    for source in sources {
        // The marker only counts inside comments — a string literal like
        // DEFAULT 'rowdiet:ignore' must never exempt a table. Markers written in the file but
        // attached to no statement (e.g. on their own line above a CREATE, which the splitter
        // consumes as a leading comment) would otherwise vanish silently; they get a note.
        // Every statement is a substring of the file, so one containment miss up front settles
        // the whole file's marker scans.
        let file_has_marker = source.sql.contains(IGNORE_MARKER);
        let mut stranded: Vec<u32> = if file_has_marker {
            split::comment_marker_lines(&source.sql, IGNORE_MARKER)
        } else {
            Vec::new()
        };
        for raw in split::split(&source.sql) {
            let origin = Origin {
                source: source.name.clone(),
                line: raw.line,
            };
            let attached = if file_has_marker {
                split::comment_marker_lines(&raw.text, IGNORE_MARKER)
            } else {
                Vec::new()
            };
            for rel in &attached {
                let abs = raw.line + rel - 1;
                if let Some(pos) = stranded.iter().position(|&l| l == abs) {
                    stranded.remove(pos);
                }
            }
            let ignore_marker = !attached.is_empty();
            if is_do_statement(&raw.text) {
                // The marker waives the body scan entirely — for DO blocks whose dynamic DDL a
                // human has judged irrelevant to layout (e.g. partition-creation loops). Inside
                // the dollar-quoted body the marker is a plpgsql comment, scanned separately.
                let body_marker = file_has_marker
                    && split::dollar_quoted_body(&raw.text)
                        .is_some_and(|body| !split::comment_marker_lines(body, IGNORE_MARKER).is_empty());
                if !ignore_marker && !body_marker {
                    scan_do_block(backend, &mut folder, &raw.text, &origin);
                }
                continue;
            }
            match extract_with(backend, &raw.text) {
                Ok(ops) => folder.apply(ops, &origin, ignore_marker),
                Err(error) => folder.skipped(&origin, error, extract::sniff(&raw.text)),
            }
        }
        for line in stranded {
            folder.unused_ignore_marker(&Origin {
                source: source.name.clone(),
                line,
            });
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
        ParserBackend::Sqlparser => extract::extract(&extract::preprocessed(text)),
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
            FragmentOutcome::Dynamic(ops) => {
                for op in ops {
                    if !dispatch_dynamic_op(folder, op, origin) {
                        unanalyzable += 1;
                    }
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
    /// Ops recovered from a dynamic `EXECUTE [format(]'…'` template — classifiable, never
    /// foldable (names may come from placeholders).
    Dynamic(Vec<extract::DdlOp>),
    Unanalyzable,
}

/// Placeholder stand-in for `%I`/`%s` in dynamic templates. Distinctive on purpose: an op whose
/// target still contains it had a placeholder where the table name goes.
const DYNAMIC_TOKEN: &str = "rowdiet_dyn";

/// plpgsql fragments carry control-flow prefixes (`BEGIN`, `IF … THEN`), so parse from each
/// word-boundary CREATE/ALTER/DROP until one parses. Fragments that resist direct parsing get
/// one more chance as a dynamic `EXECUTE` template: substitute the format placeholders and
/// parse that (`%I`/`%s` need an identifier in name positions but a number in value positions —
/// e.g. hash-partition `REMAINDER %s` — so both substitutions are tried).
fn scan_fragment(backend: ParserBackend, fragment: &str) -> FragmentOutcome {
    // Each anchor costs a full parse of the fragment tail, so unbounded retries are quadratic
    // in keyword count — a hostile 8k-keyword body took 45 s. Real plpgsql fragments carry a
    // handful of DDL keywords; past the cap the fragment is reported not analyzable (loud).
    const MAX_PARSE_ATTEMPTS: u32 = 32;
    let mut attempts = 0u32;
    let mut from = 0usize;
    let mut saw_ddl_keyword = false;
    while let Some(relative) = find_ddl_keyword(&fragment[from..]) {
        saw_ddl_keyword = true;
        if attempts >= MAX_PARSE_ATTEMPTS {
            return FragmentOutcome::Unanalyzable;
        }
        attempts += 1;
        let start = from + relative;
        if let Ok(ops) = extract_with(backend, &fragment[start..]) {
            return FragmentOutcome::Ops(ops);
        }
        from = start + 1;
    }
    if !saw_ddl_keyword {
        return FragmentOutcome::NoDdl;
    }
    if let Some(template) = execute_template(fragment) {
        for token in [DYNAMIC_TOKEN, "0"] {
            let substituted = substitute_format(&template, token);
            if let Ok(ops) = extract_with(backend, &substituted) {
                return FragmentOutcome::Dynamic(ops);
            }
        }
    }
    FragmentOutcome::Unanalyzable
}

/// The first single-quoted literal after a word-boundary `EXECUTE` — the SQL template of the
/// `EXECUTE format('…', …)` / `EXECUTE '…'` idioms ('' unescaped). Concatenation-built dynamic
/// SQL stays unanalyzable.
fn execute_template(fragment: &str) -> Option<String> {
    let b = fragment.as_bytes();
    let mut i = find_word(fragment, b"execute")? + b"execute".len();
    while i < b.len() && b[i] != b'\'' {
        i += 1;
    }
    i += 1;
    let mut template: Vec<u8> = Vec::new();
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                template.push(b'\'');
                i += 2;
            } else {
                return Some(String::from_utf8_lossy(&template).into_owned());
            }
        } else {
            template.push(b[i]);
            i += 1;
        }
    }
    None
}

/// `%I`/`%s` (and `%n$I`/`%n$s`) → `token`, `%L` → `'0'`, `%%` → `%`.
fn substitute_format(template: &str, token: &str) -> String {
    let b = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && b.get(j) == Some(&b'$') {
                j += 1;
            }
            match b.get(j) {
                Some(b'I' | b'i' | b's') => {
                    out.push_str(token);
                    i = j + 1;
                    continue;
                }
                Some(b'L' | b'l') => {
                    out.push_str("'0'");
                    i = j + 1;
                    continue;
                }
                Some(b'%') if j == i + 1 => {
                    out.push('%');
                    i = j + 1;
                    continue;
                }
                _ => {}
            }
        }
        let ch_len = utf8_len(b[i]);
        out.push_str(&template[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Dynamic ops are classified, never folded. Returns false when the op stays unanalyzable.
/// Partition children of a modeled parent are layout-inert (they inherit the parent verbatim),
/// so the ubiquitous `EXECUTE format('CREATE TABLE … PARTITION OF …')` loop earns silence.
fn dispatch_dynamic_op(folder: &mut fold::Folder, op: extract::DdlOp, origin: &Origin) -> bool {
    use extract::DdlOp;
    let concrete = |name: &extract::RawName| !name.key.contains(DYNAMIC_TOKEN);
    match &op {
        DdlOp::CreateTable {
            partition_of: Some(parent),
            ..
        } if concrete(parent) => folder.has_table(&parent.key),
        DdlOp::CreateTable {
            name,
            partition_of: None,
            ..
        } if concrete(name) => {
            dispatch_do_op(folder, op, origin);
            true
        }
        DdlOp::DropTables { names, .. } if names.iter().all(concrete) => {
            dispatch_do_op(folder, op, origin);
            true
        }
        DdlOp::AddColumn { table, .. }
        | DdlOp::DropColumns { table, .. }
        | DdlOp::RenameColumn { table, .. }
        | DdlOp::RenameTable { table, .. }
        | DdlOp::SetColumnType { table, .. }
        | DdlOp::SetNotNull { table, .. }
            if concrete(table) =>
        {
            dispatch_do_op(folder, op, origin);
            true
        }
        DdlOp::Irrelevant => true,
        _ => false,
    }
}

fn find_ddl_keyword(text: &str) -> Option<usize> {
    [b"create".as_slice(), b"alter".as_slice(), b"drop".as_slice()]
        .into_iter()
        .filter_map(|keyword| find_word(text, keyword))
        .min()
}

/// The first word-boundary, case-insensitive occurrence of `word` in `text`.
fn find_word(text: &str, word: &[u8]) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = 0usize;
    while i + word.len() <= b.len() {
        let boundary_before = i == 0 || !split::ident_byte(b[i - 1]);
        let boundary_after = b.get(i + word.len()).is_none_or(|&c| !split::ident_byte(c));
        if boundary_before && boundary_after && b[i..i + word.len()].eq_ignore_ascii_case(word) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn dispatch_do_op(folder: &mut fold::Folder, op: extract::DdlOp, origin: &Origin) {
    use extract::DdlOp;
    match op {
        DdlOp::CreateEnum { .. }
        | DdlOp::CreateComposite { .. }
        | DdlOp::CreateRange { .. }
        | DdlOp::CreateBase { .. }
        | DdlOp::CreateDomain { .. }
        | DdlOp::DropTypes { .. }
        | DdlOp::RenameType { .. } => folder.apply(vec![op], origin, false),
        DdlOp::CreateTable { name, .. } => folder.conditional_table_ddl(&name, "CREATE TABLE", origin),
        DdlOp::AddColumn { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (add column)", origin),
        DdlOp::DropColumns { table, .. } => folder.conditional_table_ddl(&table, "ALTER TABLE (drop column)", origin),
        DdlOp::RenameColumn { table, .. } => {
            folder.conditional_table_ddl(&table, "ALTER TABLE (rename column)", origin);
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
