//! The SQL that settles a frontier on real rows. A frontier's winner turns on payload lengths,
//! storage forms and NULLs the DDL does not carry. Two queries read them from the table and lay
//! every row out in both orders the way `heap_fill_tuple` places values (a 1-byte header or a
//! TOAST pointer unaligned, a 4-byte header and every fixed-width value aligned, a NULL
//! nowhere). The reader query runs as any role that can read the table and takes each value's
//! form from its size functions; the pageinspect query replays the stored bytes exactly and needs
//! a superuser. Both walk each row once, look the table and its columns up by name at run time,
//! and answer "cannot settle" with a reason when what they would read is not there.

use crate::dominance::Measure;
use crate::layout::ColumnKind;
use std::fmt::Write as _;

/// One live column as the queries address it.
#[derive(Debug, Clone)]
pub(crate) struct QueryColumn {
    /// The name PostgreSQL stores: case-folded unless the DDL quoted it.
    pub name: String,
    /// Storage class: its alignment, and whether its header decides the alignment.
    pub kind: ColumnKind,
    /// `octet_length` reads the payload length from the header (text, varchar, char, bytea).
    pub octets: bool,
}

impl QueryColumn {
    pub(crate) fn new(name: &str, kind: ColumnKind, type_display: &str) -> Self {
        let octets = matches!(kind, ColumnKind::Varlena { .. }) && octet_measurable(type_display);
        Self {
            name: name.to_string(),
            kind,
            octets,
        }
    }
}

/// Built-in types whose `octet_length` takes the length from the header without detoasting.
fn octet_measurable(type_display: &str) -> bool {
    let spelled = type_display.trim().to_ascii_lowercase();
    if spelled.contains('[') || spelled.contains(" array") {
        return false;
    }
    let unqualified = spelled.strip_prefix("pg_catalog.").unwrap_or(&spelled);
    let base = unqualified.split('(').next().unwrap_or_default().trim();
    matches!(
        base,
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "bytea"
    )
}

/// The queries that settle one frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FrontierQuery {
    /// Runs as any role that can read the table; approximate where a value's form is a guess.
    pub reader: SettlingQuery,
    /// Replays the stored tuples exactly; needs the pageinspect extension and a superuser.
    pub pageinspect: SettlingQuery,
}

/// One read-only SELECT with a single output row, and how to read its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct SettlingQuery {
    /// The statement.
    pub sql: String,
    /// What each output column means, one line each.
    pub readings: Vec<String>,
}

/// Build both queries comparing `current` and `alternative` (orders over `columns`, all of the
/// table's live columns) on the rows of `relation` (name parts, schema first), in `measure`.
pub(crate) fn query(
    relation: &[String],
    columns: &[QueryColumn],
    current: &[usize],
    alternative: &[usize],
    measure: Measure,
) -> FrontierQuery {
    FrontierQuery {
        reader: reader_query(relation, columns, current, alternative, measure),
        pageinspect: pageinspect_query(relation, columns, current, alternative, measure),
    }
}

fn unit(measure: Measure) -> &'static str {
    match measure {
        Measure::Padding => "padding",
        Measure::RowSize => "row size",
    }
}

fn counts_reading(measure: Measure) -> String {
    format!(
        "alternative_smaller and current_smaller count the rows each order stores with less {}; \
         bytes_saved totals current minus alternative over those rows, so a positive total favors \
         the alternative order",
        unit(measure)
    )
}

