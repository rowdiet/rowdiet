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
}

fn columns() -> Vec<QueryColumn> {
    vec![
        QueryColumn {
            attnum: 1,
            kind: ColumnKind::Fixed {
                len: 6,
                align: Align::Int,
            },
        },
        QueryColumn {
            attnum: 3,
            kind: ColumnKind::Varlena {
                align: Align::Int,
                proven_short: false,
                payload: Payload::ANY,
            },
        },
    ]
}

#[test]
fn the_query_addresses_attributes_by_number_and_the_table_by_literal() {
    let q = query(
        &["sales".to_string(), "Order\"s".to_string()],
        &columns(),
        &[0, 1],
        &[1, 0],
        Measure::Padding,
    );
    assert!(
        q.sql.contains("WHERE c.relname = 'Order\"s' AND n.nspname = 'sales'"),
        "{}",
        q.sql
    );
    assert!(
        q.sql.contains(
            "VALUES ('c', 1, 1, 4, false), ('c', 2, 3, 4, true), ('a', 1, 3, 4, true), ('a', 2, 1, 4, false)"
        ),
        "{}",
        q.sql
    );
    assert!(q.sql.contains("c.k = 2"), "{}", q.sql);
    let unqualified = query(&["t".to_string()], &columns(), &[0, 1], &[1, 0], Measure::RowSize);
    assert!(unqualified.sql.contains("pg_catalog.pg_table_is_visible(c.oid)"));
    assert!(unqualified.sql.contains("((c.o + 7) / 8 * 8)"));
}

#[test]
fn a_hostile_table_name_stays_one_literal_on_one_line() {
    let name = "t\n```\n<img src=x>'; DROP TABLE v; --";
    let q = query(&[name.to_string()], &columns(), &[0, 1], &[1, 0], Measure::Padding);
    assert!(
        q.sql
            .contains("c.relname = U&'t\\000A```\\000A<img src=x>''; DROP TABLE v; --'"),
        "{}",
        q.sql
    );
    assert!(!q.sql.lines().any(|l| l.starts_with("```")));
    assert_eq!(
        q.sql.matches(';').count(),
        3,
        "the statement end and the two inside the literal"
    );
}
