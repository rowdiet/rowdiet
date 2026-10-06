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
            string_literal(identifier(schema)),
            string_literal(identifier(name))
        ),
        [name] => format!("pg_catalog.format('%I', {})", string_literal(identifier(name))),
        [] => "''".to_string(),
    };
    let looked_up = match relation {
        [_, _, ..] => "'here'",
        [_] | [] => "'on the search path (' || current_setting('search_path') || ')'",
    };
    let names: Vec<String> = columns.iter().map(|c| string_literal(identifier(&c.name))).collect();
    let plain = if storage {
        "
           ARRAY(SELECT coalesce(a.attstorage = 'p', false)::text
                 FROM unnest(t.names) WITH ORDINALITY AS w(name, k)
                 LEFT JOIN pg_catalog.pg_attribute AS a
                        ON a.attrelid = t.r AND a.attname::text = w.name AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY w.k) AS plain,
           ARRAY(SELECT coalesce(y.typstorage = 'p', false)::text
                 FROM unnest(t.names) WITH ORDINALITY AS w(name, k)
                 LEFT JOIN pg_catalog.pg_attribute AS a
                        ON a.attrelid = t.r AND a.attname::text = w.name AND a.attnum > 0 AND NOT a.attisdropped
                 LEFT JOIN pg_catalog.pg_type AS y ON y.oid = a.atttypid
                 ORDER BY w.k) AS typplain,
           c.relnatts, current_setting('server_version_num')::int AS version,"
    } else {
        ""
    };
    format!(
        "WITH target AS (
    SELECT n.name, {looked_up} AS looked_up, pg_catalog.to_regclass(n.name) AS r, ARRAY[{names}]::text[] AS names
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
const CANNOT_SETTLE: &str = "CASE WHEN a.r IS NULL THEN 'cannot settle: no relation ' || a.name || ' ' || a.looked_up END,
        CASE WHEN a.relkind NOT IN ('r', 'p') THEN 'cannot settle: ' || a.name || ' is not a table' END,
        CASE WHEN a.r IS NOT NULL AND cardinality(a.missing) > 0 THEN 'cannot settle: column(s) ' || pg_catalog.array_to_string(a.missing, ', ')
             || ' not in the table yet; apply the migration that adds them and load rows first' END,
        CASE WHEN a.n = 0 AND a.relkind IN ('r', 'p') AND cardinality(a.missing) = 0
             THEN 'cannot settle: no rows to compare' END";

/// The walk's end offset for `order`: each step aligns to `e{i}` and adds `z{i}` from `forms`.
fn walk(order: &[usize], forms: &str) -> String {
    let mut offset = "0".to_string();
    for &index in order {
        let i = index + 1;
        let (e, z) = (format!("{forms}.e{i}"), format!("{forms}.z{i}"));
        offset = format!("(({offset} + {e} - 1) / {e} * {e} + {z})");
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

/// One `WHEN` of a value's form: the alignment it takes and the bytes it stores. `when: None`
/// is the `ELSE`.
struct Form {
    when: Option<String>,
    align: String,
    bytes: String,
}

fn form(when: Option<String>, align: impl ToString, bytes: impl ToString) -> Form {
    Form {
        when,
        align: align.to_string(),
        bytes: bytes.to_string(),
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

/// A value's form as a rewrite in either order stores it. `q{i}` is how much a one-column row
/// adds to the value: 24 in line, 21 for a 4-byte header a short one could replace, more for a
/// value fetched from TOAST. `plain` and `typplain` are the placeholders for the column's and
/// its type's `PLAIN` storage.
fn reader_forms(i: usize, column: &QueryColumn, plain: &str, typplain: &str) -> Vec<Form> {
    let (align, width) = align_and_width(column.kind);
    if let Some(width) = width {
        return vec![form(Some(format!("m.n{i}")), 1, 0), form(None, align, width)];
    }
    let (s, c, l, q) = (
        format!("m.s{i}"),
        format!("m.c{i}"),
        format!("m.l{i}"),
        format!("m.q{i}"),
    );
    let mut forms = vec![form(Some(format!("{s} IS NULL")), 1, 0)];
    if column.octets {
        forms.extend([
            form(Some(format!("{c} AND {q} > 24")), 1, 18),
            form(Some(c), align, &s),
            form(Some(format!("{s} = {l}")), 1, 18),
            form(
                Some(format!("{s} - {l} = 1 OR ({l} <= 126 AND NOT {plain})")),
                1,
                format!("{l} + 1"),
            ),
            form(None, align, format!("{l} + 4")),
        ]);
    } else {
        forms.extend([
            form(Some(typplain.to_string()), align, &s),
            form(Some(format!("{q} > 24")), 1, 18),
            form(Some(c), align, &s),
            form(Some(format!("{q} < 24 AND NOT {plain}")), 1, format!("{s} - 3")),
            form(Some(format!("{q} < 24")), align, &s),
            form(Some(format!("{s} <= 127")), 1, &s),
            form(None, align, &s),
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
    // Placeholders: the relation, ONLY or nothing, its attribute count, then per column its
    // name, its PLAIN flag, its type's PLAIN flag.
    let name = |i: usize| format!("r.%{}$I", i + 3);
    let plain = |i: usize| format!("%{}$s", k + i + 3);
    let typplain = |i: usize| format!("%{}$s", 2 * k + i + 3);
    let mut sizes = Vec::new();
    let mut nulls = Vec::new();
    let mut picks = Vec::new();
    let mut fetched = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let i = index + 1;
        let (align, width) = align_and_width(column.kind);
        if width.is_some() {
            sizes.push(format!("{} IS NULL AS n{i}", name(i)));
            nulls.push(format!("m.n{i}"));
            fetched.push(format!("v.e{i} AS e{i}, v.z{i} AS z{i}"));
        } else {
            let v = name(i);
            sizes.push(format!(
                "pg_catalog.pg_column_size({v}) AS s{i}, pg_catalog.pg_column_compression({v}) IS NOT NULL AS c{i}"
            ));
            let row = format!("pg_catalog.pg_column_size(ROW({v})) - pg_catalog.pg_column_size({v})");
            if column.octets {
                sizes.push(format!(
                    "pg_catalog.octet_length({v}) AS l{i}, CASE WHEN pg_catalog.pg_column_compression({v}) IS NOT NULL THEN {row} END AS q{i}"
                ));
            } else {
                sizes.push(format!("{row} AS q{i}"));
            }
            nulls.push(format!("m.s{i} IS NULL"));
            let outside = if column.octets {
                format!("CASE WHEN m.c{i} THEN m.q{i} > 24 ELSE m.s{i} = m.l{i} END")
            } else {
                format!("m.q{i} > 24")
            };
            picks.push(format!("coalesce({outside}, false) AS x{i}"));
            fetched.push(format!(
                "CASE WHEN v.x{i} THEN {align} ELSE v.e{i} END AS e{i}, CASE WHEN v.x{i} THEN m.s{i} + 4 ELSE v.z{i} END AS z{i}"
            ));
        }
        let forms = reader_forms(i, column, &plain(i), &typplain(i));
        picks.push(format!("{} AS e{i}", case(&forms, |f| f.align.clone())));
        picks.push(format!("{} AS z{i}", case(&forms, |f| f.bytes.clone())));
    }
    let header = |natts: &str, dropped: &str| {
        format!("CASE WHEN v.nulls{dropped} THEN (23 + ({natts} + 7) / 8 + 7) / 8 * 8 ELSE 24 END")
    };
    let threshold = "(current_setting('block_size')::int - 40) / 32 * 8";
    let template = format!(
        "SELECT count(*) AS n,
       count(*) FILTER (WHERE NOT w.crosses AND w.alt < w.cur) AS alternative_smaller,
       count(*) FILTER (WHERE NOT w.crosses AND w.alt > w.cur) AS current_smaller,
       coalesce(sum(w.cur - w.alt) FILTER (WHERE NOT w.crosses), 0) AS bytes_saved,
       count(*) FILTER (WHERE w.crosses) AS approximate_rows
FROM %2$s %1$s AS r
CROSS JOIN LATERAL (
    SELECT {sizes}
    OFFSET 0
) AS m
CROSS JOIN LATERAL (
    SELECT {picks},
           {nulls} AS nulls
    OFFSET 0
) AS v
CROSS JOIN LATERAL (
    SELECT {fetched}
    OFFSET 0
) AS t
CROSS JOIN LATERAL (
    SELECT {cur_walk} AS oc,
           {alt_walk} AS oa,
           {cur_fetched} AS tc,
           {alt_fetched} AS ta
    OFFSET 0
) AS o
CROSS JOIN LATERAL (
    SELECT {cur_header} + o.tc <= {threshold} AS fits_current,
           {alt_header} + o.ta <= {threshold} AS fits_alternative
    OFFSET 0
) AS f
CROSS JOIN LATERAL (
    SELECT CASE WHEN f.fits_current AND f.fits_alternative THEN {cur_inline} ELSE {cur} END AS cur,
           CASE WHEN f.fits_current AND f.fits_alternative THEN {alt_inline} ELSE {alt} END AS alt,
           f.fits_current <> f.fits_alternative AS crosses
    OFFSET 0
) AS w",
        sizes = sizes.join(",\n           "),
        picks = picks.join(",\n           "),
        nulls = nulls.join(" OR "),
        fetched = fetched.join(",\n           "),
        cur_walk = walk(current, "v"),
        alt_walk = walk(alternative, "v"),
        cur_fetched = walk(current, "t"),
        alt_fetched = walk(alternative, "t"),
        cur = cost("o.oc", measure),
        alt = cost("o.oa", measure),
        cur_inline = cost("o.tc", measure),
        alt_inline = cost("o.ta", measure),
        cur_header = header("%3$s", &format!(" OR %3$s > {k}")),
        alt_header = header(&k.to_string(), ""),
    );
    let tag = dollar_tag(&template);
    let field = |f: &str| format!("substring(s.x::text FROM '<{f}>(-?[0-9]+)</{f}>')::bigint AS {f}");
    let sql = format!(
        "{catalog}, settled AS (
    SELECT i.*,
           (SELECT pg_catalog.query_to_xml(pg_catalog.format({tag}
{template}
{tag}, VARIADIC ARRAY[i.r::text, CASE WHEN i.relkind = 'p' THEN '' ELSE 'ONLY' END, i.relnatts::text]
                       || i.names || i.plain || i.typplain),
                   false, false, '')
            WHERE i.relkind IN ('r', 'p') AND cardinality(i.missing) = 0 AND i.version >= 140000) AS x
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
        CASE WHEN a.version < 140000 THEN 'cannot settle: the reader query needs PostgreSQL 14 or later; '
             || 'the pageinspect query (--settle-exact) runs on older servers' END,
        {CANNOT_SETTLE},
        CASE WHEN a.relkind = 'p' THEN 'partitioned: every partition read' END,
        CASE WHEN a.foreign_leaves > 0 THEN a.foreign_leaves || ' foreign partition(s) read through their server, sizes approximate' END,
        CASE WHEN a.relkind = 'r' AND a.children > 0 THEN 'inheritance parent: its own rows only, ' || a.children || ' child table(s) not read' END,
        CASE WHEN cardinality(a.fast_default) > 0 THEN 'column(s) ' || pg_catalog.array_to_string(a.fast_default, ', ')
             || ' carry a default older rows do not store; both orders count it as a rewrite stores it' END,
        CASE WHEN a.approximate_rows > 0 THEN 'approximate_rows cross the TOAST threshold between the two orders and are left out of the counts' END) AS note
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
        "runs as any role that can read the table on PostgreSQL 14 or later: each value's form comes \
         from pg_column_size, pg_column_compression, octet_length for text types, and the size of a \
         one-column row built from the value, which fetches out-of-line non-text and compressed \
         values from TOAST once; both orders are counted as a rewrite would store the row, an \
         out-of-line value back in line where the row then fits under the TOAST threshold in both"
            .to_string(),
        "approximate_rows counts rows that fit under the TOAST threshold in one order and not the \
         other: a rewrite would toast them in one order only, so they are left out of the other \
         counts"
            .to_string(),
        "note gives the reason when the query cannot settle (no such table, columns not added yet, \
         no rows, a server before PostgreSQL 14) and says what it read: partitions, inheritance \
         children, fast defaults"
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
        cur_walk = walk(current, "v"),
        cur = cost("c.o", measure),
        alt = cost(&walk(alternative, "v"), measure),
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

/// A name as PostgreSQL stores it: cut to 63 bytes at a character boundary.
fn identifier(name: &str) -> &str {
    let mut end = name.len().min(63);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
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