/// The catalog lookups both queries share: the relation by name (schema-qualified when the DDL
/// qualified it), the columns the walk needs, partitions, inheritance children, fast defaults.
fn catalog(relation: &[String], columns: &[QueryColumn], storage: bool) -> String {
    let lookup = match relation {
        [.., schema, name] => format!(
            "pg_catalog.format('%I.%I', {}, {})",
            string_literal(schema),
            string_literal(name)
        ),
        [name] => format!("pg_catalog.format('%I', {})", string_literal(name)),
        [] => "''".to_string(),
    };
    let names: Vec<String> = columns.iter().map(|c| string_literal(&c.name)).collect();
    let plain = if storage {
        "
           ARRAY(SELECT coalesce(a.attstorage = 'p', false)::text
                 FROM unnest(t.names) WITH ORDINALITY AS w(name, k)
                 LEFT JOIN pg_catalog.pg_attribute AS a
                        ON a.attrelid = t.r AND a.attname::text = w.name AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY w.k) AS plain,"
    } else {
        ""
    };
    format!(
        "WITH target AS (
    SELECT n.name, pg_catalog.to_regclass(n.name) AS r, ARRAY[{names}]::text[] AS names
    FROM (SELECT {lookup} AS name) AS n
), info AS (
    SELECT t.*, c.relkind,{plain}
           (SELECT count(*) FROM pg_catalog.pg_inherits AS i WHERE i.inhparent = t.r) AS children,
           (SELECT count(*) FROM pg_catalog.pg_partition_tree(t.r) AS p
            JOIN pg_catalog.pg_class AS l ON l.oid = p.relid
            WHERE c.relkind = 'p' AND p.isleaf AND l.relkind <> 'r') AS foreign_leaves,
           ARRAY(SELECT w.name FROM unnest(t.names) WITH ORDINALITY AS w(name, k)
                 WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_attribute AS a
                                   WHERE a.attrelid = t.r AND a.attname::text = w.name
                                     AND a.attnum > 0 AND NOT a.attisdropped)
                 ORDER BY w.k) AS missing,
           ARRAY(SELECT a.attname::text FROM pg_catalog.pg_attribute AS a
                 WHERE a.attrelid = t.r AND a.atthasmissing AND NOT a.attisdropped
                   AND a.attname::text = ANY (t.names)
                 ORDER BY a.attnum) AS fast_default
    FROM target AS t LEFT JOIN pg_catalog.pg_class AS c ON c.oid = t.r
)",
        names = names.join(", ")
    )
}

/// The reasons both queries give for not settling, in a `concat_ws` list over `a`.
const CANNOT_SETTLE: &str = "CASE WHEN a.r IS NULL THEN 'cannot settle: no relation ' || a.name || ' here' END,
        CASE WHEN a.relkind NOT IN ('r', 'p') THEN 'cannot settle: ' || a.name || ' is not a table' END,
        CASE WHEN a.r IS NOT NULL AND cardinality(a.missing) > 0 THEN 'cannot settle: column(s) ' || pg_catalog.array_to_string(a.missing, ', ')
             || ' not in the table yet; apply the migration that adds them and load rows first' END,
        CASE WHEN a.n = 0 AND a.relkind IN ('r', 'p') AND cardinality(a.missing) = 0
             THEN 'cannot settle: no rows to compare' END";

/// The walk's end offset for `order`: each step aligns to `e{i}` and adds `z{i}` from `v`.
fn walk(order: &[usize]) -> String {
    let mut offset = "0".to_string();
    for &index in order {
        let i = index + 1;
        offset = format!("(({offset} + v.e{i} - 1) / v.e{i} * v.e{i} + v.z{i})");
    }
    offset
}

/// What a row's walk is compared in: the data length, or the row's footprint on the page.
fn cost(offset: &str, measure: Measure) -> String {
    match measure {
        Measure::Padding => offset.to_string(),
        // The header is the same in both orders and a multiple of 8.
        Measure::RowSize => format!("(({offset} + 7) / 8 * 8)"),
    }
}

fn align_and_width(kind: ColumnKind) -> (u64, Option<u64>) {
    match kind {
        ColumnKind::Fixed { len, align } => (align.bytes(), Some(len)),
        ColumnKind::Varlena { align, .. } => (align.bytes(), None),
    }
}

