use crate::*;

fn src(name: &str, sql: &str) -> SqlSource {
    SqlSource {
        name: name.into(),
        sql: sql.into(),
    }
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
    assert_eq!(
        t.suggested_order,
        vec!["id", "created_at", "status", "flag", "note", "meta"]
    );
    assert_eq!(t.altered_in.len(), 1);
    assert!(t.any_nullable);
    assert!(
        analysis.notes.is_empty(),
        "DML-only DO blocks are silent now: {:?}",
        analysis.notes
    );
}

#[test]
fn do_block_enum_guard_resolves_types() {
    let sql = "DO $$ BEGIN\n CREATE TYPE mood AS ENUM ('ok','bad');\nEXCEPTION WHEN duplicate_object THEN null;\nEND $$;\nCREATE TABLE t (m mood NOT NULL, id bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__m.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    assert!(t.assumed_types.is_empty());
    assert_eq!(t.tier, Tier::Exact);
    assert_eq!(t.natts, 2);
}

#[test]
fn do_block_table_ddl_is_conditional_not_folded() {
    let sql = "CREATE TABLE t (a bigint NOT NULL);\nDO $$ BEGIN\n IF NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 't' AND column_name = 'x') THEN\n ALTER TABLE t ADD COLUMN x int;\n END IF;\nEND $$;";
    let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.natts, 1, "conditional column must not be folded");
    assert!(t.incomplete);
    assert!(
        analysis.notes.iter().any(|n| n.kind == NoteKind::DoBlockDdl),
        "{:?}",
        analysis.notes
    );
}

#[test]
fn dynamic_sql_in_do_is_flagged() {
    let sql = "DO $x$ BEGIN\n EXECUTE format('ALTER TABLE %I ADD COLUMN y int', tbl);\nEND $x$;";
    let analysis = analyze_sources(&[src("V1__d.sql", sql)], &Config::default());
    assert_eq!(analysis.notes.len(), 1);
    assert_eq!(analysis.notes[0].kind, NoteKind::DoBlockDdl);
    assert!(analysis.notes[0].detail.contains("not statically analyzable"));
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
    let sql = "CREATE TABLE t (v wat_type(768), id bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__v.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.assumed_types, vec!["wat_type(768)"]);
    assert!(analysis.notes.iter().any(|n| n.kind == NoteKind::UnknownType));
    let mut config = Config::default();
    config
        .assume
        .insert("wat_type".into(), AssumedKind::Varlena { align: Align::Double });
    let taught = analyze_sources(&[src("V1__v.sql", sql)], &config);
    assert!(taught.tables[0].assumed_types.is_empty());
    assert!(taught.notes.is_empty());
}

