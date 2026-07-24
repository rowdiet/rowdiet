use super::*;

fn one(sql: &str) -> DdlOp {
    let ops = extract(sql).expect(sql);
    assert_eq!(ops.len(), 1, "{sql}");
    ops.into_iter().next().unwrap()
}

fn create_columns(sql: &str) -> Vec<RawColumn> {
    match one(sql) {
        DdlOp::CreateTable { columns, .. } => columns,
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

fn col_type(coldef: &str) -> TypeRef {
    let cols = create_columns(&format!("CREATE TABLE t (c {coldef})"));
    cols.into_iter().next().unwrap().type_ref
}

#[test]
fn type_mapping_spot_checks() {
    assert_eq!(col_type("bigint").key, "int8");
    assert_eq!(col_type("int8").key, "int8");
    assert_eq!(col_type("smallint").key, "int2");
    assert_eq!(col_type("double precision").key, "float8");
    assert_eq!(col_type("real").key, "float4");
    assert_eq!(col_type("boolean").key, "bool");
    assert_eq!(col_type("timestamptz").key, "timestamptz");
    assert_eq!(col_type("timestamp with time zone").key, "timestamptz");
    assert_eq!(col_type("timestamp(6) without time zone").key, "timestamp");
    assert_eq!(col_type("time(3) with time zone").key, "timetz");
    assert_eq!(col_type("numeric(10,2)").key, "numeric");
    assert_eq!(col_type("decimal(12,4)").key, "numeric");
    assert_eq!(col_type("bit varying(8)").key, "varbit");
    assert_eq!(col_type("bit(4)").key, "bit");
    assert_eq!(col_type("uuid").key, "uuid");
    assert_eq!(col_type("bytea").key, "bytea");
    assert_eq!(col_type("jsonb").key, "jsonb");
    assert_eq!(col_type("interval").key, "interval");
    assert_eq!(col_type("point").key, "point");
    assert_eq!(col_type("tsvector").key, "tsvector");
}

#[test]
fn char_types_and_typmods() {
    let vc = col_type("varchar(255)");
    assert_eq!((vc.key.as_str(), vc.char_len), ("varchar", Some(255)));
    let cv = col_type("character varying(31)");
    assert_eq!((cv.key.as_str(), cv.char_len), ("varchar", Some(31)));
    let bare = col_type("varchar");
    assert_eq!((bare.key.as_str(), bare.char_len), ("varchar", None));
    let c10 = col_type("char(10)");
    assert_eq!((c10.key.as_str(), c10.char_len), ("bpchar", Some(10)));
    let c = col_type("char");
    assert_eq!((c.key.as_str(), c.char_len), ("bpchar", Some(1)));
    let quoted = col_type("\"char\"");
    assert_eq!(quoted.key, "pgchar");
}

#[test]
fn arrays() {
    let a = col_type("int[]");
    assert_eq!((a.key.as_str(), a.dims), ("int4", 1));
    let b = col_type("bigint[]");
    assert_eq!((b.key.as_str(), b.dims), ("int8", 1));
    let c = col_type("double precision[][]");
    assert_eq!((c.key.as_str(), c.dims), ("float8", 2));
    let d = col_type("numeric(10,2)[]");
    assert_eq!((d.key.as_str(), d.dims), ("numeric", 1));
}

#[test]
fn custom_and_qualified_types() {
    assert_eq!(col_type("my_schema.status_enum").key, "status_enum");
    assert_eq!(col_type("public.us_postal_code").key, "us_postal_code");
    assert_eq!(col_type("pg_catalog.text").key, "text");
    assert_eq!(col_type("citext").key, "citext");
    assert_eq!(col_type("vector(768)").key, "vector");
    assert_eq!(col_type("\"MixedCaseType\"").key, "MixedCaseType");
    assert_eq!(col_type("tstzrange").key, "tstzrange");
    assert_eq!(col_type("serial").key, "serial");
    assert_eq!(col_type("money").key, "money");
    assert_eq!(col_type("inet").key, "inet");
    assert_eq!(col_type("oid").key, "oid");
    assert_eq!(col_type("xml").key, "xml");
}

#[test]
fn not_null_detection() {
    let cols = create_columns(
        "CREATE TABLE t (a int NOT NULL, b int, c uuid PRIMARY KEY, d int GENERATED ALWAYS AS IDENTITY, e int CONSTRAINT nn CHECK (e > 0))",
    );
    let nn: Vec<bool> = cols.iter().map(|c| c.not_null).collect();
    assert_eq!(nn, vec![true, false, true, true, false]);
}

#[test]
fn table_level_primary_key() {
    match one("CREATE TABLE t3 (a int, b int, PRIMARY KEY (a, b))") {
        DdlOp::CreateTable { pk_columns, .. } => assert_eq!(pk_columns, vec!["a", "b"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn quoted_and_qualified_table_names() {
    match one("CREATE TABLE \"my schema\".\"My Table\" (\"select\" int)") {
        DdlOp::CreateTable { name, columns, .. } => {
            assert_eq!(name.key, "my schema.My Table");
            assert_eq!(columns[0].key, "select");
        }
        other => panic!("{other:?}"),
    }
    match one("CREATE TABLE Foo.BAR (a int)") {
        DdlOp::CreateTable { name, .. } => assert_eq!(name.key, "foo.bar"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn alter_forms() {
    match one("ALTER TABLE t ADD COLUMN IF NOT EXISTS z timestamptz NOT NULL") {
        DdlOp::AddColumn {
            column, if_not_exists, ..
        } => {
            assert!(if_not_exists);
            assert!(column.not_null);
            assert_eq!(column.type_ref.key, "timestamptz");
        }
        other => panic!("{other:?}"),
    }
    match one("ALTER TABLE t ADD z int") {
        DdlOp::AddColumn { column, .. } => assert_eq!(column.key, "z"),
        other => panic!("{other:?}"),
    }
    let drops = extract("ALTER TABLE t DROP COLUMN a, DROP COLUMN b").unwrap();
    let dropped: Vec<String> = drops
        .into_iter()
        .flat_map(|op| match op {
            DdlOp::DropColumns { columns, .. } => columns,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(dropped, vec!["a", "b"]);
    match one("ALTER TABLE t RENAME COLUMN a TO b") {
        DdlOp::RenameColumn { old, new, .. } => assert_eq!((old.as_str(), new.as_str()), ("a", "b")),
        other => panic!("{other:?}"),
    }
    match one("ALTER TABLE t RENAME TO u") {
        DdlOp::RenameTable { new, .. } => assert_eq!(new.key, "u"),
        other => panic!("{other:?}"),
    }
    match one("ALTER TABLE t ALTER COLUMN c SET NOT NULL") {
        DdlOp::SetNotNull { value, .. } => assert!(value),
        other => panic!("{other:?}"),
    }
    match one("ALTER TABLE t ALTER COLUMN c TYPE bigint") {
        DdlOp::SetColumnType { type_ref, .. } => assert_eq!(type_ref.key, "int8"),
        other => panic!("{other:?}"),
    }
    match one("ALTER TABLE t ADD PRIMARY KEY (a)") {
        DdlOp::SetNotNull { column, value, .. } => {
            assert_eq!(column, "a");
            assert!(value);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn multi_op_alter() {
    let ops = extract("ALTER TABLE t ADD COLUMN a int, ADD COLUMN b text").unwrap();
    assert_eq!(ops.len(), 2);
}

#[test]
fn create_type_forms() {
    match one("CREATE TYPE status AS ENUM ('a', 'b')") {
        DdlOp::CreateEnum { name } => assert_eq!(name.key, "status"),
        other => panic!("{other:?}"),
    }
    match one("CREATE TYPE pair AS (x int, y int)") {
        DdlOp::CreateComposite { name } => assert_eq!(name.key, "pair"),
        other => panic!("{other:?}"),
    }
    match one("CREATE TYPE bigrange AS RANGE (SUBTYPE = int8)") {
        DdlOp::CreateRange { name, subtype } => {
            assert_eq!(name.key, "bigrange");
            assert_eq!(subtype.unwrap().key, "int8");
        }
        other => panic!("{other:?}"),
    }
    match one("CREATE DOMAIN us_postal_code AS varchar(10)") {
        DdlOp::CreateDomain { name, base } => {
            assert_eq!(name.key, "us_postal_code");
            assert_eq!(base.key, "varchar");
            assert_eq!(base.char_len, Some(10));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn drop_table() {
    match one("DROP TABLE IF EXISTS a, b CASCADE") {
        DdlOp::DropTables { names, if_exists } => {
            assert!(if_exists);
            assert_eq!(names.iter().map(|n| n.key.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn known_parse_gaps_error_loudly() {
    assert!(extract("DO $$ BEGIN NULL; END $$").is_err());
    assert!(extract("CREATE TABLE t (c integer ARRAY)").is_err());
    assert!(extract("CREATE TABLE t2 (LIKE t1 INCLUDING ALL)").is_err());
}

#[test]
fn unlogged_preprocess() {
    let stripped = preprocess("CREATE UNLOGGED TABLE ul (a int)");
    assert_eq!(stripped, "CREATE TABLE ul (a int)");
    assert!(extract(&stripped).is_ok());
    assert_eq!(preprocess("CREATE TABLE t (a int)"), "CREATE TABLE t (a int)");
}

#[test]
fn irrelevant_statements_are_silent() {
    assert_eq!(extract("CREATE INDEX i ON t (a)").unwrap(), vec![DdlOp::Irrelevant]);
    assert_eq!(extract("INSERT INTO t VALUES (1)").unwrap(), vec![DdlOp::Irrelevant]);
}

#[test]
fn sniff_targets() {
    assert_eq!(
        sniff("ALTER TABLE only_tab ADD nope"),
        Some(Sniff::AlterTable("only_tab".into()))
    );
    assert_eq!(
        sniff("alter table ONLY my_schema.Big_Tab whatever"),
        Some(Sniff::AlterTable("my_schema.big_tab".into()))
    );
    assert_eq!(
        sniff("ALTER TABLE IF EXISTS x DROP y"),
        Some(Sniff::AlterTable("x".into()))
    );
    assert_eq!(
        sniff("CREATE UNLOGGED TABLE ul (x int)"),
        Some(Sniff::CreateTable("ul".into()))
    );
    assert_eq!(
        sniff("CREATE TABLE IF NOT EXISTS s.t (x int)"),
        Some(Sniff::CreateTable("s.t".into()))
    );
    assert_eq!(sniff("DO $$ x $$"), None);
    assert_eq!(sniff("CREATE INDEX i ON t (a)"), None);
}
