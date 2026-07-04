use super::*;

fn one(sql: &str) -> DdlOp {
    let ops = extract(sql).expect(sql);
    assert_eq!(ops.len(), 1, "{sql}: {ops:?}");
    ops.into_iter().next().unwrap()
}

fn create_columns(sql: &str) -> Vec<RawColumn> {
    match one(sql) {
        DdlOp::CreateTable { columns, .. } => columns,
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

fn col_type(coldef: &str) -> TypeRef {
    create_columns(&format!("CREATE TABLE t (c {coldef})"))
        .remove(0)
        .type_ref
}

#[test]
fn keyword_types_arrive_catalog_normalized() {
    assert_eq!(col_type("int").key, "int4");
    assert_eq!(col_type("integer").key, "int4");
    assert_eq!(col_type("bigint").key, "int8");
    assert_eq!(col_type("int8").key, "int8");
    assert_eq!(col_type("smallint").key, "int2");
    assert_eq!(col_type("real").key, "float4");
    assert_eq!(col_type("double precision").key, "float8");
    assert_eq!(col_type("float(10)").key, "float4");
    assert_eq!(col_type("float(30)").key, "float8");
    assert_eq!(col_type("boolean").key, "bool");
    assert_eq!(col_type("numeric(10,2)").key, "numeric");
    assert_eq!(col_type("decimal(12,4)").key, "numeric");
    assert_eq!(col_type("timestamp with time zone").key, "timestamptz");
    assert_eq!(col_type("timestamptz").key, "timestamptz");
    assert_eq!(col_type("timestamp(6) without time zone").key, "timestamp");
    assert_eq!(col_type("time(3) with time zone").key, "timetz");
    assert_eq!(col_type("bit varying(8)").key, "varbit");
    assert_eq!(col_type("bit(4)").key, "bit");
    assert_eq!(col_type("interval").key, "interval");
}

#[test]
fn plain_names_stay_unqualified() {
    assert_eq!(col_type("uuid").key, "uuid");
    assert_eq!(col_type("text").key, "text");
    assert_eq!(col_type("bytea").key, "bytea");
    assert_eq!(col_type("jsonb").key, "jsonb");
    assert_eq!(col_type("serial").key, "serial");
    assert_eq!(col_type("bigserial").key, "bigserial");
    assert_eq!(col_type("money").key, "money");
    assert_eq!(col_type("inet").key, "inet");
    assert_eq!(col_type("tstzrange").key, "tstzrange");
    assert_eq!(col_type("citext").key, "citext");
    assert_eq!(col_type("vector(768)").key, "vector");
    assert_eq!(col_type("my_schema.status_enum").key, "status_enum");
    assert_eq!(col_type("pg_catalog.text").key, "text");
}

#[test]
fn char_semantics() {
    let c10 = col_type("char(10)");
    assert_eq!((c10.key.as_str(), c10.char_len), ("bpchar", Some(10)));
    let bare_char = col_type("char");
    assert_eq!((bare_char.key.as_str(), bare_char.char_len), ("bpchar", Some(1)));
    let bare_bpchar = col_type("bpchar");
    assert_eq!((bare_bpchar.key.as_str(), bare_bpchar.char_len), ("bpchar", None));
    let quoted = col_type("\"char\"");
    assert_eq!(quoted.key, "pgchar");
    let vc = col_type("varchar(255)");
    assert_eq!((vc.key.as_str(), vc.char_len), ("varchar", Some(255)));
    let cv = col_type("character varying(31)");
    assert_eq!((cv.key.as_str(), cv.char_len), ("varchar", Some(31)));
    assert_eq!(col_type("varchar").char_len, None);
}

#[test]
fn arrays_including_the_keyword_form() {
    let a = col_type("int[]");
    assert_eq!((a.key.as_str(), a.dims), ("int4", 1));
    let b = col_type("double precision[][]");
    assert_eq!((b.key.as_str(), b.dims), ("float8", 2));
    let kw = col_type("integer ARRAY");
    assert_eq!((kw.key.as_str(), kw.dims), ("int4", 1));
    let kwb = col_type("text ARRAY[4]");
    assert_eq!((kwb.key.as_str(), kwb.dims), ("text", 1));
}

#[test]
fn not_null_detection() {
    let cols = create_columns(
        "CREATE TABLE t (a int NOT NULL, b int, c uuid PRIMARY KEY, d int GENERATED ALWAYS AS IDENTITY)",
    );
    let nn: Vec<bool> = cols.iter().map(|c| c.not_null).collect();
    assert_eq!(nn, vec![true, false, true, true]);
}

#[test]
fn table_level_primary_key() {
    match one("CREATE TABLE t3 (a int, b int, PRIMARY KEY (a, b))") {
        DdlOp::CreateTable { pk_columns, .. } => assert_eq!(pk_columns, vec!["a", "b"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn identifier_folding() {
    match one("CREATE TABLE \"my schema\".\"My Table\" (\"select\" int, UnQuoted int)") {
        DdlOp::CreateTable { name, columns, .. } => {
            assert_eq!(name.key, "My Table");
            assert_eq!(columns[0].key, "select");
            assert_eq!(columns[1].key, "unquoted");
        }
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
    match one("ALTER TABLE t ALTER COLUMN c TYPE bigint") {
        DdlOp::SetColumnType { type_ref, .. } => assert_eq!(type_ref.key, "int8"),
        other => panic!("{other:?}"),
    }
    match one("ALTER TABLE t ALTER COLUMN c SET NOT NULL") {
        DdlOp::SetNotNull { value, .. } => assert!(value),
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
            assert_eq!((base.key.as_str(), base.char_len), ("varchar", Some(10)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn drops() {
    match one("DROP TABLE IF EXISTS a, s.b CASCADE") {
        DdlOp::DropTables { names, if_exists } => {
            assert!(if_exists);
            assert_eq!(names.iter().map(|n| n.key.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        }
        other => panic!("{other:?}"),
    }
    match one("DROP TYPE status") {
        DdlOp::DropTypes { names } => assert_eq!(names[0].key, "status"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn sqlparser_gaps_parse_natively_here() {
    assert_eq!(extract("DO $$ BEGIN NULL; END $$").unwrap(), vec![DdlOp::Irrelevant]);
    match one("CREATE UNLOGGED TABLE ul (a int)") {
        DdlOp::CreateTable { name, columns, .. } => {
            assert_eq!(name.key, "ul");
            assert_eq!(columns.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    match one("CREATE TABLE t2 (LIKE t1 INCLUDING ALL)") {
        DdlOp::CreateTable {
            incomplete_columns,
            columns,
            ..
        } => {
            assert!(incomplete_columns);
            assert!(columns.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn ctas_and_partition_children() {
    match one("CREATE TABLE snap AS SELECT 1 AS one") {
        DdlOp::CreateTable { is_ctas, .. } => assert!(is_ctas),
        other => panic!("{other:?}"),
    }
    match one("CREATE TABLE events_2026 PARTITION OF events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')") {
        DdlOp::CreateTable {
            incomplete_columns,
            partition_of,
            ..
        } => {
            assert!(!incomplete_columns);
            assert_eq!(partition_of.unwrap().key, "events");
        }
        other => panic!("{other:?}"),
    }
    match one("CREATE TABLE kid () INHERITS (folks)") {
        DdlOp::CreateTable {
            incomplete_columns,
            partition_of,
            ..
        } => {
            assert!(incomplete_columns);
            assert!(partition_of.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_error_is_loud_and_recoverable() {
    assert!(extract("CREATE TABLE t (a int").is_err());
    assert!(extract("SELECT 1").is_ok());
}
