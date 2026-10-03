//! The SQL that settles a frontier on real rows. A frontier's winner turns on payload lengths,
//! storage forms and NULLs the DDL does not carry, and every one of them is on the heap page. The
//! query reads each stored row's attributes there (pageinspect), lays the same attributes out in
//! the alternative order the way `heap_fill_tuple` places them (a 1-byte header or a TOAST
//! pointer unaligned, a 4-byte header and every fixed-width value aligned, a NULL nowhere), and
//! counts the rows each order stores in fewer bytes. No column name appears in it: attributes are
//! addressed by number, and the table by a string literal.

use crate::dominance::Measure;
use crate::layout::ColumnKind;
use std::fmt::Write as _;

/// One live column as the query addresses it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueryColumn {
    /// Physical attribute number, 1-based.
    pub attnum: usize,
    /// Storage class: its alignment, and whether its header decides the alignment.
    pub kind: ColumnKind,
}

/// The query that settles one frontier, and how to read its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FrontierQuery {
    /// One read-only SELECT, a single output row.
    pub sql: String,
    /// What each output column means, one line each.
    pub readings: Vec<String>,
}

/// Build the query comparing `current` and `alternative` (orders over `columns`, all of the
/// table's live columns) on the rows of `relation` (name parts, schema first), counted in
/// `measure`.
pub(crate) fn query(
    relation: &[String],
    columns: &[QueryColumn],
    current: &[usize],
    alternative: &[usize],
    measure: Measure,
) -> FrontierQuery {
    let (schema, name) = match relation {
        [.., schema, name] => (Some(schema.as_str()), name.as_str()),
        [name] => (None, name.as_str()),
        [] => (None, ""),
    };
    let schema_filter = match schema {
        Some(schema) => format!("n.nspname = {}", string_literal(schema)),
        None => "pg_catalog.pg_table_is_visible(c.oid)".to_string(),
    };
    let mut steps = String::new();
    for (side, order) in [("c", current), ("a", alternative)] {
        for (step, &index) in order.iter().enumerate() {
            let column = columns[index];
            let (align, varlena) = match column.kind {
                ColumnKind::Fixed { align, .. } => (align.bytes(), false),
                ColumnKind::Varlena { align, .. } => (align.bytes(), true),
            };
            if !steps.is_empty() {
                steps.push_str(", ");
            }
            let _ = write!(steps, "('{side}', {}, {}, {align}, {varlena})", step + 1, column.attnum);
        }
    }
    let n = current.len();
    let cost = |side: &str| match measure {
        Measure::Padding => format!("{side}.o"),
        // The header is the same in both orders and a multiple of 8.
        Measure::RowSize => format!("(({side}.o + 7) / 8 * 8)"),
    };
    let (cur, alt) = (cost("c"), cost("a"));
    let sql = format!(
        "WITH RECURSIVE rel AS (
    SELECT c.oid::regclass AS r
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relname = {relname} AND {schema_filter}
), tuple AS (
    SELECT row_number() OVER () AS id, h.lp_len - h.t_hoff AS stored, h.t_attrs AS a
    FROM rel,
         generate_series(0, pg_catalog.pg_relation_size(rel.r) / current_setting('block_size')::int - 1) AS p,
         heap_page_item_attrs(get_raw_page(rel.r::text, p::int), rel.r) AS h
    WHERE h.lp_flags = 1
), step(side, k, attnum, align, varlena) AS (
    VALUES {steps}
), walk(id, side, k, o) AS (
    SELECT t.id, s.side, 0, 0::bigint FROM tuple t, (VALUES ('c'), ('a')) AS s(side)
    UNION ALL
    SELECT w.id, w.side, w.k + 1,
           CASE WHEN v IS NULL THEN w.o
                WHEN s.varlena AND get_byte(v, 0) & 1 = 1 THEN w.o + length(v)
                ELSE (w.o + s.align - 1) / s.align * s.align + length(v) END
    FROM walk w
    JOIN step s ON s.side = w.side AND s.k = w.k + 1
    JOIN tuple t ON t.id = w.id
    CROSS JOIN LATERAL (SELECT t.a[s.attnum] AS v) x
)
SELECT count(*) AS rows,
       count(*) FILTER (WHERE {alt} < {cur}) AS alternative_smaller,
       count(*) FILTER (WHERE {alt} > {cur}) AS current_smaller,
       coalesce(sum({cur} - {alt}), 0) AS bytes_saved,
       count(*) FILTER (WHERE c.o <> t.stored) AS replay_mismatches
FROM tuple t
JOIN walk c ON c.id = t.id AND c.side = 'c' AND c.k = {n}
JOIN walk a ON a.id = t.id AND a.side = 'a' AND a.k = {n};",
        relname = string_literal(name),
    );
    let unit = match measure {
        Measure::Padding => "padding",
        Measure::RowSize => "row size",
    };
    let readings = vec![
        format!(
            "alternative_smaller and current_smaller count the rows each order stores with less {unit}; \
             bytes_saved totals current minus alternative over every row, so a positive total favors \
             the alternative order"
        ),
        "rows counts the row versions on the table's pages, dead ones included until VACUUM".to_string(),
        "replay_mismatches counts rows whose replayed written order does not match the page; anything \
         but 0 means the replay does not apply (a big-endian server, for one)"
            .to_string(),
        "the query needs the pageinspect extension and superuser, and reads every page; the toaster \
         decides a value's form per row, so a row near the 2 kB threshold might be stored differently \
         in the other order"
            .to_string(),
    ];
    FrontierQuery { sql, readings }
}

/// A string literal PostgreSQL reads back byte for byte: `U&'...'` with `\XXXX` escapes when the
/// text holds a backslash, a character that would break the line, or a `##[` a CI log would read
/// as a command; plain `'...'` otherwise.
pub(crate) fn string_literal(text: &str) -> String {
    let hazard = |c: char| c.is_control() || c == '\u{2028}' || c == '\u{2029}';
    if !text.chars().any(|c| c == '\\' || hazard(c)) && !text.contains("##[") {
        return format!("'{}'", text.replace('\'', "''"));
    }
    let mut out = String::from("U&'");
    for c in text.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '[' if out.ends_with("##") => out.push_str("\\005B"),
            c if hazard(c) => {
                let _ = write!(out, "\\{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests;
