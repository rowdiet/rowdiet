use crate::report::SavingRange;
use crate::*;

fn src(name: &str, sql: &str) -> SqlSource {
    SqlSource::new(name, sql)
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
    // note precedes created_at, so created_at's 8-byte pad is data-dependent: E 3.5 over the
    // full residue set on top of the 7 certain bytes.
    assert_eq!(t.current.expected_padding, 10.5);
    assert_eq!((t.current.padding_min, t.current.padding_max), (7, 14));
    // The suggestion hands note the aligned slot at offset 20 (pads zero in every storage
    // form) and parks the boolean behind it: only meta's long-form pad can remain.
    assert_eq!(t.suggested.padding, 0);
    assert_eq!((t.suggested.padding_min, t.suggested.padding_max), (0, 3));
    // Dominance-proven: 7 B of certain padding removed, saving 4-14 B/row in every realization.
    assert_eq!(t.avoidable_bytes_per_row, 14.0);
    assert_eq!(t.avoidable_deterministic, 7);
    assert_eq!(t.avoidable_dominance, 7);
    assert_eq!(t.dominance_saving, Some(SavingRange { min: 4, max: 14 }));
    assert_eq!(
        t.suggested_order,
        vec!["id", "created_at", "status", "note", "flag", "meta"]
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
    assert_eq!(t.avoidable_bytes_per_row, 8.0);
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
    assert_eq!(t.avoidable_bytes_per_row, 0.0);
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
    assert_eq!(t.avoidable_bytes_per_row, 0.0);
}

#[test]
fn serial_primary_key_table_already_optimal() {
    let sql = "CREATE TABLE s (id bigserial PRIMARY KEY, active boolean NOT NULL);";
    let analysis = analyze_sources(&[src("V1__s.sql", sql)], &Config::default());
    let t = &analysis.tables[0];
    assert_eq!(t.tier, Tier::Exact);
    assert_eq!(t.avoidable_bytes_per_row, 0.0);
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
                like_source,
            } => DdlOp::CreateTable {
                name: norm_name(name),
                columns: columns.into_iter().map(norm_col).collect(),
                pk_columns,
                if_not_exists,
                is_ctas,
                incomplete_columns,
                temporary,
                partition_of: partition_of.map(norm_name),
                like_source: like_source.map(norm_name),
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
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
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
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
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
        assert_eq!(t.avoidable_bytes_per_row, 8.0);
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
        let lenient = baseline::evaluate(&analysis, Some(0.0), false, None);
        assert!(lenient.skipped_statements > 0);
        assert!(lenient.incomplete_tables > 0);
        assert!(!lenient.exceeded, "{lenient:#?}");
        let strict = baseline::evaluate(&analysis, Some(0.0), true, None);
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
        let strict = baseline::evaluate(&analysis, Some(0.0), true, None);
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
            fail_over: 0.0,
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
    fn a_schema_qualified_rename_stays_in_its_schema() {
        // ALTER TABLE s.t RENAME TO n leaves the table in s; keying it as bare n made a later
        // CREATE TABLE n redefine it and dropped every later ALTER of s.n.
        let sql = "CREATE TABLE rs.old (m macaddr, t text NOT NULL, s smallint NOT NULL);
            ALTER TABLE rs.old RENAME TO new;
            CREATE TABLE new (id bigint NOT NULL, flag boolean NOT NULL);
            ALTER TABLE rs.new ADD COLUMN extra int4;
            CREATE TABLE \"Q.x\".\"Old\" (a int NOT NULL);
            ALTER TABLE \"Q.x\".\"Old\" RENAME TO \"New\";
            ALTER TABLE \"Q.x\".\"New\" ADD COLUMN b int;";
        #[cfg_attr(not(feature = "pg-exact"), allow(unused_mut))]
        let mut backends = vec![crate::ParserBackend::Sqlparser];
        #[cfg(feature = "pg-exact")]
        backends.push(crate::ParserBackend::PgExact);
        for backend in backends {
            let analysis = crate::analyze_sources_with(backend, &[src("V1__r.sql", sql)], &Config::default());
            let tables: Vec<(&str, usize)> = analysis.tables.iter().map(|t| (t.name.as_str(), t.natts)).collect();
            assert_eq!(tables, [("rs.new", 4), ("new", 2), ("Q.x.New", 2)], "{backend:?}");
            assert!(analysis.notes.is_empty(), "{backend:?}: {:?}", analysis.notes);
        }
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

#[test]
fn duplicate_column_in_create_is_loud_and_kept_once() {
    let sql = "CREATE TABLE account (
        active boolean NOT NULL,
        id bigint PRIMARY KEY,
        kind smallint NOT NULL,
        kind smallint NOT NULL,
        balance bigint NOT NULL
    );";
    let analysis = analyze_sources(&[src("V1__dup.sql", sql)], &Config::default());
    let table = &analysis.tables[0];
    assert_eq!(table.natts, 4, "first occurrence kept, duplicate dropped");
    assert!(
        table.incomplete,
        "an unapplyable statement must not model a clean table"
    );
    assert_eq!(
        analysis
            .notes
            .iter()
            .filter(|n| n.kind == NoteKind::DuplicateColumn)
            .count(),
        1,
        "{:?}",
        analysis.notes
    );
    let gate = crate::baseline::evaluate(&analysis, None, true, None);
    assert!(gate.exceeded, "fail-on-degraded must catch the duplicate");
    #[cfg(feature = "pg-exact")]
    {
        let exact = analyze_sources_with(ParserBackend::PgExact, &[src("V1__dup.sql", sql)], &Config::default());
        assert_eq!(exact.tables[0].natts, 4);
        assert!(exact.tables[0].incomplete);
    }
}

#[test]
fn duplicate_check_follows_identifier_folding() {
    // Unquoted identifiers fold, so KIND duplicates kind; a quoted "KIND" is a distinct column.
    let folded = analyze_sources(
        &[src(
            "V1__f.sql",
            "CREATE TABLE t (kind smallint NOT NULL, KIND smallint NOT NULL);",
        )],
        &Config::default(),
    );
    assert_eq!(folded.tables[0].natts, 1);
    assert!(folded.tables[0].incomplete);
    let quoted = analyze_sources(
        &[src(
            "V1__q.sql",
            "CREATE TABLE t (kind smallint NOT NULL, \"KIND\" smallint NOT NULL);",
        )],
        &Config::default(),
    );
    assert_eq!(quoted.tables[0].natts, 2, "{:?}", quoted.notes);
    assert!(!quoted.tables[0].incomplete);
    assert!(quoted.notes.is_empty());
}

#[test]
fn analysis_accessors_mirror_the_gate_filter() {
    let wasteful = src(
        "V1__w.sql",
        "CREATE TABLE w (a boolean NOT NULL, b bigint NOT NULL, c boolean NOT NULL, d bigint NOT NULL);",
    );
    let ignored = src(
        "V2__i.sql",
        "CREATE TABLE i (-- rowdiet:ignore\n a boolean NOT NULL, b bigint NOT NULL);",
    );
    let analysis = analyze_sources(&[wasteful, ignored], &Config::default());
    assert_eq!(analysis.tables.len(), 2);
    let gated: Vec<&str> = analysis.gated_tables().map(|t| t.name.as_str()).collect();
    assert_eq!(gated, ["w"], "the ignored table must be outside the gate filter");
    assert_eq!(analysis.worst_avoidable(), analysis.tables[0].avoidable_bytes_per_row);
    assert!(analysis.worst_avoidable() > 0.0);
    let empty = analyze_sources(&[], &Config::default());
    assert_eq!(empty.worst_avoidable(), 0.0);
}

#[test]
fn like_expands_from_a_known_same_run_source() {
    // Plain `(LIKE s)` copies s's columns verbatim, so the copy carries s's exact layout — same
    // signature, same avoidable waste — instead of landing incomplete.
    let a = analyze_sources(
        &[src(
            "V1.sql",
            "CREATE TABLE s (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
             CREATE TABLE cp (LIKE s);",
        )],
        &Config::default(),
    );
    let s = a.tables.iter().find(|t| t.name == "s").unwrap();
    let cp = a.tables.iter().find(|t| t.name == "cp").unwrap();
    assert!(!cp.incomplete, "a LIKE of a known table expands");
    assert_eq!(cp.natts, 4);
    assert_eq!(cp.layout_signature, s.layout_signature);
    assert_eq!(cp.avoidable_bytes_per_row, s.avoidable_bytes_per_row);
    assert_eq!(cp.avoidable_bytes_per_row, 8.0);
    assert!(a.notes.is_empty(), "{:#?}", a.notes);
}

#[test]
fn incomplete_table_reports_unknown_not_a_false_pass() {
    // A LIKE of a table not in the analyzed set cannot be expanded — it must not look like a
    // clean, fully-analyzed pass (the false negative behind the report).
    let a = analyze_sources(&[src("V1.sql", "CREATE TABLE c (LIKE nowhere);")], &Config::default());
    let c = &a.tables[0];
    assert!(c.incomplete);
    assert_eq!(c.tier, layout::Tier::Unknown);
    assert_eq!(c.current.footprint, None);
    assert_eq!(c.avoidable_bytes_per_row, 0.0);
    let outcome = baseline::evaluate(&a, Some(0.0), false, None);
    assert_eq!(outcome.verdicts["c"], baseline::TableVerdict::Incomplete);
    assert!(!outcome.exceeded, "incomplete alone does not fail the gate");
    assert!(
        baseline::evaluate(&a, Some(0.0), true, None).exceeded,
        "but --fail-on-degraded escalates it"
    );
    // INHERITS of an unknown parent is the same class — the unknown/incomplete path must be
    // exercised by a fixture, since a corpus where every LIKE expands never reaches it.
    let inh = analyze_sources(
        &[src("V3.sql", "CREATE TABLE k () INHERITS (unknown_parent);")],
        &Config::default(),
    );
    assert!(inh.tables[0].incomplete);
    assert_eq!(inh.tables[0].tier, layout::Tier::Unknown);
    assert_eq!(
        baseline::evaluate(&inh, Some(0.0), false, None).verdicts["k"],
        baseline::TableVerdict::Incomplete
    );
    // A genuinely empty but complete table stays exact — the fix keys on incompleteness, not natts.
    let empty = analyze_sources(&[src("V2.sql", "CREATE TABLE e ();")], &Config::default());
    assert!(!empty.tables[0].incomplete);
    assert_eq!(empty.tables[0].tier, layout::Tier::Exact);
}

/// The decision-policy cases, each verified against pageinspect on PostgreSQL 16 (the
/// measured numbers live in the xtask measure fixtures): the gate and the recommendation rest
/// on deterministic pads and dominance only, and workload-dependent pairs surface as a
/// frontier instead of a finding in either direction.
mod decision_policy {
    use super::src;
    use crate::layout::SearchScope;
    use crate::report::{BandWinner, SavingRange};
    use crate::{Config, analyze_sources};

    #[test]
    fn short_form_varlena_alignment_is_not_charged() {
        // (int2, text) measures flat 0 padding on disk; the long-form pin used to print a
        // certain 2 B/row here through the unhedged branch. The swap trades bands, so it is
        // frontier material and never a finding.
        let a = analyze_sources(
            &[src("V1__t.sql", "CREATE TABLE t (n int2 NOT NULL, t text NOT NULL);")],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.current.padding, 0);
        assert_eq!((t.current.padding_min, t.current.padding_max), (0, 2));
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
        assert_eq!(t.columns[1].pad_before, None, "the pad depends on the storage form");
        assert_eq!(t.columns[1].offset, None);
        let frontier = t.frontier.as_ref().unwrap();
        assert_eq!(frontier.order, vec!["t", "n"]);
        assert_eq!((frontier.alternative_worst, frontier.current_worst), (1, 2));
    }

    #[test]
    fn stranded_fixed_column_is_a_dominance_finding() {
        // (text, int4) is dominated by (int4, text): the swap is never worse in any storage
        // form or payload length and saves up to 3 B/row. This is the rung-1 gate case.
        let a = analyze_sources(
            &[src("V1__t.sql", "CREATE TABLE t (t text NOT NULL, n int4 NOT NULL);")],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.avoidable_bytes_per_row, 3.0);
        assert_eq!(t.avoidable_deterministic, 0);
        assert_eq!(t.avoidable_dominance, 3);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 3 }));
        assert_eq!(t.suggested_order, vec!["n", "t"]);
        assert!(t.frontier.is_none());
    }

    #[test]
    fn workload_dependent_swap_is_a_frontier_and_never_gates() {
        // (text, macaddr) vs (macaddr, text): short payloads favor macaddr-first (measured
        // flat 0 vs a 1.5 B/row mean), long payloads with friendly residues favor text-first.
        // Neither dominates, so neither order gates and both see the boundary.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE t (t text NOT NULL, m macaddr NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
        assert_eq!(t.suggested_order, vec!["t", "m"], "no rewrite advice without dominance");
        let frontier = t.frontier.as_ref().unwrap();
        assert_eq!(frontier.order, vec!["m", "t"]);
        assert!(frontier.decided);
        let short_band = frontier.bands.iter().find(|b| b.long_form.is_empty()).unwrap();
        assert_eq!(
            short_band.winner,
            BandWinner::Alternative,
            "short payloads favor macaddr-first"
        );
        let long_band = frontier.bands.iter().find(|b| b.long_form == ["t"]).unwrap();
        assert_eq!(
            long_band.winner,
            BandWinner::Mixed,
            "long payloads flip with the residue"
        );
    }

    #[test]
    fn aligned_slot_and_char_tail_dominate() {
        // (text, boolean, bigint): the winning order hands the text the aligned slot behind
        // the bigint and parks the boolean last, reaching zero padding in every realization.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE t (t text NOT NULL, b boolean NOT NULL, x bigint NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.avoidable_bytes_per_row, 7.0);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 7 }));
        assert_eq!(t.suggested_order, vec!["x", "t", "b"]);
        assert_eq!((t.suggested.padding_min, t.suggested.padding_max), (0, 0));
    }

    #[test]
    fn d_aligned_array_first_is_kept_and_the_swap_is_not_recommended() {
        // (float8[], int4) measures flat 0 for whole-float8 payloads; the old model demanded
        // (int4, float8[]), which measures flat 4. Neither dominates; current order stays.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE a (arr float8[] NOT NULL, n int4 NOT NULL);
                 CREATE TABLE b (n int4 NOT NULL, arr float8[] NOT NULL);",
            )],
            &Config::default(),
        );
        let keep = &a.tables[0];
        assert_eq!(keep.avoidable_bytes_per_row, 0.0);
        assert!(keep.frontier.is_none(), "array-first is already the minimax pole");
        let swapped = &a.tables[1];
        assert_eq!(
            swapped.avoidable_bytes_per_row, 0.0,
            "the 4-flat band is the reader's call"
        );
        let frontier = swapped.frontier.as_ref().unwrap();
        assert_eq!(frontier.order, vec!["arr", "n"]);
        assert_eq!((frontier.alternative_worst, frontier.current_worst), (3, 4));
    }

    #[test]
    fn certainty_trade_is_reported_and_never_recommended() {
        // (timetz, timetz, text): interposing the text trades a certain 4 for 0..=7, which
        // measures 3 B/row worse on 4-byte-wide texts. The trade renders as a frontier with
        // the current order kept.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE t (t1 timetz NOT NULL, t2 timetz NOT NULL, v text NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
        assert_eq!(t.suggested_order, vec!["t1", "t2", "v"]);
        assert_eq!(t.current.padding, 4);
        let frontier = t.frontier.as_ref().unwrap();
        assert_eq!(frontier.order, vec!["t1", "v", "t2"]);
        assert_eq!(frontier.alternative_deterministic, 0);
        assert_eq!(frontier.alternative_worst, 7);
    }

    #[test]
    fn issue_10_band_pair_reports_the_boundary_instead_of_identical_silence() {
        // (int8, text, float8[]) vs (int8, float8[], text): which varlena receives the aligned
        // slot is workload knowledge (issue #10's W1/W2). The text-first order shows the
        // frontier with per-form bands; the array-first order is the minimax pole and passes.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE band_b (k int8 NOT NULL, txt text NOT NULL, arr float8[] NOT NULL);
                 CREATE TABLE band_a (k int8 NOT NULL, arr float8[] NOT NULL, txt text NOT NULL);",
            )],
            &Config::default(),
        );
        let text_first = &a.tables[0];
        assert_eq!(text_first.avoidable_bytes_per_row, 0.0);
        let frontier = text_first.frontier.as_ref().unwrap();
        assert_eq!(frontier.order, vec!["k", "arr", "txt"]);
        let arr_long = frontier.bands.iter().find(|b| b.long_form == ["arr"]).unwrap();
        assert_eq!(
            arr_long.winner,
            BandWinner::Alternative,
            "long arrays want the aligned slot"
        );
        let txt_long = frontier.bands.iter().find(|b| b.long_form == ["txt"]).unwrap();
        assert_eq!(txt_long.winner, BandWinner::Current, "long texts already own it");
        let array_first = &a.tables[1];
        assert_eq!(array_first.avoidable_bytes_per_row, 0.0);
    }

    #[test]
    fn capped_search_still_gates_deterministic_waste() {
        // The 25-column false negative: with the redundant column cap dropped, the whole-order
        // search runs (its state space fits the budget), the repack dominates, and the gate
        // fires at the full 24 B/row (both prior PRs' review blocker).
        let mut cols: Vec<String> = Vec::new();
        for i in 0..4 {
            cols.push(format!("tz{i} timetz NOT NULL"));
        }
        for i in 0..4 {
            cols.push(format!("m{i} macaddr NOT NULL"));
        }
        for i in 0..4 {
            cols.push(format!("b{i} bigint NOT NULL"));
        }
        for i in 0..4 {
            cols.push(format!("i{i} integer NOT NULL"));
        }
        for i in 0..4 {
            cols.push(format!("s{i} smallint NOT NULL"));
        }
        for i in 0..4 {
            cols.push(format!("f{i} boolean NOT NULL"));
        }
        cols.push("note text NOT NULL".into());
        let sql = format!("CREATE TABLE cliff25 ({});", cols.join(", "));
        let a = analyze_sources(&[src("V1__c.sql", &sql)], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.natts, 25);
        assert_eq!(t.current.padding, 24);
        assert_eq!(t.avoidable_bytes_per_row, 24.0);
        assert_eq!(t.avoidable_deterministic, 24);
        assert_eq!(t.search_scope, SearchScope::Complete);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 24, max: 24 }));
    }

    #[test]
    fn wide_fixed_table_keeps_its_refinement_at_25_columns() {
        // 25 columns, few classes: a column-count cap would skip the search here (a measured
        // false negative of an earlier revision); the state budget admits it and the repack
        // around the smallint is dominance-proven.
        let mut cols: Vec<String> = (0..21).map(|i| format!("b{i} bigint NOT NULL")).collect();
        cols.push("t1 timetz NOT NULL".into());
        cols.push("t2 timetz NOT NULL".into());
        cols.push("s smallint NOT NULL".into());
        cols.push("note text NOT NULL".into());
        let sql = format!("CREATE TABLE wide25 ({});", cols.join(", "));
        let a = analyze_sources(&[src("V1__w.sql", &sql)], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.current.padding, 4);
        assert_eq!(t.avoidable_bytes_per_row, 4.0);
        assert_eq!(t.avoidable_deterministic, 2);
        assert_eq!(t.avoidable_dominance, 2);
        assert_eq!(t.search_scope, SearchScope::Complete);
    }

    #[test]
    fn dominating_reorder_outside_the_poles_is_found_by_the_sweep() {
        // (text, bigint, macaddr): no scalar-objective pole proposes (bigint, text, macaddr),
        // yet it dominates (measured 1.5 vs 3.5 short-heavy, 1.5 vs 3.5 long-heavy, 0 vs 0 on
        // fixed 132-byte texts). The reviews clocked pole-only search missing 11-19% of
        // dominating reorders on 4-5 column schemas; the exhaustive sweep closes the class.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE t (t text NOT NULL, k bigint NOT NULL, m macaddr NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.suggested_order, vec!["k", "t", "m"]);
        assert_eq!(t.avoidable_bytes_per_row, 4.0);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 4 }));
        assert_eq!(t.dominance_search, crate::report::DominanceScope::Exhaustive);
    }

    #[test]
    fn headline_never_exceeds_the_proven_maximum_saving() {
        // 23 columns of irregulars and texts: the dominating repack removes 28 B of certain
        // padding but pays new data-dependent pads, so only 24 B is attainable. The headline
        // must equal the proven maximum instead of the raw certain-pad delta (a measured
        // self-contradiction in an earlier revision: 28.0 next to "saves 8-24").
        let mut cols: Vec<String> = Vec::new();
        for i in 0..6 {
            cols.push(format!("tz{i} timetz NOT NULL"));
        }
        for i in 0..6 {
            cols.push(format!("m{i} macaddr NOT NULL"));
        }
        for i in 0..5 {
            cols.push(format!("b{i} bigint NOT NULL"));
        }
        for i in 0..3 {
            cols.push(format!("s{i} smallint NOT NULL"));
        }
        for i in 0..3 {
            cols.push(format!("t{i} text NOT NULL"));
        }
        let sql = format!("CREATE TABLE m23 ({});", cols.join(", "));
        let a = analyze_sources(&[src("V1__m23.sql", &sql)], &Config::default());
        let t = &a.tables[0];
        let saving = t.dominance_saving.unwrap();
        assert!(
            t.avoidable_bytes_per_row <= saving.max as f64,
            "headline {} exceeds the proven maximum {}",
            t.avoidable_bytes_per_row,
            saving.max
        );
        assert_eq!(t.avoidable_bytes_per_row, 24.0);
        assert_eq!((saving.min, saving.max), (8, 24));
        assert_eq!(t.avoidable_deterministic + t.avoidable_dominance, saving.max);
    }

    #[test]
    fn one_extra_column_no_longer_flips_a_finding_to_a_pass() {
        // 25 columns, tiny state space: a fixed column-count cap silently passed this table at
        // exactly 25 columns while 24 gated 7.0 (a measured cliff); the budget-only cap keeps
        // the finding.
        let mut cols = vec!["t text NOT NULL".to_string(), "b boolean NOT NULL".to_string()];
        for i in 0..23 {
            cols.push(format!("x{i} bigint NOT NULL"));
        }
        let sql = format!("CREATE TABLE s25 ({});", cols.join(", "));
        let a = analyze_sources(&[src("V1__s25.sql", &sql)], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.natts, 25);
        assert_eq!(t.avoidable_bytes_per_row, 7.0);
        assert_eq!(t.search_scope, SearchScope::Complete);
        assert_eq!(t.suggested_order.last().map(String::as_str), Some("b"));
    }

    #[test]
    fn same_class_varlena_identities_are_searched_individually() {
        // (t1 text, m1 macaddr, t2 text, m2 macaddr): the dominating order (m1, t2, t1, m2)
        // swaps the two texts relative to their written order, so a candidate space that
        // collapses same-class varlenas never proposes it, while its class-sequence twin
        // (m1, t1, t2, m2) measures 4 B/row worse than the current order at t1 = 132 B. The
        // sweep must treat every varlena as its own individual.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE tmtm (t1 text NOT NULL, m1 macaddr NOT NULL, t2 text NOT NULL, m2 macaddr NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.suggested_order, vec!["m1", "t2", "t1", "m2"]);
        assert_eq!(t.avoidable_bytes_per_row, 4.0);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 4 }));
        assert_eq!(t.dominance_search, crate::report::DominanceScope::Exhaustive);
    }

    #[test]
    fn many_class_table_keeps_the_fixed_prefix_win() {
        // 10 distinct fixed padding classes (via assume-type) plus a text: the whole-order
        // search is over budget, and the reviews' false negative was a silent clean checkmark
        // on this shape. The fixed-prefix search must still surface the repack and gate it.
        let mut config = Config::default();
        for (name, spec) in [
            ("w3c", "fixed:3:c"),
            ("w5c", "fixed:5:c"),
            ("w3s", "fixed:3:s"),
            ("w5i", "fixed:5:i"),
        ] {
            let (key, kind) = crate::catalog::parse_assume_spec(&format!("{name}={spec}")).unwrap();
            config.assume.insert(key, kind);
        }
        let types = [
            "boolean", "smallint", "integer", "bigint", "timetz", "macaddr", "w3c", "w5c", "w3s", "w5i",
        ];
        let mut cols: Vec<String> = Vec::new();
        for (i, ty) in types.iter().enumerate() {
            cols.push(format!("c{i}a {ty} NOT NULL"));
            cols.push(format!("c{i}b {ty} NOT NULL"));
        }
        cols.push("note text NOT NULL".into());
        let sql = format!("CREATE TABLE cls ({});", cols.join(", "));
        let a = analyze_sources(&[src("V1__cls.sql", &sql)], &config);
        let t = &a.tables[0];
        assert_eq!(t.natts, 21);
        assert_eq!(t.search_scope, SearchScope::FixedPrefix, "over the whole-order budget");
        assert!(
            t.current.padding > 0,
            "the as-written order pads: {}",
            t.current.padding
        );
        assert!(
            t.avoidable_deterministic > 0,
            "a capped search must not print a silent clean verdict over deterministic waste: {t:#?}"
        );
        assert!(t.avoidable_bytes_per_row > 0.0);
    }
}

