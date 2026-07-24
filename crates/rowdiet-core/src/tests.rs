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
        r"
        CREATE TYPE order_status AS ENUM ('new', 'paid');
        CREATE TABLE orders (
            flag boolean NOT NULL,
            id bigint PRIMARY KEY,
            status order_status NOT NULL,
            note text
        );
        DO $$ BEGIN RAISE NOTICE 'hi'; END $$;
        ",
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
fn renamed_type_resolves_under_new_name() {
    let sql = "CREATE TYPE subject AS ENUM ('a','b');\nALTER TYPE subject RENAME TO run_subject;\nCREATE TABLE t (s run_subject NOT NULL, id bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__r.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    assert!(t.assumed_types.is_empty());
    assert_eq!(t.tier, Tier::Exact);
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
fn dynamic_partition_loop_is_layout_inert() {
    let sql = "CREATE TABLE chunk (a bigint NOT NULL, b boolean NOT NULL) PARTITION BY HASH (a);\nDO $$ BEGIN\n FOR r IN 0..15 LOOP\n EXECUTE format('CREATE TABLE chunk_p%s PARTITION OF chunk FOR VALUES WITH (MODULUS 16, REMAINDER %s)', r, r);\n END LOOP;\nEND $$;";
    let analysis = analyze_sources(&[src("V1__c.sql", sql)], &Config::default());
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    assert_eq!(analysis.tables.len(), 1);
}

#[test]
fn dynamic_partition_with_ident_placeholder_name() {
    let sql = "CREATE TABLE evt (a bigint NOT NULL) PARTITION BY RANGE (a);\nDO $$ BEGIN\n EXECUTE format('CREATE TABLE %I PARTITION OF evt FOR VALUES FROM (1) TO (2)', name);\nEND $$;";
    let analysis = analyze_sources(&[src("V1__e.sql", sql)], &Config::default());
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
}

#[test]
fn dynamic_partition_of_unknown_parent_stays_flagged() {
    let sql = "DO $$ BEGIN\n EXECUTE format('CREATE TABLE p%s PARTITION OF elsewhere FOR VALUES WITH (MODULUS 4, REMAINDER %s)', i, i);\nEND $$;";
    let analysis = analyze_sources(&[src("V1__u.sql", sql)], &Config::default());
    assert_eq!(analysis.notes.len(), 1);
    assert_eq!(analysis.notes[0].kind, NoteKind::DoBlockDdl);
}

#[test]
fn dynamic_alter_with_concrete_target_notes_that_table() {
    let sql = "CREATE TABLE payments (id bigint NOT NULL);\nDO $$ BEGIN\n EXECUTE format('ALTER TABLE payments ADD COLUMN %I int', col);\nEND $$;";
    let analysis = analyze_sources(&[src("V1__pay.sql", sql)], &Config::default());
    assert!(analysis.tables[0].incomplete);
    assert_eq!(analysis.notes.len(), 1);
    assert!(analysis.notes[0].detail.contains("payments"), "{:?}", analysis.notes);
}

#[test]
fn ignore_marker_waives_do_scanning() {
    let sql = "DO $x$ BEGIN -- rowdiet:ignore\n EXECUTE format('CREATE TABLE p%s PARTITION OF t', i);\nEND $x$;";
    let analysis = analyze_sources(&[src("V1__p.sql", sql)], &Config::default());
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
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
    use crate::{Config, ParserBackend, SqlSource, analyze_sources_with, extract, extract_pgq};

    const CORPUS: &[&str] = &[
        "CREATE TABLE s1.t1 (flag boolean NOT NULL, id bigint NOT NULL);",
        "DROP TABLE s1.t1, s2.t2;",
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
        "ALTER TYPE status RENAME TO status_v2",
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
                temporary,
                partition_of,
            } => DdlOp::CreateTable {
                name: norm_name(name),
                columns: columns.into_iter().map(norm_col).collect(),
                pk_columns,
                if_not_exists,
                is_ctas,
                incomplete_columns,
                temporary,
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
            DdlOp::RenameType { name, new } => DdlOp::RenameType {
                name: norm_name(name),
                new: norm_name(new),
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

/// Focused unit tests for the dynamic-template machinery. These functions run on hostile input
/// (arbitrary plpgsql fragments), and their boundary arithmetic was the largest surviving-mutant
/// cluster in the first cargo-mutants campaign — happy-path DO tests never pinned the edges.
mod dynamic_template_units {
    use crate::{execute_template, find_ddl_keyword, substitute_format};

    #[test]
    fn execute_template_extracts_and_unescapes() {
        let t = execute_template("EXECUTE format('CREATE TABLE %I (id int)', name);").unwrap();
        assert_eq!(t, "CREATE TABLE %I (id int)");
        let doubled = execute_template("EXECUTE 'it''s %I';").unwrap();
        assert_eq!(doubled, "it's %I");
    }

    #[test]
    fn execute_template_rejects_missing_or_unterminated_literals() {
        assert_eq!(execute_template("EXECUTE make_sql(tbl);"), None);
        assert_eq!(execute_template("EXECUTE 'unterminated"), None);
        assert_eq!(execute_template("EXECUTE 'trailing escape''"), None);
        // `execute` must stand alone as a word — an identifier containing it is not the keyword.
        assert_eq!(execute_template("SELECT reexecute('CREATE TABLE x');"), None);
        assert_eq!(execute_template("SELECT executed('CREATE TABLE x');"), None);
    }

    #[test]
    fn substitute_format_handles_every_placeholder_form() {
        assert_eq!(
            substitute_format("CREATE TABLE %I (n %s)", "tok"),
            "CREATE TABLE tok (n tok)"
        );
        assert_eq!(substitute_format("%1$I keeps %2$s order", "t"), "t keeps t order");
        assert_eq!(substitute_format("DEFAULT %L", "t"), "DEFAULT '0'");
        assert_eq!(substitute_format("100%% done", "t"), "100% done");
        // Unknown verbs and a trailing bare % pass through untouched.
        assert_eq!(substitute_format("%x %", "t"), "%x %");
        // A digit run without `$` is not positional syntax; nothing is substituted.
        assert_eq!(substitute_format("%42", "t"), "%42");
        // A digitless `$` is not positional syntax, and `%%` collapses only immediately after
        // the `%` — digits in between make both literal.
        assert_eq!(substitute_format("%$I", "t"), "%$I");
        assert_eq!(substitute_format("%4%", "t"), "%4%");
        // Multi-byte characters around placeholders survive byte-exact. The adjacent-placeholder
        // cases matter: a wrong char-width only misbehaves when a placeholder sits inside the
        // mis-sliced span (verbatim spans copy correctly at any claimed width).
        assert_eq!(substitute_format("héllo %I wörld", "t"), "héllo t wörld");
        assert_eq!(substitute_format("é%s!", "t"), "ét!");
        assert_eq!(substitute_format("€%s!", "t"), "€t!");
        assert_eq!(substitute_format("𝄞%s𝄞", "t"), "𝄞t𝄞");
    }

    #[test]
    fn find_ddl_keyword_respects_word_boundaries() {
        assert_eq!(find_ddl_keyword("create table t"), Some(0));
        assert_eq!(find_ddl_keyword("IF done THEN ALTER TABLE t"), Some(13));
        assert_eq!(find_ddl_keyword("procreate() drop x"), Some(12));
        assert_eq!(find_ddl_keyword("procreated alterations dropped"), None);
        assert_eq!(find_ddl_keyword("nothing here"), None);
    }
}

#[test]
fn do_block_counts_directly_unanalyzable_fragments() {
    // Fragments with a DDL keyword that neither parse nor yield an EXECUTE template take the
    // direct Unanalyzable arm — distinct from the dynamic-dispatch fallthrough, and previously
    // reachable by no test (both `+=` mutants on its counter survived).
    let sql = r"
        DO $$ BEGIN
            EXECUTE 'CREATE ' || kind || ' whatever';
            EXECUTE 'ALTER ' || kind || ' whatever';
        END $$;
    ";
    let analysis = analyze_sources(&[src("V1__dyn.sql", sql)], &Config::default());
    let note = analysis
        .notes
        .iter()
        .find(|n| n.detail.contains("not statically analyzable"))
        .expect("summary note");
    assert!(note.detail.contains("2 DDL-like"), "{}", note.detail);
}

/// Guard pins for the pg-exact protobuf mapping — non-type statements that share a protobuf
/// shape with type DDL must map to Irrelevant, not register phantom types. (Both guards showed
/// up as surviving replace-with-true mutants before these tests.)
#[cfg(feature = "pg-exact")]
mod pgq_guards {
    use crate::extract::DdlOp;
    use crate::extract_pgq;

    #[test]
    fn non_type_define_stmts_are_irrelevant() {
        let ops = extract_pgq::extract("CREATE AGGREGATE sum2 (int) (sfunc = int4pl, stype = int4);").unwrap();
        assert_eq!(ops, vec![DdlOp::Irrelevant]);
        let ops = extract_pgq::extract("CREATE COLLATION nocase (provider = icu, locale = 'und');").unwrap();
        assert_eq!(ops, vec![DdlOp::Irrelevant]);
    }

    #[test]
    fn range_subtype_found_among_other_params() {
        let ops = extract_pgq::extract(
            "CREATE TYPE r8 AS RANGE (subtype_opclass = int8_ops, subtype = int8, collation = \"C\");",
        )
        .unwrap();
        match &ops[..] {
            [
                DdlOp::CreateRange {
                    name,
                    subtype: Some(sub),
                },
            ] => {
                assert_eq!(name.key, "r8");
                assert_eq!(sub.key, "int8");
            }
            other => panic!("unexpected ops: {other:?}"),
        }
    }
}

#[test]
fn float_precision_selects_storage_width() {
    // Postgres: float(1..=24) is float4, float(25..=53) is float8 — a modeling fact, not a
    // display nicety (4 vs 8 bytes, i vs d alignment).
    let sql = "CREATE TABLE f (a float(24) NOT NULL, b float(25) NOT NULL, c float NOT NULL, id bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__f.sql", sql)], &Config::default());
    let kinds: Vec<ColumnKind> = analysis.tables[0].columns.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds[0],
        ColumnKind::Fixed {
            len: 4,
            align: Align::Int
        }
    );
    assert_eq!(
        kinds[1],
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
    assert_eq!(
        kinds[2],
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
}

#[test]
fn statement_origins_carry_real_line_numbers() {
    // Line accounting includes comment and in-statement newlines; a single-statement file
    // cannot distinguish a broken counter from a working one (everything is line 1).
    let sql = "-- header comment\n\nCREATE TABLE a (x int NOT NULL,\n  y bigint NOT NULL);\n/* block\n   comment */\nCREATE TABLE b (z int NOT NULL);\n";
    let analysis = analyze_sources(&[src("V1__lines.sql", sql)], &Config::default());
    let by_name: std::collections::BTreeMap<&str, u32> = analysis
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t.origin.line))
        .collect();
    assert_eq!(by_name["a"], 3);
    assert_eq!(by_name["b"], 7);
    // Hyphens and slashes that are NOT comment openers must not swallow text.
    let tricky =
        "CREATE TABLE c (x int NOT NULL); INSERT INTO c SELECT 1 - 2 / 3;\nCREATE TABLE d (y bigint NOT NULL);";
    let analysis = analyze_sources(&[src("V1__t.sql", tricky)], &Config::default());
    assert_eq!(analysis.tables.len(), 2);
    assert_eq!(analysis.tables[1].origin.line, 2);
}

