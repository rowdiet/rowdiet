//! Replays extracted DDL over an in-memory schema model, in version order: CREATE TABLE
//! establishes physical column order, ALTER TABLE ADD COLUMN appends (as Postgres does), later
//! type/nullability changes edit in place. Everything the model cannot absorb becomes a [`Note`]
//! — visible degradation, never silent.

use crate::catalog::{AssumedKind, Catalog, Resolved, TypeRef};
use crate::extract::{DdlOp, RawColumn, RawName, Sniff};
use crate::layout::ColumnKind;
use std::collections::{BTreeMap, BTreeSet};

/// Where a statement (or finding) came from — every table and note carries one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Origin {
    /// The [`SqlSource::name`](crate::SqlSource::name) the statement was read from (a path, for
    /// file-based callers).
    pub source: String,
    /// 1-based line of the statement's first content byte; 0 for path-level notes (no line to
    /// point at).
    pub line: u32,
}

/// `source:line` — except for path-level origins (line 0), which print the bare source, since
/// there is no line to point at. The one place that convention lives; renderers should print
/// origins through it rather than re-deciding when the line is real.
impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            f.write_str(&self.source)
        } else {
            write!(f, "{}:{}", self.source, self.line)
        }
    }
}

/// Stable category tag of a [`Note`] — what renderers label and gates count on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum NoteKind {
    /// A statement did not parse; a sniffed ALTER target is flagged incomplete, a sniffed
    /// CREATE leaves its table unanalyzed.
    SkippedStatement,
    /// ALTER/DROP of a table never created in the analyzed set (pre-existing schema?).
    AlterUnknownTable,
    /// ALTER of a table whose CREATE was itself skipped — the alteration cannot be modeled either.
    AlterSkippedTable,
    /// `CREATE TABLE AS SELECT`: column set unknown statically, table not analyzed.
    CtasSkipped,
    /// The declared column list is not the whole table (LIKE / INHERITS / typed table / absent
    /// partition parent).
    IncompleteColumns,
    /// A column drop — every row written afterwards pays for a null bitmap sized by the original
    /// column count.
    DroppedColumn,
    /// A type resolved by the flagged (varlena, int-aligned) fallback; noted once per type name.
    UnknownType,
    /// A CREATE (or a rename landing on an existing name) replaced a table's prior definition.
    Redefined,
    /// A column name occurred twice: within one CREATE (Postgres rejects the statement) or via
    /// ADD COLUMN of an existing column.
    DuplicateColumn,
    /// An ALTER named a column the model does not have.
    UnknownColumn,
    /// A DO-block finding: conditional table DDL, unanalyzable dynamic fragments, or a body that
    /// could not be scanned.
    DoBlockDdl,
    /// A `rowdiet:ignore` marker attached to no statement — it exempts nothing.
    UnusedIgnoreMarker,
    /// A temporary table — session-lived, no storage debt, not analyzed.
    TempTableSkipped,
    /// A scanned path matched no SQL files; see [`Note::empty_scan`].
    EmptyScan,
}

impl NoteKind {
    /// Whether a note of this kind means the analysis is degraded — the analyzer could not fully
    /// account for the input — feeding [`Analysis::degraded`](crate::Analysis::degraded) and the
    /// gate's `--fail-on-degraded`. Incomplete *tables* are tracked by the `incomplete` flag, not
    /// here, so kinds that only mark a table incomplete resolve `false`.
    ///
    /// The match is deliberately WILDCARD-FREE: adding a `NoteKind` must break compilation here so
    /// the new kind gets a keep/drop decision — a `_ =>` arm would swallow it silently and dissolve
    /// the exact enumeration guarantee this exists to provide. The membership is today's shipped
    /// `--fail-on-degraded` set; growing it is a deliberate behavior change, not a convenience edit.
    pub fn is_degradation(self) -> bool {
        match self {
            Self::SkippedStatement | Self::EmptyScan => true,
            Self::AlterUnknownTable
            | Self::AlterSkippedTable
            | Self::CtasSkipped
            | Self::IncompleteColumns
            | Self::DroppedColumn
            | Self::UnknownType
            | Self::Redefined
            | Self::DuplicateColumn
            | Self::UnknownColumn
            | Self::DoBlockDdl
            | Self::UnusedIgnoreMarker
            | Self::TempTableSkipped => false,
        }
    }
}