/// One `WHEN` of a value's form: the alignment it takes, the bytes it stores, and whether the
/// form is a guess. `when: None` is the `ELSE`.
struct Form {
    when: Option<String>,
    align: String,
    bytes: String,
    guess: bool,
}

fn form(when: Option<String>, align: impl ToString, bytes: impl ToString, guess: bool) -> Form {
    Form {
        when,
        align: align.to_string(),
        bytes: bytes.to_string(),
        guess,
    }
}

fn case(forms: &[Form], pick: impl Fn(&Form) -> String) -> String {
    let mut out = String::from("CASE");
    for f in forms {
        match &f.when {
            Some(when) => {
                let _ = write!(out, " WHEN {when} THEN {}", pick(f));
            }
            None => {
                let _ = write!(out, " ELSE {}", pick(f));
            }
        }
    }
    out.push_str(" END");
    out
}

/// A value's form as a rewrite in either order stores it, from what its size functions say.
/// `plain` is the format placeholder for the column's `attstorage = 'p'`.
fn reader_forms(i: usize, column: &QueryColumn, plain: &str) -> Vec<Form> {
    let (align, width) = align_and_width(column.kind);
    if let Some(width) = width {
        return vec![
            form(Some(format!("m.n{i}")), 1, 0, false),
            form(None, align, width, false),
        ];
    }
    let (s, c, l) = (format!("m.s{i}"), format!("m.c{i}"), format!("m.l{i}"));
    let mut forms = vec![
        form(Some(format!("{s} IS NULL")), 1, 0, false),
        form(Some(format!("{c} AND NOT t.big")), align, &s, false),
        form(Some(c), 1, 18, true),
    ];
    if column.octets {
        forms.extend([
            form(Some(format!("{s} = {l}")), 1, 18, false),
            form(
                Some(format!("{s} - {l} = 1 OR ({l} <= 126 AND NOT {plain})")),
                1,
                format!("{l} + 1"),
                false,
            ),
            form(None, align, format!("{l} + 4"), false),
        ]);
    } else {
        forms.extend([
            form(Some(format!("{s} <= 127 AND NOT {plain}")), 1, &s, false),
            form(Some(format!("{s} <= 127")), align, &s, true),
            form(Some("NOT t.big".to_string()), align, &s, false),
            form(None, 1, 18, true),
        ]);
    }
    forms
}

