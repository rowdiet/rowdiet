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