#[test]
fn dynamic_layout_inert_ddl_is_silent() {
    // A dynamic template that parses to layout-irrelevant DDL (CREATE INDEX) is neither noted
    // nor counted unanalyzable — deleting the Irrelevant arm in dispatch_dynamic_op survived
    // the suite because no test exercised a benign dynamic statement.
    let sql = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);
        DO $$ BEGIN EXECUTE format('CREATE INDEX %I ON t (a)', nm); END $$;";
    let analysis = analyze_sources(&[src("V1__ix.sql", sql)], &Config::default());
    assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    assert!(!analysis.tables[0].incomplete);
}

/// Property: a template with no `%` passes through substitute_format byte-identical — over
/// arbitrary unicode, which pins the char-width walk far wider than fixed samples.
mod format_identity_property {
    use crate::substitute_format;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn substitute_format_is_identity_without_percent(template in "[^%]{0,24}") {
            prop_assert_eq!(substitute_format(&template, "tok"), template);
        }
    }
}

#[cfg(feature = "pg-exact")]
mod pgq_partition_options {
    use crate::{ParserBackend, SqlSource, analyze_sources_with};

    #[test]
    fn with_options_children_gain_no_phantom_columns() {
        // `(col WITH OPTIONS ...)` arrives from the raw tree as a type-less ColumnDef; it
        // must not become a column of type "unknown" (a confirmed pre-fix gate failure on
        // valid PG DDL).
        let sql = "CREATE TABLE p (flag boolean NOT NULL, id bigint NOT NULL) PARTITION BY RANGE (id);\n\
                   CREATE TABLE c PARTITION OF p (id WITH OPTIONS NOT NULL) FOR VALUES FROM (1) TO (2);";
        let analysis = analyze_sources_with(
            ParserBackend::PgExact,
            &[SqlSource {
                name: "V1__p.sql".into(),
                sql: sql.into(),
            }],
            &crate::Config::default(),
        );
        assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
        let child = analysis.tables.iter().find(|t| t.name == "c").unwrap();
        let parent = analysis.tables.iter().find(|t| t.name == "p").unwrap();
        assert_eq!(child.natts, parent.natts, "{child:#?}");
        assert_eq!(child.tier, parent.tier);
        assert!(child.assumed_types.is_empty());
        assert_eq!(child.avoidable_bytes_per_row, parent.avoidable_bytes_per_row);
    }
}