/// The query any role that can read the table runs. Its per-row SQL is built by `format` over
/// the names the catalog confirmed, and runs through `query_to_xml` only when they all exist.
fn reader_query(
    relation: &[String],
    columns: &[QueryColumn],
    current: &[usize],
    alternative: &[usize],
    measure: Measure,
) -> SettlingQuery {
    let k = columns.len();
    // Placeholders: %1$s the relation, %2$s ONLY or nothing, then each name, then each PLAIN flag.
    let name = |i: usize| format!("r.%{}$I", i + 2);
    let plain = |i: usize| format!("%{}$s", k + i + 2);
    let mut sizes = Vec::new();
    let mut big = vec!["24".to_string()];
    let mut picks = Vec::new();
    let mut guesses = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let i = index + 1;
        let (_, width) = align_and_width(column.kind);
        match width {
            Some(width) => {
                sizes.push(format!("{} IS NULL AS n{i}", name(i)));
                big.push(format!("CASE WHEN m.n{i} THEN 0 ELSE {width} END"));
            }
            None => {
                sizes.push(format!(
                    "pg_catalog.pg_column_size({0}) AS s{i}, pg_catalog.pg_column_compression({0}) IS NOT NULL AS c{i}",
                    name(i)
                ));
                if column.octets {
                    sizes.push(format!("pg_catalog.octet_length({}) AS l{i}", name(i)));
                    big.push(format!(
                        "coalesce(CASE WHEN NOT m.c{i} AND m.s{i} = m.l{i} THEN 18 ELSE m.s{i} END, 0)"
                    ));
                } else {
                    big.push(format!("coalesce(m.s{i}, 0)"));
                }
            }
        }
        let forms = reader_forms(i, column, &plain(i));
        picks.push(format!("{} AS e{i}", case(&forms, |f| f.align.clone())));
        picks.push(format!("{} AS z{i}", case(&forms, |f| f.bytes.clone())));
        if forms.iter().any(|f| f.guess) {
            guesses.push(case(&forms, |f| f.guess.to_string()));
        }
    }
    let guessed = if guesses.is_empty() {
        "false".to_string()
    } else {
        guesses.join("\n               OR ")
    };
    let template = format!(
        "SELECT count(*) AS n,
       count(*) FILTER (WHERE w.alt < w.cur) AS alternative_smaller,
       count(*) FILTER (WHERE w.alt > w.cur) AS current_smaller,
       coalesce(sum(w.cur - w.alt), 0) AS bytes_saved,
       count(*) FILTER (WHERE v.guessed) AS approximate_rows
FROM %2$s %1$s AS r
CROSS JOIN LATERAL (
    SELECT {sizes}
    OFFSET 0
) AS m
CROSS JOIN LATERAL (
    SELECT {big} > (current_setting('block_size')::int - 40) / 32 * 8 AS big
    OFFSET 0
) AS t
CROSS JOIN LATERAL (
    SELECT {picks},
           {guessed} AS guessed
    OFFSET 0
) AS v
CROSS JOIN LATERAL (
    SELECT {cur} AS cur,
           {alt} AS alt
    OFFSET 0
) AS w",
        sizes = sizes.join(",\n           "),
        big = big.join(" + "),
        picks = picks.join(",\n           "),
        cur = cost(&walk(current), measure),
        alt = cost(&walk(alternative), measure),
    );
    let tag = dollar_tag(&template);
    let field = |f: &str| format!("(pg_catalog.xpath('/table/row/{f}/text()', s.x))[1]::text::bigint AS {f}");
    let sql = format!(
        "{catalog}, settled AS (
    SELECT i.*,
           (SELECT pg_catalog.query_to_xml(pg_catalog.format({tag}
{template}
{tag}, VARIADIC ARRAY[i.r::text, CASE WHEN i.relkind = 'p' THEN '' ELSE 'ONLY' END] || i.names || i.plain),
                   false, false, '')
            WHERE i.relkind IN ('r', 'p') AND cardinality(i.missing) = 0) AS x
    FROM info AS i
), answer AS (
    SELECT s.*,
           {n},
           {alternative},
           {current},
           {saved},
           {approximate}
    FROM settled AS s
)
SELECT CASE WHEN a.n > 0 THEN a.n END AS rows,
       CASE WHEN a.n > 0 THEN a.alternative_smaller END AS alternative_smaller,
       CASE WHEN a.n > 0 THEN a.current_smaller END AS current_smaller,
       CASE WHEN a.n > 0 THEN a.bytes_saved END AS bytes_saved,
       CASE WHEN a.n > 0 THEN a.approximate_rows END AS approximate_rows,
       pg_catalog.concat_ws('; ',
        {CANNOT_SETTLE},
        CASE WHEN a.relkind = 'p' THEN 'partitioned: every partition read' END,
        CASE WHEN a.foreign_leaves > 0 THEN a.foreign_leaves || ' foreign partition(s) read through their server, sizes approximate' END,
        CASE WHEN a.relkind = 'r' AND a.children > 0 THEN 'inheritance parent: its own rows only, ' || a.children || ' child table(s) not read' END,
        CASE WHEN cardinality(a.fast_default) > 0 THEN 'column(s) ' || pg_catalog.array_to_string(a.fast_default, ', ')
             || ' carry a default older rows do not store; both orders count it as a rewrite stores it' END) AS note
