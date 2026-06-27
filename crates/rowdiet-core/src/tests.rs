use crate::*;

fn src(name: &str, sql: &str) -> SqlSource {
    SqlSource { name: name.into(), sql: sql.into() }
}

#[test]
fn end_to_end_migration_series() {
    let v1 = src(
        "V1__init.sql",
        r#"
        CREATE TYPE order_status AS ENUM ('new', 'paid');
        CREATE TABLE orders (
            flag boolean NOT NULL,
            id bigint PRIMARY KEY,
            status order_status NOT NULL,
            note text
        );
        DO $$ BEGIN RAISE NOTICE 'hi'; END $$;
        "#,
    );
    let v2 = src(
        "V2__add_cols.sql",
        "ALTER TABLE orders ADD COLUMN created_at timestamptz NOT NULL, ADD COLUMN meta jsonb;",
    );
    let analysis = analyze_sources(&[v1, v2], &Config::default());
    assert_eq!(analysis.tables.len(), 1);
    let t = &analysis.tables[0];
    assert_eq!(t.natts, 6);
    assert_eq!(t.tier, Tier::Estimate);
    assert_eq!(t.current.padding, 7);
    assert_eq!(t.suggested.padding, 3);
    assert_eq!(t.avoidable_bytes_per_row, 4);
    assert_eq!(t.suggested_order, vec!["id", "created_at", "status", "flag", "note", "meta"]);
    assert_eq!(t.altered_in.len(), 1);
    assert!(t.any_nullable);
    assert!(analysis.notes.iter().any(|n| n.kind == NoteKind::SkippedStatement));
}

#[test]
fn exact_tier_footprint_and_rows_per_page() {
    let sql = "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__m.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.tier, Tier::Exact);
    assert_eq!(t.current.footprint, Some(56));
    assert_eq!(t.suggested.footprint, Some(48));
    assert_eq!(t.avoidable_bytes_per_row, 8);
    assert_eq!(t.current.rows_per_page, Some(136));
    assert_eq!(t.suggested.rows_per_page, Some(157));
    assert!(!t.any_nullable);
}

#[test]
fn rung_not_crossed_reports_zero_avoidable() {
    let sql = "CREATE TABLE t (flag boolean NOT NULL, a bigint NOT NULL, b timestamptz NOT NULL);";
    let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.current.padding, 7);
    assert_eq!(t.avoidable_bytes_per_row, 0);
    assert_eq!(t.suggested_order, vec!["flag", "a", "b"]);
}

#[test]
fn ignore_marker_flags_table() {
    let sql = "CREATE TABLE noisy ( -- rowdiet:ignore\n a boolean, b bigint);";
    let analysis = analyze_sources(&[src("V1__n.sql", sql)], &Config::default());
    assert!(analysis.tables[0].ignored);
}

#[test]
fn unknown_type_assumed_and_teachable() {
    let sql = "CREATE TABLE t (v vector(768), id bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__v.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.assumed_types, vec!["vector(768)"]);
    assert!(analysis.notes.iter().any(|n| n.kind == NoteKind::UnknownType));
    let mut config = Config::default();
    config.assume.insert("vector".into(), AssumedKind::Varlena { align: Align::Double });
    let taught = analyze_sources(&[src("V1__v.sql", sql)], &config);
    assert!(taught.tables[0].assumed_types.is_empty());
    assert!(taught.notes.is_empty());
}

#[test]
fn serial_primary_key_table_already_optimal() {
    let sql = "CREATE TABLE s (id bigserial PRIMARY KEY, active boolean NOT NULL);";
    let analysis = analyze_sources(&[src("V1__s.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.tier, Tier::Exact);
    assert_eq!(t.avoidable_bytes_per_row, 0);
    assert!(!t.any_nullable);
}

#[test]
fn origins_track_source_and_line() {
    let sql = "-- header\nCREATE TABLE a (x int);\nCREATE TABLE b (y bigint, z boolean);";
    let analysis = analyze_sources(&[src("V1__ab.sql", sql)], &Config::default());
    assert_eq!(analysis.tables[0].origin, Origin { source: "V1__ab.sql".into(), line: 2 });
    assert_eq!(analysis.tables[1].origin, Origin { source: "V1__ab.sql".into(), line: 3 });
}

#[cfg(feature = "serde")]
#[test]
fn analysis_serializes() {
    let analysis = analyze_sources(&[src("V1__x.sql", "CREATE TABLE t (a boolean, b bigint);")], &Config::default());
    let json = serde_json::to_string(&analysis).unwrap();
    assert!(json.contains("\"avoidable_bytes_per_row\""));
}