/// The kind's stable tag — the same word the serde representation carries
/// (`skipped_statement`, `empty_scan`, …), so logs and JSON name note kinds identically.
/// Renderers may still label kinds with their own shorter vocabulary.
impl std::fmt::Display for NoteKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self {
            Self::SkippedStatement => "skipped_statement",
            Self::AlterUnknownTable => "alter_unknown_table",
            Self::AlterSkippedTable => "alter_skipped_table",
            Self::CtasSkipped => "ctas_skipped",
            Self::IncompleteColumns => "incomplete_columns",
            Self::DroppedColumn => "dropped_column",
            Self::UnknownType => "unknown_type",
            Self::Redefined => "redefined",
            Self::DuplicateColumn => "duplicate_column",
            Self::UnknownColumn => "unknown_column",
            Self::DoBlockDdl => "do_block_ddl",
            Self::UnusedIgnoreMarker => "unused_ignore_marker",
            Self::TempTableSkipped => "temp_table_skipped",
            Self::EmptyScan => "empty_scan",
        };
        f.write_str(tag)
    }
}

/// One analysis finding — the loud-degradation channel the module doc promises.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Note {
    /// The statement (or path) the finding points at.
    pub origin: Origin,
    /// Category for renderers and gates.
    pub kind: NoteKind,
    /// Ready-to-print explanation.
    pub detail: String,
}

impl Note {
    /// Path-level note for a scan that matched no SQL files. Line 0 means "no line to point
    /// at" — renderers show the bare path.
    pub fn empty_scan(path: &str) -> Self {
        Self {
            origin: Origin {
                source: path.to_string(),
                line: 0,
            },
            kind: NoteKind::EmptyScan,
            detail: "no SQL files found under this path — nothing was analyzed".to_string(),
        }
    }
}

/// A column after folding: resolved storage class plus the strings reports need.
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedColumn {
    /// Name as written in the DDL.
    pub display: String,
    /// Case-folded name — the identity later ALTERs address.
    pub key: String,
    /// The declared type's spelling, for reports (tracks `SET TYPE`).
    pub type_display: String,
    /// Resolved storage class for the layout walk.
    pub kind: ColumnKind,
    /// False when the type resolved by fallback ([`NoteKind::UnknownType`]).
    pub known_type: bool,
    /// Declared or implied NOT NULL (explicit, PRIMARY KEY, identity, serial).
    pub not_null: bool,
}

/// One table's modeled state after the replay — what [`Folder::finish`] hands to reporting.
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedTable {
    /// Name as written (tracks renames).
    pub display: String,
    /// Fold key: case-folded, qualification kept — the identity ALTERs address and baselines
    /// key on.
    pub key: String,
    /// The statement that created the table (the latest CREATE when redefined).
    pub origin: Origin,
    /// Statements that changed the table after creation (consecutive duplicates collapsed).
    pub altered_in: Vec<Origin>,
    /// The creating statement carried `rowdiet:ignore` — listed, but exempt from gating.
    pub ignored: bool,
    /// The model is known partial: skipped or unexpanded DDL touched this table.
    pub incomplete: bool,
    /// Live columns in physical order.
    pub columns: Vec<FoldedColumn>,
    /// Columns dropped over the series. Postgres keeps dropped attributes (attisdropped), so
    /// every row written after the drop stores a NULL for each — the null bitmap is present
    /// and sized by the ORIGINAL attribute count. Layout math must include that header cost.
    pub dropped_count: usize,
}

/// The replay state machine: feed each statement through [`Self::apply`] (parsed) or
/// [`Self::skipped`] (not), in version order, then [`Self::finish`].
#[derive(Debug)]
pub struct Folder {
    catalog: Catalog,
    order: Vec<String>,
    tables: BTreeMap<String, FoldedTable>,
    ghosts: BTreeSet<String>,
    notes: Vec<Note>,
    unknown_types_noted: BTreeSet<String>,
}

