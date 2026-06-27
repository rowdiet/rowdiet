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

/// Analyze SQL sources in the given order (callers pass migration files version-sorted; see
/// [`version::compare`] and [`fs::analyze_dir`]).
pub fn analyze_sources(sources: &[SqlSource], config: &Config) -> Analysis {
    let mut folder = fold::Folder::new(config.assume.clone());
    for source in sources {
        for raw in split::split(&source.sql) {
            let origin = Origin { source: source.name.clone(), line: raw.line };
            let ignore_marker = raw.text.contains(IGNORE_MARKER);
            let prepared = extract::preprocess(&raw.text);
            match extract::extract(&prepared) {
                Ok(ops) => folder.apply(ops, &origin, ignore_marker),
                Err(error) => folder.skipped(&origin, error, extract::sniff(&raw.text)),
            }
        }
    }
    let (tables, notes) = folder.finish();
    Analysis { tables: tables.into_iter().map(report::build).collect(), notes }
}

#[cfg(test)]
mod tests;
