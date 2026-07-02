use super::*;
use crate::extract;
use crate::layout::{Align, ColumnKind};
use std::collections::BTreeMap;

fn origin(line: u32) -> Origin {
    Origin {
        source: "test.sql".into(),
        line,
    }
}

fn folded(statements: &[&str]) -> (Vec<FoldedTable>, Vec<Note>) {
    let mut folder = Folder::new(BTreeMap::new());
    for (i, stmt) in statements.iter().enumerate() {
        let o = origin(i as u32 + 1);
        match extract::extract(stmt) {
            Ok(ops) => folder.apply(ops, &o, false),
            Err(e) => folder.skipped(&o, e, extract::sniff(stmt)),
        }
    }
    folder.finish()
}

#[test]
fn create_then_adds_append_in_order() {
    let (tables, notes) = folded(&[
        "CREATE TABLE t (a boolean NOT NULL, b bigint NOT NULL)",
        "ALTER TABLE t ADD COLUMN c integer NOT NULL",
        "ALTER TABLE t ADD COLUMN d text",
    ]);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(tables.len(), 1);
    let keys: Vec<&str> = tables[0].columns.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "b", "c", "d"]);
    assert_eq!(tables[0].altered_in.len(), 2);
}

#[test]
fn session_enum_resolves_columns() {
    let (tables, notes) = folded(&[
        "CREATE TYPE status AS ENUM ('a','b')",
        "CREATE TABLE t (s status NOT NULL, id bigint NOT NULL)",
    ]);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        tables[0].columns[0].kind,
        ColumnKind::Fixed {
            len: 4,
            align: Align::Int
        }
    );
    assert!(tables[0].columns[0].known_type);
}

#[test]
fn unknown_type_noted_once() {
    let (tables, notes) = folded(&["CREATE TABLE t (a wat_type, b wat_type)"]);
    assert!(!tables[0].columns[0].known_type);
    assert_eq!(notes.iter().filter(|n| n.kind == NoteKind::UnknownType).count(), 1);
}

#[test]
fn alter_unknown_table_notes() {
    let (tables, notes) = folded(&["ALTER TABLE elsewhere ADD COLUMN a int"]);
    assert!(tables.is_empty());
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, NoteKind::AlterUnknownTable);
}

#[test]
fn skipped_create_makes_ghost() {
    let (tables, notes) = folded(&[
        "CREATE TABLE broken (LIKE other INCLUDING ALL)",
        "ALTER TABLE broken ADD COLUMN a int",
    ]);
    assert!(tables.is_empty());
    assert_eq!(notes[0].kind, NoteKind::SkippedStatement);
    assert_eq!(notes[1].kind, NoteKind::AlterSkippedTable);
}

#[test]
fn skipped_alter_marks_incomplete() {
    let (tables, notes) = folded(&["CREATE TABLE t (a int)", "ALTER TABLE t nonsense grammar here"]);
    assert!(tables[0].incomplete);
    assert!(notes.iter().any(|n| n.kind == NoteKind::SkippedStatement));
}

#[test]
fn drop_and_rename() {
    let (tables, notes) = folded(&[
        "CREATE TABLE t (a int NOT NULL, b int NOT NULL, c int NOT NULL)",
        "ALTER TABLE t DROP COLUMN b",
        "ALTER TABLE t RENAME COLUMN c TO z",
        "ALTER TABLE t RENAME TO u",
    ]);
    assert_eq!(tables[0].key, "u");
    let keys: Vec<&str> = tables[0].columns.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "z"]);
    assert!(notes.iter().any(|n| n.kind == NoteKind::DroppedColumn));
}

#[test]
fn set_type_and_not_null_edit_in_place() {
    let (tables, _) = folded(&[
        "CREATE TABLE t (a int, b text)",
        "ALTER TABLE t ALTER COLUMN a TYPE bigint",
        "ALTER TABLE t ALTER COLUMN b SET NOT NULL",
    ]);
    assert_eq!(
        tables[0].columns[0].kind,
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
    assert!(tables[0].columns[1].not_null);
    assert!(!tables[0].columns[0].not_null);
}

#[test]
fn drop_table_removes() {
    let (tables, notes) = folded(&[
        "CREATE TABLE t (a int)",
        "DROP TABLE t",
        "DROP TABLE IF EXISTS never_seen",
    ]);
    assert!(tables.is_empty());
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn ctas_is_ghosted() {
    let (tables, notes) = folded(&[
        "CREATE TABLE snap AS SELECT 1 AS one",
        "ALTER TABLE snap ADD COLUMN extra int",
    ]);
    assert!(tables.is_empty());
    assert_eq!(notes[0].kind, NoteKind::CtasSkipped);
    assert_eq!(notes[1].kind, NoteKind::AlterSkippedTable);
}

#[test]
fn redefine_replaces_with_note() {
    let (tables, notes) = folded(&[
        "CREATE TABLE t (a int)",
        "CREATE TABLE t (b bigint NOT NULL)",
        "CREATE TABLE IF NOT EXISTS t (c text)",
    ]);
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].columns[0].key, "b");
    assert_eq!(notes.iter().filter(|n| n.kind == NoteKind::Redefined).count(), 1);
}

#[test]
fn domain_and_range_session_types() {
    let (tables, notes) = folded(&[
        "CREATE DOMAIN code AS varchar(20)",
        "CREATE TYPE bigrange AS RANGE (SUBTYPE = int8)",
        "CREATE TABLE t (c code, r bigrange)",
    ]);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        tables[0].columns[0].kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true
        }
    );
    assert_eq!(
        tables[0].columns[1].kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false
        }
    );
}

#[test]
fn serial_column_is_not_null() {
    let (tables, _) = folded(&["CREATE TABLE t (id bigserial, x int)"]);
    assert!(tables[0].columns[0].not_null);
    assert_eq!(
        tables[0].columns[0].kind,
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
}