impl Folder {
    /// A folder with an empty model; `assume` seeds the type catalog's user layer.
    pub fn new(assume: BTreeMap<String, AssumedKind>) -> Self {
        Self {
            catalog: Catalog::new(assume),
            order: Vec::new(),
            tables: BTreeMap::new(),
            ghosts: BTreeSet::new(),
            notes: Vec::new(),
            unknown_types_noted: BTreeSet::new(),
        }
    }

    /// Fold one parsed statement's ops, in order. `ignore_marker` says the statement carried
    /// `rowdiet:ignore` — a table it creates is marked ignored.
    pub fn apply(&mut self, ops: Vec<DdlOp>, origin: &Origin, ignore_marker: bool) {
        for op in ops {
            self.apply_one(op, origin, ignore_marker);
        }
    }

    /// Record an unparseable statement: loud note, plus best-effort impact tracking so tables
    /// touched by the skipped DDL are flagged incomplete rather than silently wrong.
    pub fn skipped(&mut self, origin: &Origin, error: String, sniff: Option<Sniff>) {
        let detail = match &sniff {
            Some(Sniff::AlterTable(t)) => format!("statement skipped (targets table {t}): {error}"),
            Some(Sniff::CreateTable(t)) => {
                format!("statement skipped (creates table {t} — table not analyzed): {error}")
            }
            None => format!("statement skipped: {error}"),
        };
        match sniff {
            Some(Sniff::AlterTable(key)) => {
                if let Some(table) = self.tables.get_mut(&key) {
                    table.incomplete = true;
                }
            }
            Some(Sniff::CreateTable(key)) => {
                self.ghosts.insert(key);
            }
            None => {}
        }
        self.note(origin, NoteKind::SkippedStatement, detail);
    }

    /// End the replay: surviving tables in creation order (a redefinition re-positions its
    /// table), plus every note recorded.
    pub fn finish(self) -> (Vec<FoldedTable>, Vec<Note>) {
        let mut tables = self.tables;
        let list = self.order.iter().filter_map(|key| tables.remove(key)).collect();
        (list, self.notes)
    }

