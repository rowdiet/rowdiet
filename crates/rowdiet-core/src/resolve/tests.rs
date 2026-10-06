use super::*;
use crate::layout::{Align, Payload};

#[test]
fn string_literals_read_back_byte_for_byte() {
    assert_eq!(string_literal("orders"), "'orders'");
    assert_eq!(string_literal("o'brien"), "'o''brien'");
    assert_eq!(string_literal("current_user"), "'current_user'");
    assert_eq!(string_literal("a\\b"), "U&'a\\\\b'");
    assert_eq!(string_literal("x\n::error x"), "U&'x\\000A::error x'");
    assert_eq!(string_literal("q'\r"), "U&'q''\\000D'");
    assert_eq!(string_literal("a##[error]b"), "U&'a##\\005Berror]b'");
    assert_eq!(
        string_literal("<b>**x**`"),
        "U&'\\003Cb\\003E\\002A\\002Ax\\002A\\002A\\0060'"
    );
}

#[test]
fn octet_length_reads_only_the_text_and_bytea_families() {
    for spelled in [
        "text",
        "varchar(5)",
        "CHARACTER VARYING(20)",
        "char(3)",
        "bpchar",
        "bytea",
        "pg_catalog.text",
    ] {
        assert!(octet_measurable(spelled), "{spelled}");
    }
    for spelled in [
        "text[]",
        "varchar(5) ARRAY",
        "jsonb",
        "numeric(10,2)",
        "\"char\"",
        "citext",
    ] {
        assert!(!octet_measurable(spelled), "{spelled}");
    }
}

fn columns() -> Vec<QueryColumn> {
    let fixed = ColumnKind::Fixed {
        len: 6,
        align: Align::Int,
    };
    let varlena = ColumnKind::Varlena {
        align: Align::Int,
        proven_short: false,
        payload: Payload::ANY,
    };
    vec![
        QueryColumn::new("x\"; DROP TABLE v; --", fixed, "macaddr"),
        QueryColumn::new("t", varlena, "text"),
    ]
}

#[test]
fn a_hostile_name_stays_one_literal_on_one_line() {
    let name = "t\n```\n<img src=x>'; DROP TABLE v; --";
    let q = query(&[name.to_string()], &columns(), &[0, 1], &[1, 0], Measure::Padding);
    let literal = "U&'t\\000A\\0060\\0060\\0060\\000A\\003Cimg src=x\\003E''; DROP TABLE v; --'";
    for sql in [&q.reader.sql, &q.pageinspect.sql] {
        assert_eq!(sql.matches(literal).count(), 1, "{sql}");
        assert!(!sql.lines().any(|l| l.starts_with("```")), "{sql}");
        assert!(!sql.contains("<img"), "{sql}");
    }
}

#[test]
fn the_template_quote_never_closes_inside_its_body() {
    assert_eq!(dollar_tag("SELECT 1"), "$rowdiet$");
    assert_eq!(dollar_tag("a $rowdiet$ b"), "$rowdiet1$");
}
