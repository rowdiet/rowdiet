//! Replays extracted DDL over an in-memory schema model, in version order: CREATE TABLE
//! establishes physical column order, ALTER TABLE ADD COLUMN appends (as Postgres does), later
//! type/nullability changes edit in place. Everything the model cannot absorb becomes a [`Note`]
//! — visible degradation, never silent.

use crate::catalog::{AssumedKind, Catalog, Resolved, TypeRef};
use crate::extract::{DdlOp, RawColumn, RawName, Sniff};
use crate::layout::ColumnKind;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Origin {
    pub source: String,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum NoteKind {
    SkippedStatement,
    AlterUnknownTable,
    AlterSkippedTable,
    CtasSkipped,
    IncompleteColumns,
    DroppedColumn,
    UnknownType,
    Redefined,
    DuplicateColumn,
    UnknownColumn,
    DoBlockDdl,
    UnusedIgnoreMarker,
    TempTableSkipped,
    EmptyScan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Note {
    pub origin: Origin,
    pub kind: NoteKind,
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

#[derive(Debug, Clone, PartialEq)]
pub struct FoldedColumn {
    pub display: String,
    pub key: String,
    pub type_display: String,
    pub kind: ColumnKind,
    pub known_type: bool,
    pub not_null: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FoldedTable {
    pub display: String,
    pub key: String,
    pub origin: Origin,
    pub altered_in: Vec<Origin>,
    pub ignored: bool,
    pub incomplete: bool,
    pub columns: Vec<FoldedColumn>,
    /// Columns dropped over the series. Postgres keeps dropped attributes (attisdropped), so
    /// every row written after the drop stores a NULL for each — the null bitmap is present
    /// and sized by the ORIGINAL attribute count. Layout math must include that header cost.
    pub dropped_count: usize,
}

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
        for raw in columns {
            let mut column = self.resolve_column(raw, origin);
            if pk_columns.contains(&column.key) {
                column.not_null = true;
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

    pub fn do_block_note(&mut self, origin: &Origin, detail: String) {
        self.note(origin, NoteKind::DoBlockDdl, detail);
    }

    pub fn unused_ignore_marker(&mut self, origin: &Origin) {
        self.note(
            origin,
            NoteKind::UnusedIgnoreMarker,
            "rowdiet:ignore is not attached to any statement — place it inside the statement it \
             should exempt (before its semicolon)"
                .to_string(),
        );
    }

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