    fn apply_one(&mut self, op: DdlOp, origin: &Origin, ignore_marker: bool) {
        match op {
            DdlOp::CreateTable {
                name,
                columns,
                pk_columns,
                if_not_exists,
                is_ctas,
                incomplete_columns,
                temporary,
                partition_of,
            } => {
                if temporary {
                    self.temp_table_skipped(&name.display, origin);
                } else {
                    self.create_table(
                        name,
                        columns,
                        pk_columns,
                        if_not_exists,
                        is_ctas,
                        incomplete_columns,
                        partition_of,
                        origin,
                        ignore_marker,
                    );
                }
            }
            DdlOp::AddColumn {
                table,
                column,
                if_not_exists,
            } => self.add_column(table, column, if_not_exists, origin),
            DdlOp::DropColumns {
                table,
                columns,
                if_exists,
            } => self.drop_columns(table, columns, if_exists, origin),
            DdlOp::RenameColumn { table, old, new } => self.rename_column(table, old, new, origin),
            DdlOp::RenameTable { table, new } => self.rename_table(table, new, origin),
            DdlOp::SetColumnType {
                table,
                column,
                type_ref,
            } => self.set_column_type(table, column, type_ref, origin),
            DdlOp::SetNotNull { table, column, value } => self.set_not_null(table, column, value, origin),
            DdlOp::DropTables { names, if_exists } => self.drop_tables(names, if_exists, origin),
            DdlOp::CreateEnum { name } => self.catalog.define_enum(name.key),
            DdlOp::CreateComposite { name } => self.catalog.define_composite(name.key),
            DdlOp::CreateRange { name, subtype } => self.catalog.define_range(name.key, subtype.as_ref()),
            DdlOp::CreateBase { name } => self.catalog.define_shell(name.key),
            DdlOp::CreateDomain { name, base } => self.catalog.define_domain(name.key, &base),
            DdlOp::DropTypes { names } => {
                for name in names {
                    self.catalog.drop_type(&name.key);
                }
            }
            DdlOp::RenameType { name, new } => self.catalog.rename_type(&name.key, new.key),
            DdlOp::Irrelevant => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_table(
        &mut self,
        name: RawName,
        columns: Vec<RawColumn>,
        pk_columns: Vec<String>,
        if_not_exists: bool,
        is_ctas: bool,
        incomplete_columns: bool,
        partition_of: Option<RawName>,
        origin: &Origin,
        ignore_marker: bool,
    ) {
        if is_ctas {
            let detail = format!(
                "CREATE TABLE {} AS SELECT — column set unknown statically, table not analyzed",
                name.display
            );
            self.note(origin, NoteKind::CtasSkipped, detail);
            self.ghosts.insert(name.key);
            return;
        }
        if self.tables.contains_key(&name.key) {
            if if_not_exists {
                return;
            }
            self.note(
                origin,
                NoteKind::Redefined,
                format!("table {} redefined — replacing prior definition", name.display),
            );
            self.order.retain(|key| key != &name.key);
        }
        if incomplete_columns {
            let detail = format!(
                "table {}: column list incomplete (LIKE / INHERITS / typed-table clause not expanded)",
                name.display
            );
            self.note(origin, NoteKind::IncompleteColumns, detail);
        }
        // A partition child's physical layout is its parent's, verbatim (children cannot add
        // columns) — inherit the parent's modeled columns when the parent is in the set.
        let mut incomplete = incomplete_columns;
        let mut inherited: Vec<FoldedColumn> = Vec::new();
        let mut inherited_dropped = 0usize;
        if let Some(parent) = &partition_of {
            match self.tables.get(&parent.key) {
                Some(parent_table) => {
                    inherited.clone_from(&parent_table.columns);
                    // Children share the parent's attribute numbering, dropped slots included.
                    inherited_dropped = parent_table.dropped_count;
                    incomplete = incomplete || parent_table.incomplete;
                }
                None => {
                    let detail = format!(
                        "table {}: PARTITION OF {} — parent not in the analyzed set, child not modeled",
                        name.display, parent.display
                    );
                    self.note(origin, NoteKind::IncompleteColumns, detail);
                    incomplete = true;
                }
            }
        }
        let mut table = FoldedTable {
            display: name.display,
            key: name.key.clone(),
            origin: origin.clone(),
            altered_in: Vec::new(),
            ignored: ignore_marker,
            incomplete,
            columns: inherited,
            dropped_count: inherited_dropped,
        };
        let mut seen: std::collections::HashSet<String> = table.columns.iter().map(|c| c.key.clone()).collect();
        for raw in columns {
            let mut column = self.resolve_column(raw, origin);
            if pk_columns.contains(&column.key) {
                column.not_null = true;
            }
            // Postgres rejects the whole statement over a repeated name, so a layout modeled
            // from it describes no applyable table: keep the first occurrence, say so, and
            // flag the table so `--fail-on-degraded` can gate on it.
            if !seen.insert(column.key.clone()) {
                let detail = format!(
                    "table {}: column {} specified more than once — Postgres rejects this \
                     statement; first occurrence kept",
                    table.display, column.display
                );
                self.note(origin, NoteKind::DuplicateColumn, detail);
                table.incomplete = true;
                continue;
            }
            table.columns.push(column);
        }
        self.ghosts.remove(&name.key);
        self.order.push(name.key.clone());
        self.tables.insert(name.key, table);
    }

    fn add_column(&mut self, table: RawName, column: RawColumn, if_not_exists: bool, origin: &Origin) {
        if !self.require_table(&table, origin) {
            return;
        }
        let duplicate = self.tables[&table.key].columns.iter().any(|c| c.key == column.key);
        if duplicate {
            if !if_not_exists {
                let detail = format!(
                    "ALTER TABLE {} ADD COLUMN {}: column already exists",
                    table.display, column.display
                );
                self.note(origin, NoteKind::DuplicateColumn, detail);
            }
            return;
        }
        let folded = self.resolve_column(column, origin);
        let entry = self.tables.get_mut(&table.key).expect("checked above");
        entry.columns.push(folded);
        mark_altered(entry, origin);
    }

    fn drop_columns(&mut self, table: RawName, columns: Vec<String>, if_exists: bool, origin: &Origin) {
        if !self.require_table(&table, origin) {
            return;
        }
        for column in columns {
            let entry = self.tables.get_mut(&table.key).expect("checked above");
            let existed = entry.columns.iter().any(|c| c.key == column);
            if existed {
                entry.columns.retain(|c| c.key != column);
                entry.dropped_count += 1;
                mark_altered(entry, origin);
                let detail = format!(
                    "table {}: column {column} dropped — Postgres keeps the attribute slot, so \
                     every row written from now on carries a null bitmap sized by the original \
                     column count (included in the footprint below)",
                    table.display
                );
                self.note(origin, NoteKind::DroppedColumn, detail);
            } else if !if_exists {
                self.unknown_column(&table, &column, origin);
            }
        }
    }

    fn rename_column(&mut self, table: RawName, old: String, new: String, origin: &Origin) {
        if !self.require_table(&table, origin) {
            return;
        }
        let entry = self.tables.get_mut(&table.key).expect("checked above");
        match entry.columns.iter_mut().find(|c| c.key == old) {
            Some(column) => {
                column.key.clone_from(&new);
                column.display = new;
                mark_altered(entry, origin);
            }
            None => self.unknown_column(&table, &old, origin),
        }
    }

    fn rename_table(&mut self, table: RawName, new: RawName, origin: &Origin) {
        if !self.require_table(&table, origin) {
            return;
        }
        // Postgres would reject a rename onto an existing name; replaying it here replaces the
        // target, which must not happen in silence.
        if new.key != table.key && self.tables.contains_key(&new.key) {
            self.note(
                origin,
                NoteKind::Redefined,
                format!(
                    "rename of {} to {} replaces an existing table of that name (Postgres would \
                     reject this rename)",
                    table.display, new.display
                ),
            );
            self.order.retain(|key| key != &new.key);
        }
        let mut entry = self.tables.remove(&table.key).expect("checked above");
        entry.key.clone_from(&new.key);
        entry.display = new.display;
        mark_altered(&mut entry, origin);
        for slot in &mut self.order {
            if slot == &table.key {
                slot.clone_from(&new.key);
            }
        }
        self.tables.insert(new.key, entry);
    }

    fn set_column_type(&mut self, table: RawName, column: String, type_ref: TypeRef, origin: &Origin) {
        if !self.require_table(&table, origin) {
            return;
        }
        let resolved = self.resolve_type(&type_ref, origin);
        let entry = self.tables.get_mut(&table.key).expect("checked above");
        match entry.columns.iter_mut().find(|c| c.key == column) {
            Some(col) => {
                col.kind = resolved.kind;
                col.known_type = resolved.known;
                col.type_display = type_ref.display;
                mark_altered(entry, origin);
            }
            None => self.unknown_column(&table, &column, origin),
        }
    }

    fn set_not_null(&mut self, table: RawName, column: String, value: bool, origin: &Origin) {
        if !self.require_table(&table, origin) {
            return;
        }
        let entry = self.tables.get_mut(&table.key).expect("checked above");
        match entry.columns.iter_mut().find(|c| c.key == column) {
            Some(col) => {
                col.not_null = value;
                mark_altered(entry, origin);
            }
            None => self.unknown_column(&table, &column, origin),
        }
    }

    fn drop_tables(&mut self, names: Vec<RawName>, if_exists: bool, origin: &Origin) {
        for name in names {
            let existed = self.tables.remove(&name.key).is_some();
            self.order.retain(|key| key != &name.key);
            let was_ghost = self.ghosts.remove(&name.key);
            if !existed && !was_ghost && !if_exists {
                let detail = format!("DROP TABLE {} — table not created in the analyzed set", name.display);
                self.note(origin, NoteKind::AlterUnknownTable, detail);
            }
        }
    }

    /// True if the table is in the model; otherwise records why it is not analyzable.
    fn require_table(&mut self, table: &RawName, origin: &Origin) -> bool {
        if self.tables.contains_key(&table.key) {
            return true;
        }
        if self.ghosts.contains(&table.key) {
            let detail = format!(
                "ALTER TABLE {} — its CREATE was skipped, table not analyzed",
                table.display
            );
            self.note(origin, NoteKind::AlterSkippedTable, detail);
        } else {
            let detail = format!(
                "ALTER TABLE {} — table not created in the analyzed set (pre-existing schema?), appended columns not modeled",
                table.display
            );
            self.note(origin, NoteKind::AlterUnknownTable, detail);
        }
        false
    }

    /// True if `key` (a fold key) currently names a modeled table.
    pub fn has_table(&self, key: &str) -> bool {
        self.tables.contains_key(key)
    }

    /// Table DDL found inside a DO body: execution is conditional, so it is never folded — the
    /// table (when known) is marked incomplete and the finding surfaces as a note.
    pub fn conditional_table_ddl(&mut self, table: &RawName, verb: &str, origin: &Origin) {
        if let Some(entry) = self.tables.get_mut(&table.key) {
            entry.incomplete = true;
        }
        let detail = format!("DO block: {verb} {} — conditional execution, not folded", table.display);
        self.note(origin, NoteKind::DoBlockDdl, detail);
    }

    /// Record a [`NoteKind::DoBlockDdl`] note with caller-supplied detail.
    pub fn do_block_note(&mut self, origin: &Origin, detail: String) {
        self.note(origin, NoteKind::DoBlockDdl, detail);
    }

    /// Note a `rowdiet:ignore` marker that ended up attached to no statement.
    pub fn unused_ignore_marker(&mut self, origin: &Origin) {
        self.note(
            origin,
            NoteKind::UnusedIgnoreMarker,
            "rowdiet:ignore is not attached to any statement — place it inside the statement it \
             should exempt (before its semicolon)"
                .to_string(),
        );
    }

    /// Note a temporary table that was not analyzed; `name` is its display spelling.
    pub fn temp_table_skipped(&mut self, name: &str, origin: &Origin) {
        self.note(
            origin,
            NoteKind::TempTableSkipped,
            format!("temporary table {name} is not analyzed (session-lived, no storage debt)"),
        );
    }

    fn unknown_column(&mut self, table: &RawName, column: &str, origin: &Origin) {
        let detail = format!("table {}: column {column} not found", table.display);
        self.note(origin, NoteKind::UnknownColumn, detail);
    }

    fn resolve_column(&mut self, raw: RawColumn, origin: &Origin) -> FoldedColumn {
        let resolved = self.resolve_type(&raw.type_ref, origin);
        FoldedColumn {
            display: raw.display,
            key: raw.key,
            type_display: raw.type_ref.display,
            kind: resolved.kind,
            known_type: resolved.known,
            not_null: raw.not_null || resolved.implicit_not_null,
        }
    }

    fn resolve_type(&mut self, type_ref: &TypeRef, origin: &Origin) -> Resolved {
        let resolved = self.catalog.resolve(type_ref);
        if !resolved.known && self.unknown_types_noted.insert(type_ref.key.clone()) {
            let detail = format!(
                "type {} unresolvable from DDL — assumed varlena, int-aligned (teach it via --assume-type or Config.assume)",
                type_ref.display
            );
            self.note(origin, NoteKind::UnknownType, detail);
        }
        resolved
    }

    fn note(&mut self, origin: &Origin, kind: NoteKind, detail: String) {
        self.notes.push(Note {
            origin: origin.clone(),
            kind,
            detail,
        });
    }
}

fn mark_altered(table: &mut FoldedTable, origin: &Origin) {
    if table.altered_in.last() != Some(origin) {
        table.altered_in.push(origin.clone());
    }
}

#[cfg(test)]
mod tests;