/// Wide and hostile tables from the closure review of ee5844e: every one terminates within a
/// wall-time bound, and none prints a clean verdict over a dominating reorder the engine can
/// decide.
mod closure_review {
    use super::src;
    use crate::layout::SearchScope;
    use crate::report::{DominanceScope, SavingRange, TableReport};
    use crate::{Config, analyze_sources};
    use std::time::{Duration, Instant};

    fn table_sql(name: &str, types: &[&str]) -> String {
        let cols: Vec<String> = types
            .iter()
            .enumerate()
            .map(|(i, ty)| format!("c{i} {ty} NOT NULL"))
            .collect();
        format!("CREATE TABLE {name} ({});", cols.join(", "))
    }

    /// Analyze on a worker thread and fail instead of hanging when it overruns `bound`.
    fn analyze_within(sql: String, bound: Duration) -> (TableReport, Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let analysis = analyze_sources(&[src("V1__h.sql", &sql)], &Config::default());
            let _ = tx.send((
                analysis.tables.into_iter().next().expect("one table"),
                started.elapsed(),
            ));
        });
        rx.recv_timeout(bound).expect("analysis overran its wall-time bound")
    }

    fn repeat(ty: &'static str, n: usize) -> Vec<&'static str> {
        vec![ty; n]
    }

    #[test]
    fn same_class_runs_of_256_and_more_terminate() {
        // 256 members of one padding class wrapped a u8 counter and spun the DP forever.
        let bound = Duration::from_secs(60);
        let (t, _) = analyze_within(table_sql("t256", &repeat("text", 256)), bound);
        assert_eq!(t.natts, 256);
        let mut types = repeat("timetz", 2);
        types.extend(repeat("integer", 300));
        let (t, _) = analyze_within(table_sql("tz_i300", &types), bound);
        // The exact tier takes the DP's padding minimum: one int4 between the two timetz.
        assert_eq!(t.avoidable_bytes_per_row, 8.0);
        assert_eq!(t.suggested.padding, 0);
        assert_eq!(t.dominance_search, DominanceScope::Exhaustive);
    }

    #[test]
    fn postgres_width_tables_terminate_within_the_bound() {
        let bound = Duration::from_secs(60);
        let seven = ["integer", "smallint", "boolean", "bigint", "timetz", "macaddr", "uuid"];
        let mut five_varlenas = repeat("integer", 1595);
        five_varlenas.extend(["text", "text", "text", "text", "float8[]"]);
        let mut seven_classes: Vec<&str> = (0..1595).map(|i| seven[i % 7]).collect();
        seven_classes.extend(["text", "text", "text", "text", "float8[]"]);
        let mut five_thousand = vec!["text"];
        five_thousand.extend(repeat("integer", 5039));
        for (name, types) in [
            ("w1600_5v", five_varlenas),
            ("w1600_7cls_5v", seven_classes),
            ("w5040", five_thousand),
        ] {
            let (t, elapsed) = analyze_within(table_sql(name, &types), bound);
            assert_eq!(t.natts, types.len(), "{name}");
            assert!(elapsed < bound, "{name} took {elapsed:?}");
        }
    }

    #[test]
    fn a_dominating_pole_is_recommended_when_the_sweep_runs_out_of_budget() {
        // Four varchars too short to compress and a text exhaust the sweep's comparison budget;
        // the minimax pole (id, note, ...) dominates the written order and used to print as a
        // workload-dependent frontier. Measured on PostgreSQL 16 for the varchar(8) spelling
        // the review used: 0.000 vs 0.736 B/row mean, no row worse.
        let a = analyze_sources(
            &[src(
                "V1__o.sql",
                "CREATE TABLE orders (id bigint NOT NULL, country varchar(2) NOT NULL, \
                 currency varchar(3) NOT NULL, status varchar(5) NOT NULL, channel varchar(4) NOT NULL, note text NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.dominance_search, DominanceScope::Budgeted);
        assert_eq!(t.avoidable_bytes_per_row, 3.0);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 3 }));
        assert_eq!(t.suggested_order[..2], ["id".to_string(), "note".to_string()]);
        assert!(t.frontier.is_none());
    }

    #[test]
    fn trimmed_sweeps_keep_the_findings_only_a_fallback_holds() {
        // Each sweep runs out of comparison budget before it reaches a dominating order, and only
        // the fallback candidates hold one; testing them only when the order space is too large
        // to sweep passed these tables clean. tests/oracle.rs verifies each finding.
        for types in [
            [
                "varchar(5)",
                "text",
                "jsonb",
                "varchar(8)",
                "macaddr",
                "smallint",
                "varchar(5)",
            ],
            [
                "macaddr",
                "jsonb",
                "varchar(8)",
                "text",
                "integer",
                "float8[]",
                "bigint",
            ],
            [
                "varchar(5)",
                "text",
                "integer",
                "varchar(8)",
                "integer",
                "varchar(5)",
                "varchar(8)",
            ],
        ] {
            let a = analyze_sources(&[src("V1__b.sql", &table_sql("b2", &types))], &Config::default());
            let t = &a.tables[0];
            assert_eq!(t.dominance_search, DominanceScope::Budgeted, "{types:?}");
            assert!(t.avoidable_bytes_per_row > 0.0, "{types:?} passed clean");
            assert!(t.dominance_saving.is_some(), "{types:?}");
        }
    }

    #[test]
    fn varchars_that_can_compress_keep_the_aligned_form() {
        // varchar(10) holds up to 40 bytes, and the toaster compresses an attribute over 24
        // bytes in line behind an aligned 4-byte header (lz4 has no minimum input). Modeled as
        // never aligned, the engine recommended (c0, c2, c3, c1) here, which PostgreSQL 16 stores
        // 1-3 B longer than the written order in 18 of 23 measured rows.
        let a = analyze_sources(
            &[src(
                "V1__w.sql",
                "CREATE TABLE w66 (c0 timetz, c1 varchar(10) NOT NULL, c2 macaddr NOT NULL, c3 text NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.avoidable_bytes_per_row, 0.0, "{:?}", t.suggested_order);
        assert_ne!(t.dominance_search, DominanceScope::Exhaustive);
        let small = analyze_sources(
            &[src(
                "V1__s.sql",
                "CREATE TABLE s (a smallint NOT NULL, v varchar(5) NOT NULL);",
            )],
            &Config::default(),
        );
        assert_eq!(
            small.tables[0].current.padding_max, 0,
            "varchar(5) never reaches 21 bytes"
        );
    }

    #[test]
    fn fixed_waste_past_24_fixed_columns_gates() {
        // 30 fixed columns and 6 varlenas: every pole reorders the varlenas out of the
        // enumeration budget, and the prefix repack was a no-op past 24 fixed columns, so 100 B
        // of deterministic padding passed clean. Measured: 100.806 vs 0.806 B/row.
        let mut types: Vec<&str> = Vec::new();
        for _ in 0..10 {
            types.extend(["boolean", "bigint"]);
        }
        for _ in 0..5 {
            types.extend(["smallint", "timestamptz"]);
        }
        types.extend(["varchar(20)", "text", "jsonb", "text", "numeric", "text[]"]);
        let a = analyze_sources(&[src("V1__w.sql", &table_sql("wide36", &types))], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.current.padding, 100);
        assert_eq!(t.avoidable_deterministic, 100);
        assert!(t.avoidable_bytes_per_row >= 100.0, "{}", t.avoidable_bytes_per_row);
        let saving = t.dominance_saving.expect("dominance-proven");
        assert!(saving.min >= 97, "{saving:?}");
    }

    #[test]
    fn fixed_waste_at_1600_columns_gates() {
        // The same cliff at PostgreSQL's column limit: 1,590 B/row of certain padding passed
        // clean with six varlenas.
        let bound = Duration::from_secs(60);
        let seven = ["integer", "smallint", "boolean", "bigint", "timetz", "macaddr", "uuid"];
        let mut types: Vec<&str> = (0..1594).map(|i| seven[i % 7]).collect();
        types.extend(["text", "text", "jsonb", "text", "numeric", "float8[]"]);
        let (t, _) = analyze_within(table_sql("w1600_6v", &types), bound);
        assert_eq!(t.current.padding, 1590);
        assert!(t.avoidable_deterministic > 0, "{}", t.avoidable_deterministic);
        assert!(t.dominance_saving.is_some());
    }

    #[test]
    fn exact_tier_past_24_columns_takes_the_search_minimum() {
        // 26 fixed columns: the exact tier used the capped heuristic and printed "nothing to
        // gain" while alternating timetz and int4 saves a MAXALIGN rung (measured 232 vs 236 B
        // tuples, 3 vs 4 pages per 100 rows).
        let mut types: Vec<&str> = Vec::new();
        for _ in 0..11 {
            types.extend(["timetz", "integer"]);
        }
        types.extend(["timetz", "timetz", "integer", "integer"]);
        let a = analyze_sources(&[src("V1__e.sql", &table_sql("exact26", &types))], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.current.padding, 4);
        assert_eq!(t.suggested.padding, 0);
        assert_eq!(t.avoidable_bytes_per_row, 8.0);
        assert_eq!(t.search_scope, SearchScope::Complete);
        assert_eq!(t.dominance_search, DominanceScope::Exhaustive);
    }

    #[test]
    fn short_arrays_keep_their_storable_residues() {
        // An uncompressed float8[] always stores payload 4 mod 8. Modeled with every residue,
        // the engine printed "no dominating reorder exists" here while (m, s, a2, a1) measures
        // never worse on 3,000 rows and 5.803 to 1.454 B/row on average.
        let a = analyze_sources(
            &[src(
                "V1__p.sql",
                "CREATE TABLE pin (s smallint NOT NULL, a1 float8[] NOT NULL, a2 float8[] NOT NULL, m macaddr NOT NULL);",
            )],
            &Config::default(),
        );
        let t = &a.tables[0];
        assert_eq!(t.suggested_order, ["m", "s", "a2", "a1"]);
        assert!(t.avoidable_bytes_per_row > 0.0);
        assert_eq!(t.dominance_search, DominanceScope::Exhaustive);
        assert!(t.superset_types.is_empty());
    }

    #[test]
    fn an_unverified_payload_model_withholds_the_absence_claim() {
        // inet stores 6 or 18 payload bytes and never the long form, which the model does not
        // know; a sweep over the wider model cannot prove that nothing dominates.
        let a = analyze_sources(
            &[src(
                "V1__i.sql",
                "CREATE TABLE i (a inet NOT NULL, b smallint NOT NULL); CREATE TABLE t (a text NOT NULL, b smallint NOT NULL);",
            )],
            &Config::default(),
        );
        let inet = &a.tables[0];
        assert_eq!(inet.avoidable_bytes_per_row, 0.0);
        assert_eq!(inet.dominance_search, DominanceScope::Superset);
        assert_eq!(inet.superset_types, ["inet"]);
        let text = &a.tables[1];
        assert_eq!(text.dominance_search, DominanceScope::Exhaustive);
        assert!(text.superset_types.is_empty());
    }

    #[test]
    fn a_capped_fixed_search_still_packs_the_irregulars() {
        // 49 fixed columns in 7 classes put the block search over budget; the sort keeps every
        // timetz and macaddr padding, and the capped table passed as "nothing to gain".
        // Measured on PostgreSQL 16: 403 B tuples as written, 367 B hand-packed.
        let mut types: Vec<&str> = Vec::new();
        for ty in ["bigint", "timetz", "integer", "macaddr", "smallint"] {
            types.extend([ty; 7]);
        }
        for _ in 0..7 {
            types.extend(["boolean", "uuid"]);
        }
        let a = analyze_sources(&[src("V1__f.sql", &table_sql("fbs", &types))], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.search_scope, SearchScope::SortOnly);
        assert_eq!(t.current.padding, 36);
        assert_eq!(t.suggested.padding, 0);
        assert_eq!((t.current.footprint, t.suggested.footprint), (Some(408), Some(368)));
        assert_eq!(t.avoidable_bytes_per_row, 40.0);
    }

    #[test]
    fn a_capped_exact_search_claims_no_exhaustiveness() {
        // Seven fixed classes of 40 columns each put both searches over budget; the heuristic
        // sort still pads, and the labels must say the search was capped.
        let seven = ["integer", "smallint", "boolean", "bigint", "timetz", "macaddr", "uuid"];
        let types: Vec<&str> = (0..280).map(|i| seven[i % 7]).collect();
        let a = analyze_sources(&[src("V1__c.sql", &table_sql("capped", &types))], &Config::default());
        let t = &a.tables[0];
        assert_eq!(t.search_scope, SearchScope::SortOnly);
        assert_eq!(t.dominance_search, DominanceScope::Budgeted);
        assert!(t.avoidable_bytes_per_row > 0.0, "the sort still beats round-robin");
    }
}

/// The issue-1 repro: identical column multisets, opposite orders. Interleaving fixed columns
/// among varlenas strands them at data-dependent offsets; grouping places every fixed column
/// while the offset is still exact.
mod varlena_residue_uncertainty {
    use super::src;
    use crate::{Config, Tier, analyze_sources};

    const INTERLEAVED: &str = "CREATE TABLE interleaved (a text NOT NULL, tag int4 NOT NULL, b text NOT NULL, \
        c text NOT NULL, d text NOT NULL, e text NOT NULL, score float8 NOT NULL, seen timestamp NOT NULL);";
    const GROUPED: &str = "CREATE TABLE grouped (score float8 NOT NULL, seen timestamp NOT NULL, tag int4 NOT NULL, \
        a text NOT NULL, b text NOT NULL, c text NOT NULL, d text NOT NULL, e text NOT NULL);";

    #[test]
    fn interleaved_reports_dominance_avoidable_and_surfaces_the_reorder() {
        // Grouping dominates interleaving here: never worse in any storage-form/payload
        // realization, and up to 11 B/row better. The display expectation stays 5.0
        // (pageinspect on independently varying short payloads measures a 4.5-5.1 B/row mean).
        let analysis = analyze_sources(&[src("V1__i.sql", INTERLEAVED)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.tier, Tier::Estimate);
        assert_eq!(t.current.padding, 0, "no pad in this order is certain");
        assert_eq!(t.current.expected_padding, 5.0);
        assert_eq!((t.current.padding_min, t.current.padding_max), (0, 19));
        assert_eq!((t.suggested.padding_min, t.suggested.padding_max), (0, 12));
        assert_eq!(t.avoidable_bytes_per_row, 11.0);
        assert_eq!(t.avoidable_deterministic, 0);
        assert_eq!(t.avoidable_dominance, 11);
        assert_eq!(t.dominance_saving, Some(crate::report::SavingRange { min: 0, max: 11 }));
        assert_eq!(t.suggested_order, vec!["score", "seen", "tag", "a", "b", "c", "d", "e"]);
    }

    #[test]
    fn grouped_reports_zero_avoidable() {
        // The issue's control table: pageinspect measures it flat at zero padding, and the
        // expectation must agree; only the long-form max (0-12) is data-dependent.
        let analysis = analyze_sources(&[src("V1__g.sql", GROUPED)], &Config::default());
        let t = &analysis.tables[0];
        assert_eq!(t.tier, Tier::Estimate);
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
        assert_eq!(t.current.expected_padding, 0.0);
        assert_eq!(t.current.padding, 0);
        assert_eq!((t.current.padding_min, t.current.padding_max), (0, 12));
        assert_eq!(t.suggested, t.current);
        assert_eq!(t.suggested_order, vec!["score", "seen", "tag", "a", "b", "c", "d", "e"]);
        assert!(t.frontier.is_none(), "the control table is its own minimax pole");
        // Five individually-distinct texts put the exhaustive sweep out of budget; the clean
        // verdict must say so instead of claiming nonexistence.
        assert_eq!(t.dominance_search, crate::report::DominanceScope::Budgeted);
    }
}

/// NULL presence as a realization variable: a NULL stores nothing, so a nullable column moves
/// later offsets differently in rows that hold it, and the gate and the recommendation quantify
/// over NULL patterns too.
mod null_masks {
    use super::src;
    use crate::report::{BandWinner, DominanceScope, NullRows, PaddingBounds, SavingRange};
    use crate::{Config, Tier, analyze_sources};

    fn one(sql: &str) -> crate::TableReport {
        let a = analyze_sources(&[src("V1__t.sql", sql)], &Config::default());
        a.tables.into_iter().next().expect("one table")
    }

    #[test]
    fn a_recommendation_that_loses_in_a_null_row_is_withdrawn() {
        // (m macaddr, t text NOT NULL, s smallint NOT NULL): without NULLs (m, s, t) is never
        // worse and the NULL-blind engine recommended it ("saves 0-3 in every realization"). In
        // a row where m is NULL the smallint takes offset 0 and a long text pads 2 behind it,
        // while the written order pads at most 1, so the swap is a frontier.
        let t = one("CREATE TABLE t (m macaddr, t text NOT NULL, s smallint NOT NULL);");
        assert_eq!(t.avoidable_bytes_per_row, 0.0);
        assert_eq!(t.null_variables, vec!["m"]);
        let frontier = t.frontier.as_ref().expect("the swap is reported, not recommended");
        assert_eq!(frontier.order, vec!["m", "s", "t"]);
        let rows = frontier.without_nulls.as_ref().expect("NULLs change the verdict");
        assert_eq!(rows.winner, BandWinner::Alternative);
        assert_eq!((rows.min_saving, rows.max_saving), (0, 3));
        // NOT NULL removes the variable and the reorder is proven again.
        let nn = one("CREATE TABLE t (m macaddr NOT NULL, t text NOT NULL, s smallint NOT NULL);");
        assert_eq!(nn.avoidable_bytes_per_row, 3.0);
        assert_eq!(nn.suggested_order, vec!["m", "s", "t"]);
        assert!(nn.null_variables.is_empty());
    }

    #[test]
    fn waste_that_only_null_rows_carry_is_found() {
        // (s int2, n int2 NULL, i int4, t text) pads zero when every column is stored, which
        // the NULL-blind engine reported as clean; rows with n NULL pad 2 before i.
        let t = one("CREATE TABLE t (s smallint NOT NULL, n smallint, i integer NOT NULL, t text NOT NULL);");
        assert_eq!(t.current.without_nulls, Some(PaddingBounds { min: 0, max: 0 }));
        assert_eq!((t.current.padding_min, t.current.padding_max), (0, 2));
        assert_eq!(t.avoidable_bytes_per_row, 2.0);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 2 }));
        assert_eq!(t.suggested_order, vec!["i", "s", "n", "t"]);
        assert_eq!(t.dominance_search, DominanceScope::Exhaustive);
    }

    #[test]
    fn exact_tier_reports_both_null_scenarios_and_the_bitmap_header() {
        // Issue #4's cols9: rows holding a NULL carry a 32-byte header, so a one-NULL row keeps
        // the full 96 bytes; cols8 still fits its bitmap in 24.
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE cols9 (c1 int8,c2 int8,c3 int8,c4 int8,c5 int8,c6 int8,c7 int8,c8 int8,c9 int8);
                 CREATE TABLE cols8 (c1 int8,c2 int8,c3 int8,c4 int8,c5 int8,c6 int8,c7 int8,c8 int8);",
            )],
            &Config::default(),
        );
        let cols9 = &a.tables[0];
        assert_eq!(cols9.tier, Tier::Exact);
        assert_eq!(cols9.current.footprint, Some(96));
        assert_eq!(
            cols9.current.with_nulls,
            Some(NullRows {
                t_hoff: 32,
                footprint_min: 32,
                footprint_max: 96
            })
        );
        assert_eq!(cols9.avoidable_bytes_per_row, 0.0);
        assert_eq!(cols9.dominance_search, DominanceScope::Exhaustive);
        let cols8 = &a.tables[1];
        assert_eq!(
            cols8.current.with_nulls.map(|r| (r.t_hoff, r.footprint_max)),
            Some((24, 80))
        );
    }

    #[test]
    fn exact_tier_gates_on_the_row_size_saving_over_null_patterns() {
        // (b1 bool, b2 bool, n int4 NULL, z timetz): every-column rows round to 48 bytes in both
        // orders, but rows with n NULL shrink from 48 to 40 under (z, n, b1, b2).
        let t = one("CREATE TABLE t (b1 boolean NOT NULL, b2 boolean NOT NULL, n integer, z timetz NOT NULL);");
        assert_eq!(t.tier, Tier::Exact);
        assert_eq!(
            t.current.footprint, t.suggested.footprint,
            "no rung crossed without NULLs"
        );
        assert_eq!(t.avoidable_bytes_per_row, 8.0);
        assert_eq!(t.avoidable_deterministic, 0);
        assert_eq!(t.avoidable_dominance, 8);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 8 }));
        assert_eq!(t.suggested_order, vec!["z", "n", "b1", "b2"]);
    }

    #[test]
    fn exact_tier_finds_an_order_that_wins_only_in_row_size() {
        // (c0 timetz NULL, c1 int2 NOT NULL, c2 timetz NULL): (c0, c2, c1) pads 2 more in rows
        // without NULLs, which round to 56 bytes either way, and saves 8 in rows where c0 is NULL
        // (48 vs 40 B measured on PostgreSQL 16). A padding search never finds it; a row-size
        // search recommends it.
        let t = one("CREATE TABLE t (c0 timetz, c1 smallint NOT NULL, c2 timetz);");
        assert_eq!(t.tier, Tier::Exact);
        assert_eq!(t.suggested_order, vec!["c0", "c2", "c1"]);
        assert_eq!(t.dominance_saving, Some(SavingRange { min: 0, max: 8 }));
        assert_eq!(t.avoidable_bytes_per_row, 8.0);
        assert!(t.frontier.is_none());
    }

    #[test]
    fn not_null_restores_deterministic_padding() {
        let sql = |nn: &str| format!("CREATE TABLE t (f boolean{nn}, x bigint NOT NULL, v text NOT NULL);");
        let t = one(&sql(""));
        assert_eq!(t.current.padding, 7, "rows that store every column pad 7 before x");
        assert_eq!(t.current.without_nulls, Some(PaddingBounds { min: 7, max: 7 }));
        assert_eq!(
            t.avoidable_deterministic, 0,
            "a NULL f moves x to offset 0, so the 7 is not certain"
        );
        assert_eq!(t.avoidable_bytes_per_row, 7.0);
        let c = one(&sql(" NOT NULL"));
        assert_eq!(c.current.padding, 7);
        assert_eq!(c.avoidable_deterministic, 7);
        assert_eq!(c.current.without_nulls, None);
    }

    #[test]
    fn a_nullable_text_adds_nothing_and_a_nullable_array_does() {
        // A NULL text advances 0 bytes mod 8, like a 7-byte text, so nullability there changes
        // nothing. A NULL float8[] advances 0 where every stored short array advances 5, so it is
        // a realization of its own and the array is listed (the engine-level witness is in
        // dominance/tests.rs).
        let a = analyze_sources(
            &[src(
                "V1__t.sql",
                "CREATE TABLE a (k int8 NOT NULL, txt text, b int4 NOT NULL);
                 CREATE TABLE b (k int8 NOT NULL, txt text NOT NULL, b int4 NOT NULL);
                 CREATE TABLE c (i int4 NOT NULL, a float8[], n numeric, s int2 NOT NULL);",
            )],
            &Config::default(),
        );
        let [a, b, c] = &a.tables[..] else {
            panic!("three tables")
        };
        assert!(a.null_variables.is_empty());
        assert_eq!(a.current, b.current);
        assert_eq!(a.suggested_order, b.suggested_order);
        assert_eq!(a.avoidable_bytes_per_row, b.avoidable_bytes_per_row);
        assert_eq!(c.null_variables, vec!["a", "n"]);
    }

    #[test]
    fn zero_cost_candidates_spend_no_sweep_budget() {
        // Seven nullable or irregular fixed columns: comparing every member of the order space
        // with NULL bits in flight exhausts the sweep's work budget, but the orders that cost
        // nothing in row size under every NULL pattern are proven without a comparison, which
        // keeps the sweep, and so the verdict, exhaustive.
        for sql in [
            "CREATE TABLE t (c0 integer NOT NULL, c1 timetz NOT NULL, c2 uuid, c3 timetz NOT NULL, \
             c4 smallint NOT NULL, c5 integer, c6 uuid NOT NULL);",
            "CREATE TABLE t (c0 macaddr, c1 integer, c2 uuid NOT NULL, c3 integer, c4 boolean NOT NULL, c5 bigint);",
        ] {
            let t = one(sql);
            assert_eq!(t.tier, Tier::Exact);
            assert_eq!(t.avoidable_bytes_per_row, 8.0, "{sql}");
            assert_eq!(t.dominance_search, DominanceScope::Exhaustive, "{sql}");
        }
    }

    #[test]
    fn wide_nullable_tables_keep_the_finding_past_every_budget() {
        // 24 and 30 nullable regular columns written in a padding order: the pair comparison is
        // out of every budget (more NULL bits in flight than the joint walk holds), but the
        // sorted order pads zero in every NULL pattern, which proves it dominates for free.
        // Measured on PostgreSQL 16 for the 24-column table over 400 NULL patterns: the sorted
        // order is never larger and is smaller in 397.
        for width in [24, 30] {
            let types = ["boolean", "bigint", "smallint", "integer"];
            let cols: Vec<String> = (0..width).map(|i| format!("c{i} {}", types[i % 4])).collect();
            let t = one(&format!("CREATE TABLE r ({});", cols.join(", ")));
            assert_eq!(t.tier, Tier::Exact);
            assert_eq!(t.dominance_search, DominanceScope::Budgeted, "{width}");
            let saving = t.dominance_saving.expect("a proven saving");
            assert!(saving.max > 0, "{width}: {saving:?}");
            assert_eq!(t.avoidable_bytes_per_row, saving.max as f64);
            assert_eq!(t.suggested.with_nulls.map(|r| r.footprint_min), Some(32), "{width}");
        }
    }
}