FROM answer AS a;",
        catalog = catalog(relation, columns, true),
        n = field("n"),
        alternative = field("alternative_smaller"),
        current = field("current_smaller"),
        saved = field("bytes_saved"),
        approximate = field("approximate_rows"),
    );
    let readings = vec![
        counts_reading(measure),
        "runs as any role that can read the table on PostgreSQL 14 or later and reads no TOAST \
         data: each value's form comes from pg_column_size, pg_column_compression and, for text \
         types, octet_length, as a rewrite in either order would store it"
            .to_string(),
        "approximate_rows counts rows holding a value whose form is a guess: a compressed or long \
         non-text value in a row over the TOAST threshold, or a short non-text value in a PLAIN \
         column; the pageinspect query (--settle-exact) settles those"
            .to_string(),
        "note gives the reason when the query cannot settle (no such table, columns not added yet, \
         no rows) and says what it read: partitions, inheritance children, fast defaults"
            .to_string(),
    ];
    SettlingQuery { sql, readings }
}

/// A dollar-quote tag that does not occur in `body`.
fn dollar_tag(body: &str) -> String {
    let mut tag = "$rowdiet$".to_string();
    let mut n = 0;
    while body.contains(&tag) {
        n += 1;
        tag = format!("$rowdiet{n}$");
    }
    tag
}

/// The exact query: reads each stored row version from the page and replays its bytes.
fn pageinspect_query(
    relation: &[String],
    columns: &[QueryColumn],
    current: &[usize],
    alternative: &[usize],
    measure: Measure,
) -> SettlingQuery {
    let mut values = Vec::new();
    let mut picks = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let i = index + 1;
        values.push(format!("h.t_attrs[l.n[{i}]] AS v{i}"));
        let (align, width) = align_and_width(column.kind);
        let unaligned = match width {
            Some(_) => format!("x.v{i} IS NULL"),
            None => format!("x.v{i} IS NULL OR get_byte(x.v{i}, 0) & 1 = 1"),
        };
        picks.push(format!(
            "CASE WHEN {unaligned} THEN 1 ELSE {align} END AS e{i}, coalesce(length(x.v{i}), 0) AS z{i}"
        ));
    }
    let sql = format!(
        "{catalog}, leaf AS (
    SELECT c.oid::regclass AS r,
           ARRAY(SELECT a.attnum FROM unnest(i.names) WITH ORDINALITY AS w(name, k)
                 JOIN pg_catalog.pg_attribute AS a
                   ON a.attrelid = c.oid AND a.attname::text = w.name AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY w.k)::int[] AS n,
           ARRAY(SELECT a.attnum FROM pg_catalog.pg_attribute AS a
                 WHERE a.attrelid = c.oid AND a.attisdropped)::int[] AS dropped,
           coalesce((SELECT max(a.attnum) FROM pg_catalog.pg_attribute AS a
                     WHERE a.attrelid = c.oid AND a.atthasmissing AND NOT a.attisdropped
                       AND a.attname::text = ANY (i.names)), 0) AS need
    FROM info AS i
    CROSS JOIN LATERAL (SELECT i.r AS relid WHERE i.relkind = 'r'
                        UNION ALL
                        SELECT p.relid FROM pg_catalog.pg_partition_tree(i.r) AS p
                        WHERE i.relkind = 'p' AND p.isleaf) AS l
    JOIN pg_catalog.pg_class AS c ON c.oid = l.relid AND c.relkind = 'r'
    WHERE cardinality(i.missing) = 0
), ordered AS MATERIALIZED (
    SELECT l.*, l.n = ARRAY(SELECT u FROM unnest(l.n) AS u ORDER BY u) AS written_order
    FROM leaf AS l
), counted AS (
    SELECT count(*) FILTER (WHERE x.whole) AS n,
           count(*) FILTER (WHERE x.whole AND w.alt < w.cur) AS alternative_smaller,
           count(*) FILTER (WHERE x.whole AND w.alt > w.cur) AS current_smaller,
           coalesce(sum(w.cur - w.alt) FILTER (WHERE x.whole), 0) AS bytes_saved,
           count(*) FILTER (WHERE x.whole AND x.checkable AND c.o <> x.stored) AS replay_mismatches,
           count(*) FILTER (WHERE NOT x.whole) AS skipped_rows
    FROM ordered AS l
    CROSS JOIN LATERAL generate_series(0, pg_catalog.pg_relation_size(l.r) / current_setting('block_size')::int - 1) AS p
    CROSS JOIN LATERAL heap_page_item_attrs(get_raw_page(l.r::text, p::int), l.r) AS h
    CROSS JOIN LATERAL (
        SELECT h.t_infomask2 & 2047 >= l.need AS whole, h.lp_len - h.t_hoff AS stored,
               l.written_order AND NOT EXISTS (SELECT FROM unnest(l.dropped) AS d WHERE h.t_attrs[d] IS NOT NULL) AS checkable,
               {values}
        OFFSET 0
    ) AS x
    CROSS JOIN LATERAL (
        SELECT {picks}
        OFFSET 0
    ) AS v
    CROSS JOIN LATERAL (
        SELECT {cur_walk} AS o
        OFFSET 0
    ) AS c
    CROSS JOIN LATERAL (
        SELECT {cur} AS cur,
               {alt} AS alt
        OFFSET 0
    ) AS w
    WHERE h.lp_flags = 1
), answer AS (
    SELECT i.*, k.* FROM info AS i, counted AS k
)
SELECT CASE WHEN a.n > 0 THEN a.n END AS rows,
       CASE WHEN a.n > 0 THEN a.alternative_smaller END AS alternative_smaller,
       CASE WHEN a.n > 0 THEN a.current_smaller END AS current_smaller,
       CASE WHEN a.n > 0 THEN a.bytes_saved END AS bytes_saved,
       CASE WHEN a.n > 0 THEN a.replay_mismatches END AS replay_mismatches,
       a.skipped_rows,
       pg_catalog.concat_ws('; ',
        {CANNOT_SETTLE},
        CASE WHEN a.relkind = 'p' THEN 'partitioned: every leaf partition read' END,
        CASE WHEN a.foreign_leaves > 0 THEN a.foreign_leaves || ' foreign partition(s) not read' END,
        CASE WHEN a.relkind = 'r' AND a.children > 0 THEN 'inheritance parent: its own rows only, ' || a.children || ' child table(s) not read' END,
        CASE WHEN a.skipped_rows > 0 THEN 'skipped_rows predate column(s) ' || pg_catalog.array_to_string(a.fast_default, ', ')
             || ' and store no value for them' END) AS note