#[test]
fn pgvector_columns_resolve_verified() {
    let sql = "CREATE TABLE emb (id bigint NOT NULL, v vector(768) NOT NULL);";
    let analysis = analyze_sources(&[src("V1__emb.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert!(t.assumed_types.is_empty());
    assert!(analysis.notes.is_empty());
    assert_eq!(t.tier, Tier::Estimate);
    assert_eq!(t.avoidable_bytes_per_row, 0);
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
    assert_eq!(
        analysis.tables[0].origin,
        Origin {
            source: "V1__ab.sql".into(),
            line: 2
        }
    );
    assert_eq!(
        analysis.tables[1].origin,
        Origin {
            source: "V1__ab.sql".into(),
            line: 3
        }
    );
}

#[cfg(feature = "serde")]
#[test]
fn analysis_serializes() {
    let analysis = analyze_sources(
        &[src("V1__x.sql", "CREATE TABLE t (a boolean, b bigint);")],
        &Config::default(),
    );
    let json = serde_json::to_string(&analysis).unwrap();
    assert!(json.contains("\"avoidable_bytes_per_row\""));
}

/// The pg-exact backend doubles as the differential oracle: both parsers must produce the same
/// DdlOps (modulo display text) and the same analysis numbers over everything both can parse.
#[cfg(feature = "pg-exact")]
mod differential {
    use crate::catalog::TypeRef;
    use crate::extract::{DdlOp, RawColumn, RawName};
    use crate::{analyze_sources_with, extract, extract_pgq, Config, ParserBackend, SqlSource};

    const CORPUS: &[&str] = &[
        "CREATE TABLE account (active boolean NOT NULL, id bigint PRIMARY KEY, kind smallint NOT NULL, balance bigint NOT NULL)",
        "CREATE TABLE ints (a int, b integer, c int4, d int8, e bigint, f smallint, g real, h double precision, i float4, j float8)",
        "CREATE TABLE chars (a varchar(255), b character varying(31), c char(10), d char, e varchar, f text)",
        "CREATE TABLE times (a timestamptz, b timestamp with time zone, c timestamp(6) without time zone, d time, e time(3) with time zone, f timetz, g date, h interval)",
        "CREATE TABLE nums (a numeric(10,2), b decimal(12,4), c numeric, d bit(4), e bit varying(8))",
        "CREATE TABLE arrs (a int[], b bigint[], c double precision[][], d numeric(10,2)[], e varchar(16)[], f text[])",
        "CREATE TABLE serials (id bigserial PRIMARY KEY, n serial, m smallserial)",
        "CREATE TABLE nn (a int NOT NULL, b int GENERATED ALWAYS AS IDENTITY, c int, PRIMARY KEY (a, c))",
        "CREATE TABLE cu (a my_schema.status_enum, b citext, c vector(768), d tstzrange, e int4range, f inet, g macaddr, h money, i oid, j xml, k tsvector, l point, m uuid, n jsonb, o bytea)",
        "CREATE TABLE \"my schema\".\"My Table\" (\"select\" int, \"Weird Col\" text, UnQuoted int)",
        "CREATE TABLE t4 (a text DEFAULT 'x; y', b int)",
        "CREATE TEMPORARY TABLE tmp (a int)",
        "ALTER TABLE t ADD COLUMN z timestamptz NOT NULL",
        "ALTER TABLE t ADD COLUMN IF NOT EXISTS w int",
        "ALTER TABLE t ADD COLUMN x int, ADD COLUMN y text",
        "ALTER TABLE t DROP COLUMN a",
        "ALTER TABLE t RENAME COLUMN a TO b",
        "ALTER TABLE t RENAME TO u",
        "ALTER TABLE t ALTER COLUMN c TYPE bigint",
        "ALTER TABLE t ALTER COLUMN c SET NOT NULL",
        "ALTER TABLE t ALTER COLUMN c DROP NOT NULL",
        "ALTER TABLE t ADD PRIMARY KEY (a)",
        "ALTER TABLE t ADD CONSTRAINT pk PRIMARY KEY (a)",
        "CREATE TYPE status AS ENUM ('a','b')",
        "CREATE TYPE pair AS (x int, y int)",
        "CREATE TYPE br AS RANGE (SUBTYPE = int8)",
        "CREATE DOMAIN code AS varchar(20)",
        "DROP TABLE IF EXISTS a, b",
        "DROP TYPE status",
        "CREATE INDEX i ON t (a)",
        "CREATE TABLE part_parent (a int NOT NULL, b bigint NOT NULL) PARTITION BY RANGE (a)",
        "CREATE TABLE part_child PARTITION OF part_parent FOR VALUES FROM (1) TO (10)",
    ];

    fn norm_name(n: RawName) -> RawName {
        RawName {
            display: n.key.clone(),
            key: n.key,
        }
    }

    fn norm_type(t: TypeRef) -> TypeRef {
        TypeRef {
            display: format!("{}/{}", t.key, t.dims),
            key: t.key,
            char_len: t.char_len,
            dims: t.dims,
        }
    }

    fn norm_col(c: RawColumn) -> RawColumn {
        RawColumn {
            display: c.key.clone(),
            key: c.key,
            type_ref: norm_type(c.type_ref),
            not_null: c.not_null,
        }
    }

    fn norm(op: DdlOp) -> DdlOp {
        match op {
            DdlOp::CreateTable {
                name,
                columns,
                pk_columns,
                if_not_exists,
                is_ctas,
                incomplete_columns,
                partition_of,
            } => DdlOp::CreateTable {
                name: norm_name(name),
                columns: columns.into_iter().map(norm_col).collect(),
                pk_columns,
                if_not_exists,
                is_ctas,
                incomplete_columns,
                partition_of: partition_of.map(norm_name),
            },
            DdlOp::AddColumn {
                table,
                column,
                if_not_exists,
            } => DdlOp::AddColumn {
                table: norm_name(table),
                column: norm_col(column),
                if_not_exists,
            },
            DdlOp::DropColumns {
                table,
                columns,
                if_exists,
            } => DdlOp::DropColumns {
                table: norm_name(table),
                columns,
                if_exists,
            },
            DdlOp::RenameColumn { table, old, new } => DdlOp::RenameColumn {
                table: norm_name(table),
                old,
                new,
            },
            DdlOp::RenameTable { table, new } => DdlOp::RenameTable {
                table: norm_name(table),
                new: norm_name(new),
            },
            DdlOp::SetColumnType {
                table,
                column,
                type_ref,
            } => DdlOp::SetColumnType {
                table: norm_name(table),
                column,
                type_ref: norm_type(type_ref),
            },
            DdlOp::SetNotNull { table, column, value } => DdlOp::SetNotNull {
                table: norm_name(table),
                column,
                value,
            },
            DdlOp::DropTables { names, if_exists } => DdlOp::DropTables {
                names: names.into_iter().map(norm_name).collect(),
                if_exists,
            },
            DdlOp::CreateEnum { name } => DdlOp::CreateEnum { name: norm_name(name) },
            DdlOp::CreateComposite { name } => DdlOp::CreateComposite { name: norm_name(name) },
            DdlOp::CreateRange { name, subtype } => DdlOp::CreateRange {
                name: norm_name(name),
                subtype: subtype.map(norm_type),
            },
            DdlOp::CreateBase { name } => DdlOp::CreateBase { name: norm_name(name) },
            DdlOp::CreateDomain { name, base } => DdlOp::CreateDomain {
                name: norm_name(name),
                base: norm_type(base),
            },
            DdlOp::DropTypes { names } => DdlOp::DropTypes {
                names: names.into_iter().map(norm_name).collect(),
            },
            DdlOp::Irrelevant => DdlOp::Irrelevant,
        }
    }

    #[test]
    fn backends_agree_on_extracted_ops() {
        for sql in CORPUS {
            let via_sqlparser: Vec<DdlOp> = extract::extract(&extract::preprocess(sql))
                .expect(sql)
                .into_iter()
                .map(norm)
                .collect();
            let via_pgq: Vec<DdlOp> = extract_pgq::extract(sql).expect(sql).into_iter().map(norm).collect();
            assert_eq!(via_sqlparser, via_pgq, "{sql}");
        }
    }

    #[test]
    fn partition_children_inherit_parent_layout() {
        let sql = "CREATE TABLE evt (flag boolean NOT NULL, id bigint NOT NULL) PARTITION BY RANGE (id);\nCREATE TABLE evt_1 PARTITION OF evt FOR VALUES FROM (1) TO (10);";
        for backend in [ParserBackend::Sqlparser, ParserBackend::PgExact] {
            let analysis = analyze_sources_with(
                backend,
                &[SqlSource {
                    name: "V1__evt.sql".into(),
                    sql: sql.into(),
                }],
                &Config::default(),
            );
            let child = &analysis.tables[1];
            assert_eq!(child.name, "evt_1");
            assert_eq!(child.natts, 2, "{backend:?}");
            assert!(!child.incomplete);
            assert_eq!(
                child.avoidable_bytes_per_row,
                analysis.tables[0].avoidable_bytes_per_row
            );
            assert!(analysis.notes.is_empty(), "{backend:?}: {:?}", analysis.notes);
        }
    }

    #[test]
    fn do_block_scan_agrees_across_backends() {
        let sql = "DO $$ BEGIN CREATE TYPE mood AS ENUM ('ok','bad'); EXCEPTION WHEN duplicate_object THEN null; END $$;\nCREATE TABLE t (m mood NOT NULL, id bigint NOT NULL);\nDO $$ BEGIN IF true THEN ALTER TABLE t ADD COLUMN x int; END IF; END $$;";
        for backend in [ParserBackend::Sqlparser, ParserBackend::PgExact] {
            let analysis = analyze_sources_with(
                backend,
                &[SqlSource {
                    name: "V1__do.sql".into(),
                    sql: sql.into(),
                }],
                &Config::default(),
            );
            let t = &analysis.tables[0];
            assert_eq!(t.natts, 2, "{backend:?}");
            assert!(t.assumed_types.is_empty(), "{backend:?}");
            assert!(t.incomplete, "{backend:?}");
            assert_eq!(analysis.notes.len(), 1, "{backend:?}: {:?}", analysis.notes);
        }
    }

    #[test]
    fn backends_agree_on_full_analysis() {
        let sources = vec![
            SqlSource {
                name: "V1__init.sql".into(),
                sql: "CREATE TYPE order_status AS ENUM ('new','paid');\nCREATE UNLOGGED TABLE orders (flag boolean NOT NULL, id bigint PRIMARY KEY, status order_status NOT NULL, note text);".into(),
            },
            SqlSource {
                name: "V2__add.sql".into(),
                sql: "ALTER TABLE orders ADD COLUMN created_at timestamptz NOT NULL, ADD COLUMN meta jsonb;".into(),
            },
        ];
        let a = analyze_sources_with(ParserBackend::Sqlparser, &sources, &Config::default());
        let b = analyze_sources_with(ParserBackend::PgExact, &sources, &Config::default());
        assert_eq!(a.tables.len(), b.tables.len());
        for (x, y) in a.tables.iter().zip(&b.tables) {
            assert_eq!(x.name, y.name);
            assert_eq!(x.natts, y.natts, "{}", x.name);
            assert_eq!(x.tier, y.tier);
            assert_eq!(x.current.padding, y.current.padding);
            assert_eq!(x.suggested.padding, y.suggested.padding);
            assert_eq!(x.avoidable_bytes_per_row, y.avoidable_bytes_per_row);
            assert_eq!(x.suggested_order, y.suggested_order);
            assert_eq!(x.any_nullable, y.any_nullable);
        }
    }
}
