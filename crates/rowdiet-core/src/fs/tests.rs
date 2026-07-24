use super::*;
use crate::ParserBackend;

fn fixture_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("V1__init.sql"),
        "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL);",
    )
    .unwrap();
    std::fs::write(dir.join("V2__more.sql"), "ALTER TABLE m ADD COLUMN c int NOT NULL;").unwrap();
    dir
}

#[test]
fn analyze_dir_walks_in_version_order() {
    let dir = fixture_dir("order");
    let analysis = analyze_dir(&dir, &Config::default()).unwrap();
    assert_eq!(analysis.tables.len(), 1);
    assert_eq!(analysis.tables[0].natts, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn analyze_dir_with_selects_the_backend() {
    let dir = fixture_dir("backend");
    let default = analyze_dir_with(ParserBackend::Sqlparser, &dir, &Config::default()).unwrap();
    assert_eq!(default.tables[0].natts, 3);
    #[cfg(feature = "pg-exact")]
    {
        let exact = analyze_dir_with(ParserBackend::PgExact, &dir, &Config::default()).unwrap();
        assert_eq!(exact.tables[0].natts, default.tables[0].natts);
        assert_eq!(
            exact.tables[0].avoidable_bytes_per_row,
            default.tables[0].avoidable_bytes_per_row
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn version_order_beats_lexicographic_order() {
    // "V10" sorts before "V2" lexicographically; version order must win or the ALTER in V10
    // arrives before its CREATE. Pins the filename-extraction feeding version::compare (its
    // replace-with-constant mutants survived while fixtures kept both orders identical).
    let dir = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-verlex", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("V2__init.sql"),
        "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL);",
    )
    .unwrap();
    std::fs::write(dir.join("V10__more.sql"), "ALTER TABLE m ADD COLUMN c int NOT NULL;").unwrap();
    let analysis = analyze_dir(&dir, &Config::default()).unwrap();
    assert_eq!(analysis.tables[0].natts, 3, "notes: {:?}", analysis.notes);
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recursion_spans_subdirectories_and_extension_case() {
    let dir = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-nested", std::process::id()));
    let deeper = dir.join("sub").join("deeper");
    std::fs::create_dir_all(&deeper).unwrap();
    std::fs::write(dir.join("V1__init.sql"), "CREATE TABLE n (a int NOT NULL);").unwrap();
    std::fs::write(
        dir.join("sub").join("V2__more.SQL"),
        "ALTER TABLE n ADD COLUMN b bigint NOT NULL;",
    )
    .unwrap();
    std::fs::write(
        deeper.join("V3__even.Sql"),
        "ALTER TABLE n ADD COLUMN c smallint NOT NULL;",
    )
    .unwrap();
    let analysis = analyze_dir(&dir, &Config::default()).unwrap();
    assert_eq!(analysis.tables[0].natts, 3, "notes: {:?}", analysis.notes);
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn symlinked_directories_are_not_entered() {
    // One fixture, both hazards: a loop link pointing back up (a cycle would collect V1 once
    // per traversal depth) and a directory reachable only through a link.
    let root = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-symdir", std::process::id()));
    let scanned = root.join("scanned");
    let outside = root.join("outside");
    std::fs::create_dir_all(&scanned).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(scanned.join("V1__init.sql"), "CREATE TABLE s (a int NOT NULL);").unwrap();
    std::fs::write(
        outside.join("V2__linked.sql"),
        "ALTER TABLE s ADD COLUMN b bigint NOT NULL;",
    )
    .unwrap();
    std::os::unix::fs::symlink(&root, scanned.join("loop")).unwrap();
    std::os::unix::fs::symlink(&outside, scanned.join("linked")).unwrap();
    let files = collect_sql_files(&scanned).unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    let analysis = analyze_dir(&scanned, &Config::default()).unwrap();
    assert_eq!(analysis.tables[0].natts, 1);
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn symlinked_sql_files_are_read_through() {
    let root = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-symfile", std::process::id()));
    let scanned = root.join("scanned");
    std::fs::create_dir_all(&scanned).unwrap();
    std::fs::write(root.join("target.sql"), "CREATE TABLE via_link (a int NOT NULL);").unwrap();
    std::os::unix::fs::symlink(root.join("target.sql"), scanned.join("V1__link.sql")).unwrap();
    let analysis = analyze_dir(&scanned, &Config::default()).unwrap();
    assert_eq!(analysis.tables.len(), 1, "notes: {:?}", analysis.notes);
    assert_eq!(analysis.tables[0].display, "via_link");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hidden_entries_are_skipped_but_a_hidden_root_works() {
    let root = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-hidden", std::process::id()));
    let hidden_root = root.join(".migrations");
    std::fs::create_dir_all(hidden_root.join(".editor")).unwrap();
    std::fs::write(hidden_root.join("V1__init.sql"), "CREATE TABLE h (a int NOT NULL);").unwrap();
    std::fs::write(hidden_root.join(".stray.sql"), "CREATE TABLE stray (a int NOT NULL);").unwrap();
    std::fs::write(
        hidden_root.join(".editor").join("V9__junk.sql"),
        "CREATE TABLE junk (a int NOT NULL);",
    )
    .unwrap();
    let analysis = analyze_dir(&hidden_root, &Config::default()).unwrap();
    assert_eq!(analysis.tables.len(), 1);
    assert_eq!(analysis.tables[0].display, "h");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn empty_scan_is_noted() {
    let dir = std::env::temp_dir().join(format!("rowdiet-fs-test-{}-empty", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("readme.txt"), "not sql").unwrap();
    let analysis = analyze_dir(&dir, &Config::default()).unwrap();
    assert!(analysis.tables.is_empty());
    assert_eq!(analysis.notes.len(), 1);
    assert_eq!(analysis.notes[0].kind, crate::fold::NoteKind::EmptyScan);
    assert_eq!(analysis.notes[0].origin.line, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