/// Regression pins for the hostile-audit batch (each reproduced pre-fix by the refute pass).
mod audit_fixes {
    use super::src;
    use crate::{Config, NoteKind, analyze_sources};

    #[test]
    fn marker_in_string_literal_does_not_exempt() {
        let sql = "CREATE TABLE t (a boolean NOT NULL, b bigint NOT NULL, note text DEFAULT 'rowdiet:ignore');";
        let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
        assert!(!analysis.tables[0].ignored);
    }

    #[test]
    fn marker_above_statement_gets_a_note() {
        let sql = "-- rowdiet:ignore\nCREATE TABLE t (a boolean NOT NULL, b bigint NOT NULL);";
        let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
        assert!(!analysis.tables[0].ignored);
        let note = analysis
            .notes
            .iter()
            .find(|n| n.kind == NoteKind::UnusedIgnoreMarker)
            .expect("stranded marker note");
        assert_eq!(note.origin.line, 1);
    }

    #[test]
    fn attached_marker_still_works_and_produces_no_stranded_note() {
        let sql = "CREATE TABLE t ( -- rowdiet:ignore\n a boolean, b bigint);";
        let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
        assert!(analysis.tables[0].ignored);
        assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    }

    #[test]
    fn rename_onto_existing_table_is_loud() {
        let sql = "CREATE TABLE a (x boolean NOT NULL, y bigint NOT NULL);
            CREATE TABLE b (z int NOT NULL);
            ALTER TABLE a RENAME TO b;";
        let analysis = analyze_sources(&[src("V1__r.sql", sql)], &Config::default());
        assert_eq!(analysis.tables.len(), 1);
        assert!(
            analysis
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Redefined && n.detail.contains("rename")),
            "{:?}",
            analysis.notes
        );
    }

    #[test]
    fn temporary_tables_are_skipped_with_a_note() {
        let sql = "CREATE TEMPORARY TABLE scratch (a boolean NOT NULL, b bigint NOT NULL);
            CREATE TABLE keep (a bigint NOT NULL);";
        let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
        assert_eq!(analysis.tables.len(), 1);
        assert_eq!(analysis.tables[0].name, "keep");
        assert!(
            analysis.notes.iter().any(|n| n.kind == NoteKind::TempTableSkipped),
            "{:?}",
            analysis.notes
        );
    }

    #[test]
    fn suggested_stats_match_the_suggested_order() {
        // Rung-not-crossed fixture: nothing avoidable, so both the order AND the stats must
        // describe the current layout (previously suggested.padding said 0 beside the original
        // order whose padding is 7).
        let sql = "CREATE TABLE t (flag boolean NOT NULL, a bigint NOT NULL, b timestamptz NOT NULL);";
        let analysis = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.avoidable_bytes_per_row, 0);
        assert_eq!(t.suggested, t.current);
    }

    #[test]
    fn assume_type_length_bounds_are_enforced() {
        use crate::catalog::parse_assume_spec;
        assert!(parse_assume_spec("huge=fixed:18446744073709551615:d").is_err());
        assert!(parse_assume_spec("zero=fixed:0:c").is_err());
        assert!(parse_assume_spec("name=fixed:64:c").is_ok());
        assert!(parse_assume_spec("big=fixed:32767:d").is_ok());
    }

    #[test]
    fn skipped_qualified_quoted_target_still_flags_the_table() {
        // sqlparser cannot parse LIKE ... INCLUDING; the sniffer must still resolve the
        // schema-qualified quoted name instead of collapsing it to an empty string.
        let sql = r#"CREATE TABLE myschema."My Table" (a bigint NOT NULL);
            ALTER TABLE myschema."My Table" ADD COLUMN broken_seq int, ADD woops;"#;
        let analysis = analyze_sources(&[src("V1__q.sql", sql)], &Config::default());
        let note = &analysis.notes[0];
        assert_eq!(note.kind, NoteKind::SkippedStatement);
        assert!(note.detail.contains("My Table"), "{}", note.detail);
        assert!(analysis.tables[0].incomplete, "{:#?}", analysis.tables);
    }

    #[test]
    fn dynamic_concrete_create_and_drop_get_targeted_notes() {
        let sql = "CREATE TABLE t (a bigint NOT NULL);
            DO $$ BEGIN EXECUTE format('CREATE TABLE audit_log (id %s)', ty); END $$;
            DO $$ BEGIN EXECUTE 'DROP TABLE t'; END $$;";
        let analysis = analyze_sources(&[src("V1__d.sql", sql)], &Config::default());
        assert!(
            analysis
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::DoBlockDdl && n.detail.contains("audit_log")),
            "{:?}",
            analysis.notes
        );
        assert!(
            analysis
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::DoBlockDdl && n.detail.contains("DROP TABLE")),
            "{:?}",
            analysis.notes
        );
        assert!(
            !analysis
                .notes
                .iter()
                .any(|n| n.detail.contains("not statically analyzable")),
            "{:?}",
            analysis.notes
        );
    }

    #[test]
    fn hostile_do_body_is_capped_not_quadratic() {
        let body: String = (0..500).map(|i| format!("x{i} create ")).collect();
        let sql = format!("CREATE TABLE t (a bigint NOT NULL);\nDO $$ BEGIN {body}; END $$;");
        let started = std::time::Instant::now();
        let analysis = analyze_sources(&[src("V1__h.sql", &sql)], &Config::default());
        assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
        assert!(
            analysis
                .notes
                .iter()
                .any(|n| n.detail.contains("not statically analyzable")),
            "{:?}",
            analysis.notes
        );
    }
}