FROM answer AS a;",
        catalog = catalog(relation, columns, false),
        values = values.join(",\n               "),
        picks = picks.join(",\n               "),
        cur_walk = walk(current),
        cur = cost("c.o", measure),
        alt = cost(&walk(alternative), measure),
    );
    let readings = vec![
        counts_reading(measure),
        "rows counts the row versions on the table's pages, dead ones included until pruning or \
         VACUUM removes them; \
         skipped_rows counts versions written before a column the walk reads was added with a \
         default, which store no value for it"
            .to_string(),
        "replay_mismatches counts rows whose replayed written order does not match the page; \
         anything but 0 means the replay does not apply (a big-endian server, for one)"
            .to_string(),
        "needs the pageinspect extension and a superuser, and reads every page (of every leaf \
         partition of a partitioned table); the toaster decides a value's form per row, so a row \
         near the 2 kB threshold might be stored differently in the other order"
            .to_string(),
    ];
    SettlingQuery { sql, readings }
}

/// A string literal PostgreSQL reads back byte for byte: `U&'...'` with `\XXXX` escapes when the
/// text holds a backslash, a character that would break the line or read as markdown or HTML,
/// or a `##[` a CI log would read as a command; plain `'...'` otherwise.
pub(crate) fn string_literal(text: &str) -> String {
    let hazard = |c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '<' | '>' | '`' | '*');
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