mod audit_fixes_model {
    use super::src;
    use crate::{Config, analyze_sources};

    #[test]
    fn dropped_columns_keep_the_original_width_bitmap() {
        // Verified against live PostgreSQL 17 pageinspect: 10 int4 columns, one dropped —
        // new rows carry t_hoff 32 (23 + 2-byte bitmap for natts=10, MAXALIGNed), so the
        // footprint is 72 and 107 rows fit a page, not the naive 64/120.
        let sql = "CREATE TABLE w (c1 int NOT NULL, c2 int NOT NULL, c3 int NOT NULL, c4 int NOT NULL, c5 int NOT NULL, c6 int NOT NULL, c7 int NOT NULL, c8 int NOT NULL, c9 int NOT NULL, c10 int NOT NULL);
            ALTER TABLE w DROP COLUMN c5;";
        let analysis = analyze_sources(&[src("V1__w.sql", sql)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.natts, 9);
        assert_eq!(t.dropped_columns, 1);
        assert_eq!(t.current.footprint, Some(72));
        assert_eq!(t.current.rows_per_page, Some(107));
        assert_eq!(t.avoidable_bytes_per_row, 0);
    }

    #[test]
    fn undropped_table_keeps_the_bare_header() {
        let sql = "CREATE TABLE w (c1 int NOT NULL, c2 int NOT NULL, c3 int NOT NULL, c4 int NOT NULL, c5 int NOT NULL, c6 int NOT NULL, c7 int NOT NULL, c8 int NOT NULL, c9 int NOT NULL, c10 int NOT NULL);";
        let analysis = analyze_sources(&[src("V1__w.sql", sql)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.dropped_columns, 0);
        assert_eq!(t.current.footprint, Some(64));
        assert_eq!(t.current.rows_per_page, Some(120));
    }

    #[test]
    fn drop_shift_does_not_change_avoidable() {
        // The bitmap shift applies to current and suggested equally; the reorder delta must
        // survive a drop untouched.
        let sql = "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL, e int NOT NULL, f int NOT NULL, g int NOT NULL, h int NOT NULL, i int NOT NULL);
            ALTER TABLE m DROP COLUMN e;";
        let analysis = analyze_sources(&[src("V1__m.sql", sql)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.dropped_columns, 1);
        assert_eq!(t.avoidable_bytes_per_row, 8);
    }

    #[test]
    fn partition_children_inherit_the_dropped_slots() {
        let sql = "CREATE TABLE p (a int NOT NULL, b bigint NOT NULL, junk int NOT NULL) PARTITION BY RANGE (b);
            ALTER TABLE p DROP COLUMN junk;
            CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);";
        let analysis = analyze_sources(&[src("V1__p.sql", sql)], &Config::default());
        let child = analysis.tables.iter().find(|t| t.name == "c").unwrap();
        assert_eq!(child.dropped_columns, 1);
        assert_eq!(child.current.footprint, analysis.tables[0].current.footprint);
    }
}

mod audit_fixes_gate {
    use super::src;
    use crate::{Config, analyze_sources, baseline};

    #[test]
    fn degradation_is_surfaced_and_optionally_gating() {
        let sql = "CREATE TABLE ok (a bigint NOT NULL);\nALTER TABLE ok ADD COLUMN x @@@ bad;";
        let analysis = analyze_sources(&[src("V1__b.sql", sql)], &Config::default());
        let lenient = baseline::evaluate(&analysis, Some(0), false, None);
        assert!(lenient.skipped_statements > 0);
        assert!(lenient.incomplete_tables > 0);
        assert!(!lenient.exceeded, "{lenient:#?}");
        let strict = baseline::evaluate(&analysis, Some(0), true, None);
        assert!(strict.exceeded);
    }
}

#[cfg(feature = "pg-exact")]
mod keying_portability {
    use crate::{Config, ParserBackend, SqlSource, analyze_sources_with};

    #[test]
    fn mixed_case_names_key_identically_across_backends() {
        let sql = "CREATE TABLE MyTable (flag boolean NOT NULL, id bigint NOT NULL);";
        let src = SqlSource {
            name: "V1__m.sql".into(),
            sql: sql.into(),
        };
        let a = analyze_sources_with(ParserBackend::Sqlparser, std::slice::from_ref(&src), &Config::default());
        let b = analyze_sources_with(ParserBackend::PgExact, &[src], &Config::default());
        assert_eq!(a.tables[0].name, "mytable");
        assert_eq!(b.tables[0].name, "mytable");
        assert_eq!(a.tables[0].display, "MyTable");
        assert_eq!(a.tables[0].layout_signature, b.tables[0].layout_signature);
    }
}

mod schema_qualification {
    use super::src;
    use crate::{Config, analyze_sources};

    #[test]
    fn same_named_tables_in_different_schemas_are_distinct() {
        // The jvm-session reproducer: pre-fix these collided on the unqualified fold key and
        // the first relation vanished with only a redefined note.
        let sql = "CREATE SCHEMA a;\nCREATE SCHEMA b;\n\
                   CREATE TABLE a.things (flag boolean NOT NULL, id bigint NOT NULL);\n\
                   CREATE TABLE b.things (id bigint NOT NULL, flag boolean NOT NULL);";
        let analysis = analyze_sources(&[src("V1__s.sql", sql)], &Config::default());
        assert_eq!(analysis.tables.len(), 2, "{:#?}", analysis.tables);
        assert_eq!(analysis.tables[0].name, "a.things");
        assert_eq!(analysis.tables[1].name, "b.things");
        assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
        assert_ne!(analysis.tables[0].current.padding, analysis.tables[1].current.padding);
    }

    #[test]
    fn qualified_alters_fold_onto_qualified_creates() {
        let sql = "CREATE TABLE app.users (flag boolean NOT NULL, id bigint NOT NULL);\n\
                   ALTER TABLE app.users ADD COLUMN n int NOT NULL;";
        let analysis = analyze_sources(&[src("V1__q.sql", sql)], &Config::default());
        assert_eq!(analysis.tables[0].name, "app.users");
        assert_eq!(analysis.tables[0].natts, 3);
        assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    }
}

mod postaudit_pins {
    use super::src;
    use crate::{Config, NoteKind, analyze_sources, baseline};

    #[test]
    fn clean_analysis_passes_even_with_fail_on_degraded() {
        let analysis = analyze_sources(
            &[src("V1__c.sql", "CREATE TABLE ok (a bigint NOT NULL);")],
            &Config::default(),
        );
        let strict = baseline::evaluate(&analysis, Some(0), true, None);
        assert_eq!(strict.skipped_statements, 0);
        assert_eq!(strict.incomplete_tables, 0);
        assert!(!strict.exceeded, "{strict:#?}");
    }

    #[test]
    fn accept_matches_the_display_spelling_too() {
        let sql = "CREATE TABLE MyTable (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);";
        let analysis = analyze_sources(&[src("V1__m.sql", sql)], &Config::default());
        let mut base = baseline::Baseline {
            rowdiet: "test".into(),
            fail_over: 0,
            tables: std::collections::BTreeMap::new(),
        };
        baseline::accept_tables(&mut base, &analysis, &["MyTable".into()]).unwrap();
        assert!(base.tables.contains_key("mytable"), "{base:?}");
    }

    #[test]
    fn self_rename_keeps_the_table() {
        // ALTER TABLE a RENAME TO A folds onto the same key; the collision guard must not
        // treat it as replacing an existing table (a mutated guard deleted the table).
        let sql = "CREATE TABLE a (flag boolean NOT NULL, id bigint NOT NULL);\nALTER TABLE a RENAME TO A;";
        let analysis = analyze_sources(&[src("V1__a.sql", sql)], &Config::default());
        assert_eq!(analysis.tables.len(), 1);
        assert!(analysis.notes.is_empty(), "{:?}", analysis.notes);
    }

    #[test]
    fn drop_bitmap_boundary_at_nine_original_columns() {
        // live 8 + dropped 1 = 9 original attributes: bitmap pushes t_hoff 24 -> 32. A wrong
        // combination (multiplying instead of adding the counts) lands back under the
        // 8-attribute boundary and reports 56.
        let cols: String = (1..=9).map(|i| format!("c{i} int NOT NULL, ")).collect();
        let sql = format!(
            "CREATE TABLE w ({}); ALTER TABLE w DROP COLUMN c9;",
            cols.trim_end_matches(", ")
        );
        let analysis = analyze_sources(&[src("V1__w.sql", &sql)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.natts, 8);
        assert_eq!(t.dropped_columns, 1);
        assert_eq!(t.current.footprint, Some(64));
    }

    #[test]
    fn placeholder_named_dynamic_create_and_drop_stay_summarized() {
        // A placeholder in the TARGET name cannot become a targeted note; it must fall to the
        // loud summary (mutating the concreteness guards to true routed it to a note naming
        // the placeholder token).
        let sql = "DO $$ BEGIN EXECUTE format('CREATE TABLE %I (id int)', nm); END $$;\n\
                   DO $$ BEGIN EXECUTE format('DROP TABLE %I', nm); END $$;";
        let analysis = analyze_sources(&[src("V1__p.sql", sql)], &Config::default());
        assert_eq!(analysis.notes.len(), 2, "{:#?}", analysis.notes);
        for note in &analysis.notes {
            assert!(note.detail.contains("not statically analyzable"), "{}", note.detail);
            assert!(!note.detail.contains("rowdiet_dyn"), "{}", note.detail);
        }
    }

    #[test]
    fn do_scan_cap_returns_unanalyzable_not_a_late_parse() {
        // Keyword #34 would parse; the cap (32 attempts) must fire first. An uncapped scan
        // reaches it and emits a conditional CREATE note instead of the summary.
        let noise: String = (0..33).map(|i| format!("k{i} create ")).collect();
        let sql = format!("DO $$ BEGIN {noise} create table capx (id int); END $$;");
        let analysis = analyze_sources(&[src("V1__cap.sql", &sql)], &Config::default());
        assert!(
            analysis
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::DoBlockDdl && n.detail.contains("not statically analyzable")),
            "{:#?}",
            analysis.notes
        );
        assert!(
            !analysis.notes.iter().any(|n| n.detail.contains("capx")),
            "{:#?}",
            analysis.notes
        );
    }
}
