//! Repository tasks. `cargo run -p xtask -- measure` regression-tests the layout model's
//! claims against real tuples: it builds the release binary, applies DDL fixtures to a
//! disposable PostgreSQL (Docker, pageinspect), inserts short-heavy and long-heavy workloads,
//! and asserts the properties from the decision-policy spec:
//!
//! (a) measured per-row padding falls inside every reported [min, max];
//! (b) whenever the tool recommends a reorder, the suggested order measures no worse than the
//!     current order on every row of both workloads;
//! (c) every declared frontier band holds row by row on a workload that sweeps payload residues
//!     inside the band: each row's saving lies inside the band's bounds, the declared winner
//!     matches the signs measured, and where every residue is controllable the measured extremes
//!     equal the bounds; every declared boundary pair flips the measured winner;
//! (d) the gated headline never exceeds the dominance engine's proven maximum saving;
//! (e) at the exact tier, which decides in row size, every row takes the reported size: rows
//!     without NULLs the footprint, rows with one the bitmap header and the reported range.
//!
//! Fixtures with nullable columns also run workloads that hold NULL in them, one bit of the row
//! counter per nullable column, so every NULL pattern over up to ten columns occurs; those rows
//! are held to the bounds over every NULL pattern and the rows without NULLs to the bounds in
//! rows that store every column. At the exact tier property (b) and the band checks compare
//! row sizes (the tuple length rounded up to 8), which is what the tool reports there.
//!
//! Rows are generated deterministically, so the current and the alternative table hold the same
//! values row for row and pair by insertion order. Padding is the tuple length minus the header
//! and the stored bytes of every attribute (`heap_page_item_attrs`), which counts short headers,
//! compressed values and TOAST pointers as they sit on the page. Array workloads include
//! uncompressed short and long arrays, compressed inline arrays, and TOAST pointers.
//!
//! Skipped (exit 0, loud) when no reachable container: the harness is for maintainers and
//! Docker-equipped CI legs, and plain CI must not fail for lacking a database. Container/user/db
//! come from `ROWDIET_MEASURE_CONTAINER` / `_USER` / `_DB` (defaults: condescending_tu, test,
//! test). Every table it creates carries the `ROWDIET_MEASURE_PREFIX` prefix (default `synb_`)
//! and is dropped afterwards, so concurrent users of one database keep apart.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("measure") => measure(),
        other => {
            eprintln!("usage: cargo run -p xtask -- measure (got {other:?})");
            std::process::exit(2);
        }
    }
}

struct Pg {
    container: String,
    user: String,
    db: String,
}

impl Pg {
    fn query(&self, sql: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                &self.container,
                "psql",
                "-U",
                &self.user,
                "-d",
                &self.db,
                "-v",
                "ON_ERROR_STOP=1",
                "-tA",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("docker exec: {e}"))?;
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(sql.as_bytes())
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "psql failed: {}\nsql: {sql}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// One measured table under one workload.
#[derive(Debug, Clone, Copy)]
struct Measured {
    rows: u64,
    mean: f64,
    min: i64,
    max: i64,
}

/// Two tables holding the same rows, paired by their values. `saving` is current minus
/// alternative padding per row, `size_saving` the same for row sizes.
#[derive(Debug, Clone, Copy)]
struct Paired {
    current: Measured,
    alternative: Measured,
    saving_min: i64,
    saving_max: i64,
    size_saving_min: i64,
    size_saving_max: i64,
    /// Current minus alternative over the rows stored alike: padding, then row size.
    saving_sum: i64,
    size_saving_sum: i64,
    /// Rows the toaster stored differently in the two orders, left out of the savings.
    stored_differently: u64,
}

struct Column {
    name: &'static str,
    sql_type: &'static str,
    nullable: bool,
}

/// (name, SQL type, nullable) as the table DDL and the workload generators see a column.
type Col<'a> = (&'a str, &'a str, bool);

/// The table-name prefix every created table carries.
fn prefix() -> String {
    std::env::var("ROWDIET_MEASURE_PREFIX").unwrap_or_else(|_| "synb_".into())
}

fn is_varlena(sql_type: &str) -> bool {
    matches!(sql_type, "text" | "float8[]" | "numeric" | "jsonb" | "text[]") || sql_type.starts_with("varchar")
}

/// A per-column seed from the name, the same in every order of the table. Distinct values keep
/// the toaster's size ties apart, which it breaks by attribute number and so by order.
fn column_seed(name: &str) -> usize {
    name.bytes().map(usize::from).sum::<usize>() % 97
}

/// Values for an uncompressed float8[] of `count` elements.
fn float_array(count: &str, seed: usize) -> String {
    format!("(SELECT array_agg((g * 1000 + x + {seed})::float8) FROM generate_series(1, ({count})::int) x)")
}

/// Mostly-zero float8[] past the tuple threshold: compressed inline, at a length that varies.
fn compressible_array(variety: &str, seed: usize) -> String {
    format!(
        "(SELECT array_agg(CASE WHEN x % (1 + ({variety}) % 17) = 0 THEN (x + {seed})::float8 ELSE 0 END) \
         FROM generate_series(1, (300 + {seed} + ({variety}) % 64)::int) x)"
    )
}

/// Incompressible float8[] past the tuple threshold: moved out of line, an 18-byte pointer.
fn toasted_array(seed: usize) -> String {
    format!("(SELECT array_agg(sin(g * 1000 + x)) FROM generate_series(1, 400 + {seed}) x)")
}

/// Per-type (short-heavy, long-heavy) generators. `g` is the row counter, so widths sweep the
/// payload residues deterministically and every table gets the same rows.
fn type_spec(sql_type: &str, seed: usize) -> (String, String) {
    if let Some(n) = sql_type
        .strip_prefix("varchar(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let generator = format!(
            "repeat('x', (g % {})::int)",
            n.parse::<u64>().expect("varchar length") + 1
        );
        return (generator.clone(), generator);
    }
    match sql_type {
        "text" => (
            "repeat('x', (g % 16)::int)".to_string(),
            "repeat('x', (130 + (g * 7) % 110)::int)".to_string(),
        ),
        "float8[]" => (
            float_array("1 + g % 2", seed),
            format!(
                "CASE WHEN g % 2 = 0 THEN {} ELSE {} END",
                float_array("25 + g % 16", seed),
                compressible_array("g", seed)
            ),
        ),
        "numeric" => (
            "((g % 997) * 1.25)::numeric".to_string(),
            "('1' || repeat('7', (260 + g % 23)::int))::numeric".to_string(),
        ),
        "jsonb" => (
            "jsonb_build_object('k', repeat('x', (g % 16)::int))".to_string(),
            "jsonb_build_object('k', repeat('x', (200 + g % 40)::int))".to_string(),
        ),
        "text[]" => (
            "ARRAY[repeat('x', (g % 5)::int)]".to_string(),
            "(SELECT array_agg(repeat('x', (x % 7)::int)) FROM generate_series(1, (30 + g % 9)::int) x)".to_string(),
        ),
        "bigint" => ("42".into(), "42".into()),
        "int4_alias_integer" | "integer" => ("7".into(), "7".into()),
        "smallint" => ("1".into(), "1".into()),
        "boolean" => ("true".into(), "true".into()),
        "float8" => ("1.5".into(), "1.5".into()),
        "timestamp" => (
            "'2024-01-02 03:04:05'::timestamp".into(),
            "'2024-01-02 03:04:05'::timestamp".into(),
        ),
        "timestamptz" => (
            "'2024-01-02 03:04:05+00'::timestamptz".into(),
            "'2024-01-02 03:04:05+00'::timestamptz".into(),
        ),
        "timetz" => ("'03:04:05+02'::timetz".into(), "'03:04:05+02'::timetz".into()),
        "macaddr" => ("'08:00:2b:01:02:03'".into(), "'08:00:2b:01:02:03'".into()),
        other => panic!("no type spec for {other}"),
    }
}

/// A band workload's generator for one varlena and whether it sweeps every residue: `selector`
/// picks a payload residue (0..8) and, for arrays, the storage shape. Long texts are 128 +
/// residue bytes and short texts residue bytes. Long arrays are uncompressed (payload 4 mod 8)
/// or compressed inline (any residue); short arrays are uncompressed or a TOAST pointer.
fn band_generator(sql_type: &str, long: bool, selector: &str, seed: usize) -> (String, bool) {
    let swept = match (sql_type, long) {
        ("text", true) => Some(format!("repeat('x', (128 + {selector})::int)")),
        ("text", false) => Some(format!("repeat('x', ({selector})::int)")),
        ("float8[]", true) => Some(format!(
            "CASE WHEN {selector} < 4 THEN {} ELSE {} END",
            float_array(&format!("16 + {selector}"), seed),
            compressible_array(&format!("g * 7 + {selector}"), seed)
        )),
        ("float8[]", false) => Some(format!(
            "CASE WHEN {selector} < 6 THEN {} ELSE {} END",
            float_array(&format!("1 + {selector} % 3"), seed),
            toasted_array(seed)
        )),
        _ => None,
    };
    // Other types fall back to their plain generators, which do not sweep every residue.
    match swept {
        Some(generator) => (generator, true),
        None => {
            let (short, long_gen) = type_spec(sql_type, seed);
            (if long { long_gen } else { short }, false)
        }
    }
}

/// Per-column generator overrides forming one workload.
type Workload = Vec<(&'static str, String)>;

struct Fixture {
    name: &'static str,
    columns: Vec<Column>,
    /// (current-wins workload, alternative-wins workload): the pair must flip the measured
    /// winner across the frontier boundary.
    boundary: Option<(Workload, Workload)>,
    /// The named text column is STORAGE EXTERNAL; its long workload is a toasted 4 kB payload
    /// measured as the 18-byte pointer.
    toast_column: Option<&'static str>,
    /// An order the tool must not recommend, and a workload on which it measures worse than the
    /// written order on some row.
    witness: Option<(Vec<&'static str>, Workload)>,
}

fn col(name: &'static str, sql_type: &'static str) -> Column {
    Column {
        name,
        sql_type,
        nullable: false,
    }
}

fn ncol(name: &'static str, sql_type: &'static str) -> Column {
    Column {
        name,
        sql_type,
        nullable: true,
    }
}

fn numbered(prefix: &str, count: u32, sql_type: &'static str) -> Vec<Column> {
    (0..count)
        .map(|i| col(Box::leak(format!("{prefix}{i}").into_boxed_str()), sql_type))
        .collect()
}

#[allow(clippy::too_many_lines)]
fn fixtures() -> Vec<Fixture> {
    let plain = |name, columns| Fixture {
        name,
        columns,
        boundary: None,
        toast_column: None,
        witness: None,
    };
    let mut out = vec![
        // Issue #1's control table: measured flat zero, must pass clean.
        plain(
            "grouped",
            vec![
                col("score", "float8"),
                col("seen", "timestamp"),
                col("tag", "int4_alias_integer"),
                col("a", "text"),
                col("b", "text"),
                col("c", "text"),
                col("d", "text"),
                col("e", "text"),
            ],
        ),
        // Issue #1's repro: dominance finding, suggested must measure no worse on both loads.
        plain(
            "interleaved",
            vec![
                col("a", "text"),
                col("tag", "int4_alias_integer"),
                col("b", "text"),
                col("c", "text"),
                col("d", "text"),
                col("e", "text"),
                col("score", "float8"),
                col("seen", "timestamp"),
            ],
        ),
        // Issue #10's W1/W2 pair: frontier with a boundary that must flip the measured winner.
        Fixture {
            name: "band",
            columns: vec![col("k", "bigint"), col("txt", "text"), col("arr", "float8[]")],
            boundary: Some((
                vec![
                    ("txt", "repeat('x', (130 + (g * 7) % 110)::int)".into()),
                    ("arr", float_array("1 + g % 2", column_seed("arr"))),
                ],
                vec![
                    ("txt", "repeat('x', (g % 16)::int)".into()),
                    ("arr", float_array("25 + g % 16", column_seed("arr"))),
                ],
            )),
            toast_column: None,
            witness: None,
        },
        // (float8[], int4): the old model demanded a swap that measures 4 B/row worse; the
        // policy keeps the order. Bounds still checked under both loads.
        plain(
            "arr_first",
            vec![col("arr", "float8[]"), col("n", "int4_alias_integer")],
        ),
        // (int4, float8[]): frontier; whole-float8 long arrays make the alternative win flat.
        Fixture {
            name: "int_first",
            columns: vec![col("n", "int4_alias_integer"), col("arr", "float8[]")],
            boundary: Some((
                vec![("arr", float_array("1 + g % 2", column_seed("arr")))],
                vec![("arr", float_array("25 + g % 16", column_seed("arr")))],
            )),
            toast_column: None,
            witness: None,
        },
        // (timetz, timetz, text): the certainty trade. Text width 4 keeps the current order
        // ahead (flat 4 vs flat 7); width 2 favors the interposed alternative (flat 1).
        Fixture {
            name: "ttv",
            columns: vec![col("t1", "timetz"), col("t2", "timetz"), col("v", "text")],
            boundary: Some((
                vec![("v", "repeat('x', 4)".into())],
                vec![("v", "repeat('x', 2)".into())],
            )),
            toast_column: None,
            witness: None,
        },
        // (text, macaddr): frontier; short payloads favor macaddr-first, 128-byte texts favor
        // text-first (the text pads 0 at offset 0 and the macaddr lands aligned).
        Fixture {
            name: "tm",
            columns: vec![col("t", "text"), col("m", "macaddr")],
            boundary: Some((
                vec![("t", "repeat('x', 128)".into())],
                vec![("t", "repeat('x', (g % 16)::int)".into())],
            )),
            toast_column: None,
            witness: None,
        },
        // (text, boolean, bigint): dominance finding via the aligned slot and the char tail.
        plain("frac", vec![col("t", "text"), col("b", "boolean"), col("x", "bigint")]),
        // The pole-completeness regression: no scalar-objective pole proposes the dominating
        // (bigint, text, macaddr), only the exhaustive sweep does; property (b) then verifies
        // the reorder on disk.
        plain("miss", vec![col("t", "text"), col("k", "bigint"), col("m", "macaddr")]),
        // The varlena-identity regression: the dominating (m1, t2, t1, m2) swaps the two texts
        // relative to the written order, so a class-collapsed candidate space never proposes
        // it; property (b) verifies the reorder on disk.
        plain(
            "tmtm",
            vec![
                col("t1", "text"),
                col("m1", "macaddr"),
                col("t2", "text"),
                col("m2", "macaddr"),
            ],
        ),
        // TOAST: 4 kB EXTERNAL payloads store an 18-byte unaligned pointer; the pinned residue
        // sits inside the reported range and the dominating reorder must still measure better.
        Fixture {
            name: "toastish",
            columns: vec![col("t", "text"), col("x", "float8")],
            boundary: None,
            toast_column: Some("t"),
            witness: None,
        },
        // Array residues: a short uncompressed float8[] stores payload 4 mod 8, which makes
        // (macaddr, smallint, a2, a1) dominate; any-residue modeling called this table clean.
        plain(
            "pin",
            vec![
                col("s", "smallint"),
                col("a1", "float8[]"),
                col("a2", "float8[]"),
                col("m", "macaddr"),
            ],
        ),
        // A trimmed sweep: four always-short varchars and a text exhaust the comparison budget;
        // the dominating minimax pole must be recommended and measure no worse.
        plain(
            "orders5",
            vec![
                col("id", "bigint"),
                col("country", "varchar(2)"),
                col("currency", "varchar(3)"),
                col("status", "varchar(5)"),
                col("channel", "varchar(4)"),
                col("note", "text"),
            ],
        ),
        // The same shape with varchars wide enough to compress, which can then align.
        plain(
            "orders",
            vec![
                col("id", "bigint"),
                col("country", "varchar(2)"),
                col("currency", "varchar(3)"),
                col("status", "varchar(16)"),
                col("channel", "varchar(12)"),
                col("note", "text"),
            ],
        ),
    ];
    // Fixed waste past 24 fixed columns: the prefix repack must gate it at any width.
    let mut wide36 = Vec::new();
    for (flag, reference) in numbered("flag", 10, "boolean")
        .into_iter()
        .zip(numbered("ref", 10, "bigint"))
    {
        wide36.extend([flag, reference]);
    }
    for (kind, at) in numbered("kind", 5, "smallint")
        .into_iter()
        .zip(numbered("at", 5, "timestamptz"))
    {
        wide36.extend([kind, at]);
    }
    wide36.extend([
        col("name", "varchar(20)"),
        col("description", "text"),
        col("payload", "jsonb"),
        col("notes", "text"),
        col("amount", "numeric"),
        col("tags", "text[]"),
    ]);
    out.push(plain("wide36", wide36));
    // The exact tier past 24 columns: alternating timetz and int4 saves a MAXALIGN rung.
    let mut exact26 = Vec::new();
    for (tz, int) in numbered("tz", 11, "timetz")
        .into_iter()
        .zip(numbered("i", 11, "integer"))
    {
        exact26.extend([tz, int]);
    }
    exact26.extend([
        col("tz11", "timetz"),
        col("tz12", "timetz"),
        col("i11", "integer"),
        col("i12", "integer"),
    ]);
    out.push(plain("exact26", exact26));
    // The headline-invariant repro: the dominating repack removes 28 B of certain padding but
    // pays new data-dependent pads, so only 24 B is attainable; property (d) pins the headline
    // to the proven range.
    let mut m23 = numbered("tz", 6, "timetz");
    m23.extend(numbered("mm", 6, "macaddr"));
    m23.extend(numbered("bb", 5, "bigint"));
    m23.extend(numbered("ss", 3, "smallint"));
    m23.extend(numbered("tt", 3, "text"));
    out.push(plain("m23", m23));
    // The 25-column cap cliff: deterministic 24 B/row, must gate and must measure.
    let mut cliff = numbered("tz", 4, "timetz");
    cliff.extend(numbered("m", 4, "macaddr"));
    cliff.extend(numbered("b", 4, "bigint"));
    cliff.extend(numbered("i", 4, "int4_alias_integer"));
    cliff.extend(numbered("s", 4, "smallint"));
    cliff.extend(numbered("f", 4, "boolean"));
    cliff.push(col("note", "text"));
    out.push(plain("cliff25", cliff));
    // The wide-table false negative: 21 bigints push the whole-order search over budget; the
    // fixed-prefix repack must still be found, gated, and measured better.
    let mut wide = numbered("w", 21, "bigint");
    wide.push(col("t1", "timetz"));
    wide.push(col("t2", "timetz"));
    wide.push(col("s", "smallint"));
    wide.push(col("note", "text"));
    out.push(plain("wide25", wide));
    out.extend(null_fixtures());
    out
}

/// Fixtures whose verdicts depend on NULLs in nullable columns.
#[allow(clippy::too_many_lines)]
fn null_fixtures() -> Vec<Fixture> {
    let plain = |name, columns| Fixture {
        name,
        columns,
        boundary: None,
        toast_column: None,
        witness: None,
    };
    let long_text = || "repeat('x', (130 + (g * 7) % 110)::int)".to_string();
    let mut out = vec![
        // The written order pads zero when every column is stored, which read as clean without
        // NULLs; rows with n NULL pad 2 before i, and (i, s, n, t) never does.
        plain(
            "nullfill",
            vec![
                col("s", "smallint"),
                ncol("n", "smallint"),
                col("i", "integer"),
                col("t", "text"),
            ],
        ),
        // Exact tier: both orders round to 48 bytes when every column is stored, but rows with
        // n NULL shrink to 40 under (z, n, b1, b2).
        plain(
            "nullexact",
            vec![
                col("b1", "boolean"),
                col("b2", "boolean"),
                ncol("n", "integer"),
                col("z", "timetz"),
            ],
        ),
        // Exact tier decided in row size: (c0, c2, c1) pads 2 more when every column is stored,
        // where both orders round to 56 bytes, and saves 8 in rows where c0 is NULL.
        plain(
            "nulltz",
            vec![ncol("c0", "timetz"), col("c1", "smallint"), ncol("c2", "timetz")],
        ),
        // An exact-tier frontier decided by NULLs alone.
        plain(
            "nullmac",
            vec![
                ncol("c0", "macaddr"),
                ncol("c1", "macaddr"),
                ncol("c2", "boolean"),
                ncol("c3", "smallint"),
            ],
        ),
        // Without NULLs (m, s, t) is never worse; with m NULL a long text pads 2 behind the
        // smallint while the written order pads at most 1.
        Fixture {
            name: "nullslot",
            columns: vec![ncol("m", "macaddr"), col("t", "text"), col("s", "smallint")],
            boundary: None,
            toast_column: None,
            witness: Some((vec!["m", "s", "t"], vec![("m", "NULL".into()), ("t", long_text())])),
        },
        // A NULL float8[] advances 0 bytes where every stored short array advances 5: (a, s, i)
        // dominates when the array is NOT NULL and pads 2 before i in the rows where it is NULL.
        Fixture {
            name: "nullarr",
            columns: vec![col("i", "integer"), ncol("a", "float8[]"), col("s", "smallint")],
            boundary: None,
            toast_column: None,
            witness: Some((vec!["a", "s", "i"], vec![("a", "NULL".into())])),
        },
        // A realistic shape with five nullable columns of four widths.
        plain(
            "nullreal",
            vec![
                col("id", "bigint"),
                col("created_at", "timestamptz"),
                ncol("deleted", "boolean"),
                ncol("owner_id", "bigint"),
                ncol("score", "integer"),
                col("name", "text"),
                ncol("kind", "smallint"),
                ncol("updated_at", "timestamptz"),
                col("payload", "jsonb"),
                ncol("rank", "integer"),
            ],
        ),
    ];
    // Issue #4's cols9: rows holding a NULL carry a 32-byte header.
    let cols9 = (0..9u32)
        .map(|i| ncol(Box::leak(format!("c{i}").into_boxed_str()), "bigint"))
        .collect();
    out.push(plain("nullcols9", cols9));
    // 24 nullable regular columns written in a padding order: no pair comparison fits a budget,
    // and the sorted order, which pads zero in every NULL pattern, must still be recommended.
    let types = ["boolean", "bigint", "smallint", "integer"];
    let reg24 = (0..24usize)
        .map(|i| ncol(Box::leak(format!("c{i}").into_boxed_str()), types[i % 4]))
        .collect();
    out.push(plain("nullreg24", reg24));
    out
}

fn sql_type_name(t: &str) -> &str {
    if t == "int4_alias_integer" { "integer" } else { t }
}

fn measure() {
    let pg = Pg {
        container: std::env::var("ROWDIET_MEASURE_CONTAINER").unwrap_or_else(|_| "condescending_tu".into()),
        user: std::env::var("ROWDIET_MEASURE_USER").unwrap_or_else(|_| "test".into()),
        db: std::env::var("ROWDIET_MEASURE_DB").unwrap_or_else(|_| "test".into()),
    };
    match pg.query("SELECT 1;") {
        Ok(_) => {}
        Err(e) => {
            eprintln!("measure: skipped, no reachable docker Postgres ({e})");
            return;
        }
    }
    pg.query("CREATE EXTENSION IF NOT EXISTS pageinspect;")
        .expect("pageinspect");
    let binary = build_binary();
    let mut failures: Vec<String> = Vec::new();
    println!("| fixture | table | workload | model [min,max] | measured mean (min-max) | n |");
    println!("|---|---|---|---|---|---|");
    for fixture in fixtures() {
        run_fixture(&pg, &binary, &fixture, &mut failures);
    }
    for fixture in block_fixtures() {
        run_block_fixture(&pg, &binary, &fixture, &mut failures);
    }
    run_plain_checks(&pg, &binary, &mut failures);
    run_report_only_checks(&binary, &mut failures);
    let pattern = format!("{}%", prefix().replace('_', "\\_"));
    pg.query(&format!(
        "DO $$ DECLARE r record; BEGIN FOR r IN SELECT tablename FROM pg_tables WHERE tablename LIKE '{pattern}' LOOP EXECUTE 'DROP TABLE ' || quote_ident(r.tablename); END LOOP; END $$;"
    ))
    .expect("cleanup");
    let remaining = pg
        .query(&format!(
            "SELECT count(*) FROM pg_tables WHERE tablename LIKE '{pattern}';"
        ))
        .expect("count");
    assert_eq!(remaining.trim(), "0", "{} tables must all be dropped", prefix());
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("rowdiet-xtask-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("rowdiet-xtask-{}-blocks", std::process::id())));
    if failures.is_empty() {
        println!("\nmeasure: all model claims hold against real tuples");
    } else {
        eprintln!("\nmeasure: {} claim(s) falsified:", failures.len());
        for f in &failures {
            eprintln!("  FAIL {f}");
        }
        std::process::exit(1);
    }
}

fn build_binary() -> std::path::PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release", "-p", "rowdiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "release build failed");
    let root = std::env::var("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .expect("manifest dir");
    root.parent().expect("workspace root").join("target/release/rowdiet")
}

/// Analyze `sql` with the built binary and return the parsed JSON envelope.
fn analyze(binary: &std::path::Path, sql: &str, extra_args: &[&str]) -> serde_json::Value {
    let dir = std::env::temp_dir().join(format!("rowdiet-xtask-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("V1__fixture.sql");
    std::fs::write(&file, sql).expect("write fixture");
    let out = Command::new(binary)
        .arg(&file)
        .args(["--format", "json"])
        .args(extra_args)
        .output()
        .expect("run rowdiet");
    serde_json::from_slice(&out.stdout).expect("rowdiet JSON")
}

fn create_table_sql(table: &str, columns: &[Col<'_>]) -> String {
    let cols: Vec<String> = columns
        .iter()
        .map(|(name, ty, nullable)| {
            let constraint = if *nullable { "" } else { " NOT NULL" };
            format!("{name} {}{constraint}", sql_type_name(ty))
        })
        .collect();
    format!("CREATE TABLE {table} ({});", cols.join(", "))
}

/// The padding bounds a workload's rows are held to: over every NULL pattern when it stores
/// NULLs, else in rows that store every column.
fn model_bounds(stats: &serde_json::Value, nulls: bool) -> (i64, i64) {
    let without = &stats["without_nulls"];
    if !nulls && without.is_object() {
        (
            without["min"].as_i64().expect("min"),
            without["max"].as_i64().expect("max"),
        )
    } else {
        (
            stats["padding_min"].as_i64().expect("min"),
            stats["padding_max"].as_i64().expect("max"),
        )
    }
}

#[allow(clippy::too_many_lines)]
fn run_fixture(pg: &Pg, binary: &std::path::Path, fixture: &Fixture, failures: &mut Vec<String>) {
    let current: Vec<Col<'_>> = fixture
        .columns
        .iter()
        .map(|c| (c.name, c.sql_type, c.nullable))
        .collect();
    let null_order: Vec<&str> = fixture.columns.iter().filter(|c| c.nullable).map(|c| c.name).collect();
    let cur_table = format!("{}{}", prefix(), fixture.name);
    let report = analyze(binary, &create_table_sql(&cur_table, &current), &[]);
    let table_report = &report["analysis"]["tables"][0];
    let avoidable = table_report["avoidable_bytes_per_row"].as_f64().expect("avoidable");
    // The exact tier decides and reports in row size; the estimate tier in padding.
    let exact = table_report["tier"].as_str() == Some("exact");
    // Property (d): the gated headline never exceeds the engine's proven maximum saving.
    if table_report["dominance_saving"].is_object() {
        let max_saving = table_report["dominance_saving"]["max"].as_f64().expect("saving max");
        if avoidable > max_saving {
            failures.push(format!(
                "{}: headline {avoidable} exceeds the proven maximum saving {max_saving}",
                fixture.name
            ));
        }
    }
    // The alternative order comes from the tool itself: the recommendation when it gates, the
    // frontier order when it reports one.
    let names = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .expect("order")
            .iter()
            .map(|v| v.as_str().expect("name").to_string())
            .collect()
    };
    let alt_names: Option<Vec<String>> = if avoidable > 0.0 {
        Some(names(&table_report["suggested_order"]))
    } else if table_report["frontier"].is_object() {
        Some(names(&table_report["frontier"]["order"]))
    } else {
        None
    };
    let alt: Option<Vec<Col<'_>>> = alt_names.as_ref().map(|names| {
        names
            .iter()
            .map(|n| current.iter().copied().find(|(c, _, _)| c == n).expect("known column"))
            .collect()
    });
    let alt_table = format!("{}{}_alt", prefix(), fixture.name);
    let mut combined = create_table_sql(&cur_table, &current);
    if let Some(alt) = &alt {
        combined.push('\n');
        combined.push_str(&create_table_sql(&alt_table, alt));
    }
    let both = analyze(binary, &combined, &[]);
    let stats_of = |index: usize| both["analysis"]["tables"][index]["current"].clone();
    pg.query(&format!("DROP TABLE IF EXISTS {cur_table}, {alt_table};\n{combined}"))
        .expect("create tables");
    if let Some(toast) = fixture.toast_column {
        let mut alter = format!("ALTER TABLE {cur_table} ALTER COLUMN {toast} SET STORAGE EXTERNAL;");
        if alt.is_some() {
            alter.push_str(&format!(
                "\nALTER TABLE {alt_table} ALTER COLUMN {toast} SET STORAGE EXTERNAL;"
            ));
        }
        pg.query(&alter).expect("set storage");
    }
    let workloads = workloads(fixture, &null_order);
    let mut means: std::collections::BTreeMap<(String, bool), f64> = std::collections::BTreeMap::new();
    for (workload, overrides, nulls) in &workloads {
        let measured: Vec<(bool, Measured)> = match &alt {
            Some(alt_cols) => {
                let insert_cur = insert_sql(&cur_table, &current, workload, overrides, *nulls, 1200);
                let insert_alt = insert_sql(&alt_table, alt_cols, workload, overrides, *nulls, 1200);
                pg.query(&format!(
                    "TRUNCATE {cur_table}, {alt_table};\n{insert_cur}\n{insert_alt}"
                ))
                .expect("insert");
                let paired = measure_pair(pg, (&cur_table, &current), (&alt_table, alt_cols));
                if paired.stored_differently > 0 {
                    println!(
                        "| {} | pair | {workload} | - | {} row(s) stored differently by the toaster, not compared | - |",
                        fixture.name, paired.stored_differently
                    );
                }
                // Property (f): the frontier's printed query, run on the written table, replays the
                // alternative order to exactly what the alternative table measures.
                if avoidable == 0.0 && paired.stored_differently == 0 {
                    verify_query(pg, fixture, table_report, &paired, workload, failures);
                }
                // Property (b): a recommendation is pointwise, so no row may measure worse, in
                // the measure the tier decides in.
                let (worst, unit) = if exact {
                    (paired.size_saving_min, "row size")
                } else {
                    (paired.saving_min, "padding")
                };
                if avoidable > 0.0 && worst < 0 {
                    failures.push(format!(
                        "{}/{workload}: the recommended order measures worse in {unit} on some row (padding saving {}..{}, row size saving {}..{})",
                        fixture.name, paired.saving_min, paired.saving_max, paired.size_saving_min, paired.size_saving_max
                    ));
                }
                if exact {
                    check_row_sizes(
                        pg,
                        &cur_table,
                        &stats_of(0),
                        &format!("{}/{workload}", fixture.name),
                        failures,
                    );
                    check_row_sizes(
                        pg,
                        &alt_table,
                        &stats_of(1),
                        &format!("{}/{workload}/alt", fixture.name),
                        failures,
                    );
                }
                vec![(false, paired.current), (true, paired.alternative)]
            }
            None => {
                let insert = insert_sql(&cur_table, &current, workload, overrides, *nulls, 1200);
                pg.query(&format!("TRUNCATE {cur_table};\n{insert}")).expect("insert");
                if exact {
                    check_row_sizes(
                        pg,
                        &cur_table,
                        &stats_of(0),
                        &format!("{}/{workload}", fixture.name),
                        failures,
                    );
                }
                vec![(false, measure_table(pg, &cur_table, &current))]
            }
        };
        for (is_alt, measured) in measured {
            means.insert((workload.clone(), is_alt), measured.mean);
            let (model_min, model_max) = model_bounds(&stats_of(usize::from(is_alt)), *nulls != NullFill::Stored);
            println!(
                "| {} | {} | {} | [{model_min},{model_max}] | {:.3} ({}-{}) | {} |",
                fixture.name,
                if is_alt { "alt" } else { "current" },
                workload,
                measured.mean,
                measured.min,
                measured.max,
                measured.rows
            );
            // Property (a): every row inside the model's bounds.
            if measured.min < model_min || measured.max > model_max {
                failures.push(format!(
                    "{}/{}/{workload}: measured [{}-{}] outside model [{model_min},{model_max}]",
                    fixture.name,
                    if is_alt { &alt_table } else { &cur_table },
                    measured.min,
                    measured.max
                ));
            }
        }
    }
    if fixture.boundary.is_some() {
        check_boundary(fixture, &means, failures);
    }
    // Property (c), band half: every declared band holds row by row on a residue sweep inside it.
    if let (Some(alt_cols), true) = (&alt, table_report["frontier"].is_object()) {
        verify_bands(
            pg,
            fixture,
            table_report,
            (&current, alt_cols),
            (&cur_table, &alt_table),
            &null_order,
            failures,
        );
    }
    if let Some((order, workload)) = &fixture.witness {
        verify_witness(pg, fixture, table_report, &current, order, workload, failures);
    }
}

/// A fixture's workloads: short and long payloads with every column stored, the same with NULL
/// patterns when a column is nullable, and the boundary pair.
fn workloads<'a>(fixture: &Fixture, null_order: &'a [&'a str]) -> Vec<(String, Workload, NullFill<'a>)> {
    let stored = NullFill::Stored;
    let patterned = NullFill::Pattern {
        order: null_order,
        first_bit: 0,
    };
    let mut w = vec![("short".to_string(), Vec::new(), stored)];
    let long: Workload = if let Some(toast) = fixture.toast_column {
        vec![(toast, "repeat('y', 4096)".to_string())]
    } else {
        fixture
            .columns
            .iter()
            .filter(|c| is_varlena(c.sql_type))
            .map(|c| (c.name, type_spec(c.sql_type, column_seed(c.name)).1))
            .collect()
    };
    let long_name = if fixture.toast_column.is_some() {
        "toast"
    } else {
        "long"
    };
    w.push((long_name.to_string(), long.clone(), stored));
    if !null_order.is_empty() {
        w.push(("short+nulls".to_string(), Vec::new(), patterned));
        w.push((format!("{long_name}+nulls"), long, patterned));
    }
    if let Some((cur_wins, alt_wins)) = &fixture.boundary {
        w.push(("boundary-current".to_string(), cur_wins.clone(), stored));
        w.push(("boundary-alt".to_string(), alt_wins.clone(), stored));
    }
    w
}

/// The boundary pair must flip the measured winner across the frontier boundary.
fn check_boundary(
    fixture: &Fixture,
    means: &std::collections::BTreeMap<(String, bool), f64>,
    failures: &mut Vec<String>,
) {
    let cur_a = means[&("boundary-current".to_string(), false)];
    let alt_a = means[&("boundary-current".to_string(), true)];
    let cur_b = means[&("boundary-alt".to_string(), false)];
    let alt_b = means[&("boundary-alt".to_string(), true)];
    if !(cur_a < alt_a - 0.01 && alt_b < cur_b - 0.01) {
        failures.push(format!(
            "{}: boundary does not flip the winner (current {cur_a:.3} vs alt {alt_a:.3}; then current {cur_b:.3} vs alt {alt_b:.3})",
            fixture.name
        ));
    }
}

/// Run a frontier's printed query against the written table and hold its answer to the paired
/// measurement: the same rows, no replay mismatch, the same bytes saved in the frontier's
/// measure, and the same count of rows each order stores smaller.
fn verify_query(
    pg: &Pg,
    fixture: &Fixture,
    table_report: &serde_json::Value,
    paired: &Paired,
    workload: &str,
    failures: &mut Vec<String>,
) {
    let Some(sql) = table_report["frontier"]["query"]["sql"].as_str() else {
        failures.push(format!("{}: a frontier without a settling query", fixture.name));
        return;
    };
    let out = pg.query(sql).expect("settling query");
    let f: Vec<i64> = out.trim().split('|').map(|v| v.parse().expect("query count")).collect();
    let (rows, saved, mismatches) = (f[0], f[3], f[4]);
    let measured = if table_report["frontier"]["measure"].as_str() == Some("row_size") {
        paired.size_saving_sum
    } else {
        paired.saving_sum
    };
    println!(
        "| {} | query | {workload} | rows {rows}, alternative smaller {}, current smaller {}, saved {saved} | measured saved {measured} | mismatches {mismatches} |",
        fixture.name, f[1], f[2]
    );
    if rows != paired.current.rows as i64 || mismatches != 0 || saved != measured {
        failures.push(format!(
            "{}/{workload}: the settling query says {rows} rows, {saved} B saved, {mismatches} mismatches; measured {} rows, {measured} B",
            fixture.name, paired.current.rows
        ));
    }
}

/// Exact tier: rows that store every column take exactly the reported footprint, and rows
/// holding a NULL carry the reported bitmap header and stay inside the reported size range.
fn check_row_sizes(pg: &Pg, table: &str, stats: &serde_json::Value, label: &str, failures: &mut Vec<String>) {
    let sql = format!(
        "SELECT (h.t_infomask & 1) <> 0, min(h.t_hoff), max(h.t_hoff), min(((h.lp_len + 7) / 8) * 8), max(((h.lp_len + 7) / 8) * 8) \
         FROM (SELECT p, (heap_page_items(get_raw_page('{table}', p::int))).* \
               FROM generate_series(0, pg_relation_size('{table}') / 8192 - 1) p) h \
         WHERE h.lp_len > 0 GROUP BY 1 ORDER BY 1;"
    );
    let out = pg.query(&sql).expect("row sizes");
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split('|').collect();
        let has_null = f[0] == "t";
        let (hoff_min, hoff_max): (u64, u64) = (f[1].parse().expect("hoff"), f[2].parse().expect("hoff"));
        let (size_min, size_max): (u64, u64) = (f[3].parse().expect("size"), f[4].parse().expect("size"));
        let rows = if has_null { "with NULLs" } else { "without NULLs" };
        let (want_hoff, want_min, want_max) = if has_null {
            let with_nulls = &stats["with_nulls"];
            if !with_nulls.is_object() {
                failures.push(format!(
                    "{label}: rows with NULLs exist but the report has no with_nulls"
                ));
                continue;
            }
            (
                with_nulls["t_hoff"].as_u64().expect("t_hoff"),
                with_nulls["footprint_min"].as_u64().expect("min"),
                with_nulls["footprint_max"].as_u64().expect("max"),
            )
        } else {
            let fp = stats["footprint"].as_u64().expect("footprint");
            (24, fp, fp)
        };
        println!(
            "| {label} | rows {rows} | t_hoff {hoff_min}-{hoff_max} (model {want_hoff}) | [{want_min},{want_max}] | {size_min}-{size_max} B/row | - |"
        );
        if hoff_min != want_hoff || hoff_max != want_hoff || size_min < want_min || size_max > want_max {
            failures.push(format!(
                "{label}: rows {rows} measure t_hoff {hoff_min}-{hoff_max}, {size_min}-{size_max} B against model t_hoff {want_hoff}, [{want_min},{want_max}]"
            ));
        }
    }
}

/// An order the tool must not recommend: check the report, then measure the order against the
/// written one on its witness workload, where some row must come out worse.
fn verify_witness(
    pg: &Pg,
    fixture: &Fixture,
    table_report: &serde_json::Value,
    current: &[Col<'_>],
    order: &[&str],
    workload: &Workload,
    failures: &mut Vec<String>,
) {
    let suggested: Vec<&str> = table_report["suggested_order"]
        .as_array()
        .expect("order")
        .iter()
        .map(|v| v.as_str().expect("name"))
        .collect();
    if table_report["avoidable_bytes_per_row"].as_f64().expect("avoidable") > 0.0 && suggested == order {
        failures.push(format!("{}: the order {order:?} is recommended", fixture.name));
    }
    let witness: Vec<Col<'_>> = order
        .iter()
        .map(|n| current.iter().copied().find(|(c, _, _)| c == n).expect("known column"))
        .collect();
    let cur_table = format!("{}{}_wcur", prefix(), fixture.name);
    let wit_table = format!("{}{}_wit", prefix(), fixture.name);
    let insert_cur = insert_sql(&cur_table, current, "witness", workload, NullFill::Stored, 1200);
    let insert_wit = insert_sql(&wit_table, &witness, "witness", workload, NullFill::Stored, 1200);
    pg.query(&format!(
        "DROP TABLE IF EXISTS {cur_table}, {wit_table};\n{}\n{}\n{insert_cur}\n{insert_wit}",
        create_table_sql(&cur_table, current),
        create_table_sql(&wit_table, &witness)
    ))
    .expect("witness tables");
    let paired = measure_pair(pg, (&cur_table, current), (&wit_table, &witness));
    println!(
        "| {} | witness {} | - | - | saving {}..{}, cur {:.3} vs witness {:.3} | {} |",
        fixture.name,
        order.join(","),
        paired.saving_min,
        paired.saving_max,
        paired.current.mean,
        paired.alternative.mean,
        paired.current.rows
    );
    if paired.saving_min >= 0 {
        failures.push(format!(
            "{}: the witness order {order:?} measures no worse on any row of its workload",
            fixture.name
        ));
    }
}

/// Rows of a band workload: every residue combination of the first three varlenas, four times
/// over with different array contents.
const BAND_ROWS: u64 = 2048;

/// Measure each declared frontier band on a workload that keeps exactly its `long_form` columns
/// long and sweeps every varlena's payload residue (and, with nullable columns, the NULL
/// patterns the remaining bits of the row counter reach), then hold the band to its claim row by
/// row: each row's
/// saving inside the band's bounds, the declared winner's signs (alternative never worse and
/// better somewhere, current the reverse, tie all zero, mixed both signs), and, where no long
/// array leaves a residue to compression and no column is nullable, the bounds attained exactly.
/// Savings count padding, or row size when the frontier was decided in row size.
fn verify_bands(
    pg: &Pg,
    fixture: &Fixture,
    table_report: &serde_json::Value,
    (current, alt_cols): (&[Col<'_>], &[Col<'_>]),
    (cur_table, alt_table): (&str, &str),
    null_order: &[&str],
    failures: &mut Vec<String>,
) {
    let bands = table_report["frontier"]["bands"].as_array().expect("bands");
    let row_size = table_report["frontier"]["measure"].as_str() == Some("row_size");
    let varlenas: Vec<&Column> = fixture.columns.iter().filter(|c| is_varlena(c.sql_type)).collect();
    // The residue sweep takes three bits of the row counter per varlena, up to nine; NULL
    // patterns take the next ones.
    let nulls = if null_order.is_empty() {
        NullFill::Stored
    } else {
        NullFill::Pattern {
            order: null_order,
            first_bit: 3 * varlenas.len().min(3) as u32,
        }
    };
    for band in bands {
        let winner = band["winner"].as_str().expect("winner");
        let (min_saving, max_saving) = (
            band["min_saving"].as_i64().expect("min_saving"),
            band["max_saving"].as_i64().expect("max_saving"),
        );
        let long_form: Vec<&str> = band["long_form"]
            .as_array()
            .expect("long_form")
            .iter()
            .map(|v| v.as_str().expect("column name"))
            .collect();
        let mut controllable = null_order.is_empty();
        let overrides: Workload = varlenas
            .iter()
            .enumerate()
            .map(|(k, c)| {
                let selector = if k < 3 {
                    format!("((g / {}) % 8)", 8u64.pow(k as u32))
                } else {
                    format!("((g * {} + g / 8) % 8)", 2 * k + 3)
                };
                let long = long_form.contains(&c.name);
                let (generator, swept) = band_generator(c.sql_type, long, &selector, column_seed(c.name));
                controllable &= swept && !(long && c.sql_type == "float8[]");
                (c.name, generator)
            })
            .collect();
        let insert_cur = insert_sql(cur_table, current, "band", &overrides, nulls, BAND_ROWS);
        let insert_alt = insert_sql(alt_table, alt_cols, "band", &overrides, nulls, BAND_ROWS);
        pg.query(&format!(
            "TRUNCATE {cur_table}, {alt_table};\n{insert_cur}\n{insert_alt}"
        ))
        .expect("band insert");
        let paired = measure_pair(pg, (cur_table, current), (alt_table, alt_cols));
        let band_label = if long_form.is_empty() {
            "all short".to_string()
        } else {
            long_form.join("+")
        };
        let (lo, hi) = if row_size {
            (paired.size_saving_min, paired.size_saving_max)
        } else {
            (paired.saving_min, paired.saving_max)
        };
        println!(
            "| {} | band | {band_label} -> {winner} [{min_saving},{max_saving}]{} | - | saving {lo}..{hi}, cur {:.3} vs alt {:.3} | {} |",
            fixture.name,
            if row_size { " row size" } else { "" },
            paired.current.mean,
            paired.alternative.mean,
            paired.current.rows
        );
        if lo < min_saving || hi > max_saving {
            failures.push(format!(
                "{}: band [{band_label}] measures savings {lo}..{hi} outside its bounds [{min_saving},{max_saving}]",
                fixture.name
            ));
        }
        let holds = match winner {
            "alternative" => lo >= 0 && hi > 0,
            "current" => hi <= 0 && lo < 0,
            "tie" => lo == 0 && hi == 0,
            "mixed" => lo < 0 && hi > 0,
            other => {
                failures.push(format!("{}: unknown band winner {other}", fixture.name));
                true
            }
        };
        if !holds {
            failures.push(format!(
                "{}: band [{band_label}] declared {winner} but measures savings {lo}..{hi}",
                fixture.name
            ));
        }
        if controllable && (lo, hi) != (min_saving, max_saving) {
            failures.push(format!(
                "{}: band [{band_label}] bounds [{min_saving},{max_saving}] but every residue measures {lo}..{hi}",
                fixture.name
            ));
        }
    }
}

/// What nullable columns hold in a workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NullFill<'a> {
    /// A value in every row.
    Stored,
    /// NULL by a per-column bit of the row counter, numbered by the column's place in `order`
    /// (the fixture's nullable columns) so every order of a table holds the same rows.
    Pattern { order: &'a [&'a str], first_bit: u32 },
}

fn insert_sql(
    table: &str,
    columns: &[Col<'_>],
    workload: &str,
    overrides: &[(&str, String)],
    nulls: NullFill<'_>,
    rows: u64,
) -> String {
    let names: Vec<&str> = columns.iter().map(|(n, _, _)| *n).collect();
    let exprs: Vec<String> = columns
        .iter()
        .map(|(name, ty, _)| {
            let expr = if let Some((_, expr)) = overrides.iter().find(|(n, _)| n == name) {
                expr.clone()
            } else {
                let (short, long) = type_spec(ty, column_seed(name));
                if !workload.starts_with("short") && is_varlena(ty) {
                    long
                } else {
                    short
                }
            };
            let bit = match nulls {
                NullFill::Pattern { order, first_bit } => order
                    .iter()
                    .position(|n| n == name)
                    .map(|k| first_bit + (k as u32) % 10),
                NullFill::Stored => None,
            };
            match bit {
                Some(bit) => format!(
                    "CASE WHEN (g / {}) % 2 = 1 THEN NULL ELSE ({expr})::{} END",
                    1u64 << bit,
                    sql_type_name(ty)
                ),
                None => expr,
            }
        })
        .collect();
    format!(
        "INSERT INTO {table} ({}) SELECT {} FROM generate_series(1, {rows}) g ORDER BY g;",
        names.join(", "),
        exprs.join(", ")
    )
}

/// Per-row padding: the tuple length minus the header and every attribute's stored bytes, and
/// the row size (the tuple length rounded up to 8). Each row also carries its values (`key`, in
/// column-name order, numbered within duplicates, NULL spelled out) to pair it with the same row
/// of another order, and its stored sizes (`forms`, same order, -1 for NULL) to tell whether the
/// toaster stored it the same way there.
fn row_pads_sql(table: &str, columns: &[Col<'_>]) -> String {
    let mut by_name: Vec<&str> = columns.iter().map(|(n, _, _)| *n).collect();
    by_name.sort_unstable();
    let key: Vec<String> = by_name
        .iter()
        .map(|n| format!("coalesce(t.{n}::text, '\\N')"))
        .collect();
    // Attribute numbers come from the catalog, which counts dropped columns.
    let forms: Vec<String> = by_name
        .iter()
        .map(|n| {
            format!(
                "coalesce(length(h.t_attrs[(SELECT attnum FROM pg_attribute \
                 WHERE attrelid = '{table}'::regclass AND attname = '{n}')]), -1)"
            )
        })
        .collect();
    format!(
        "SELECT k.key, row_number() OVER (PARTITION BY k.key ORDER BY h.p, h.lp) AS dup, \
                ARRAY[{forms}] AS forms, \
                h.lp_len - h.t_hoff - (SELECT coalesce(sum(length(a)), 0) FROM unnest(h.t_attrs) a) AS pad, \
                ((h.lp_len + 7) / 8) * 8 AS size \
         FROM (SELECT p, (heap_page_item_attrs(get_raw_page('{table}', p::int), '{table}'::regclass)).* \
               FROM generate_series(0, pg_relation_size('{table}') / 8192 - 1) p) h \
         JOIN {table} t ON t.ctid = format('(%s,%s)', h.p, h.lp)::tid \
         CROSS JOIN LATERAL (SELECT concat_ws('|', {key}) AS key) k \
         WHERE h.lp_len > 0",
        forms = forms.join(", "),
        key = key.join(", ")
    )
}

fn measure_table(pg: &Pg, table: &str, columns: &[Col<'_>]) -> Measured {
    let sql = format!(
        "SELECT count(*), round(avg(pad)::numeric, 3), min(pad), max(pad) FROM ({}) x;",
        row_pads_sql(table, columns)
    );
    let out = pg.query(&sql).expect("measure query");
    let fields: Vec<&str> = out.trim().split('|').collect();
    Measured {
        rows: fields[0].parse().expect("rows"),
        mean: fields[1].parse().expect("mean"),
        min: fields[2].parse().expect("min"),
        max: fields[3].parse().expect("max"),
    }
}

/// Pair the rows of two orders of one table by their values. Savings count only rows the toaster
/// stored the same way in both orders: it compresses or moves out the largest attribute first
/// and breaks ties by attribute number, so a row near its size threshold or with tied sizes can
/// realize differently in two orders, and the model's claims are per realization.
fn measure_pair(pg: &Pg, cur: (&str, &[Col<'_>]), alt: (&str, &[Col<'_>])) -> Paired {
    let sql = format!(
        "WITH c AS ({}), a AS ({}), j AS (SELECT c.pad AS cp, a.pad AS ap, c.size AS cs, a.size AS asz, \
                                            c.forms = a.forms AS same \
                                     FROM c JOIN a USING (key, dup)) \
         SELECT (SELECT count(*) FROM j), (SELECT count(*) FROM c), (SELECT count(*) FROM a), \
                round(avg(cp)::numeric, 3), min(cp), max(cp), \
                round(avg(ap)::numeric, 3), min(ap), max(ap), \
                min(cp - ap) FILTER (WHERE same), max(cp - ap) FILTER (WHERE same), \
                min(cs - asz) FILTER (WHERE same), max(cs - asz) FILTER (WHERE same), \
                count(*) FILTER (WHERE NOT same), \
                coalesce(sum(cp - ap) FILTER (WHERE same), 0), coalesce(sum(cs - asz) FILTER (WHERE same), 0) \
         FROM j;",
        row_pads_sql(cur.0, cur.1),
        row_pads_sql(alt.0, alt.1)
    );
    let out = pg.query(&sql).expect("pair query");
    let f: Vec<&str> = out.trim().split('|').collect();
    let rows: u64 = f[0].parse().expect("rows");
    assert_eq!(
        (
            f[1].parse::<u64>().expect("cur rows"),
            f[2].parse::<u64>().expect("alt rows")
        ),
        (rows, rows),
        "{} and {} must pair row for row",
        cur.0,
        alt.0
    );
    let measured = |mean: &str, min: &str, max: &str| Measured {
        rows,
        mean: mean.parse().expect("mean"),
        min: min.parse().expect("min"),
        max: max.parse().expect("max"),
    };
    Paired {
        current: measured(f[3], f[4], f[5]),
        alternative: measured(f[6], f[7], f[8]),
        saving_min: f[9].parse().expect("saving min"),
        saving_max: f[10].parse().expect("saving max"),
        size_saving_min: f[11].parse().expect("size saving min"),
        size_saving_max: f[12].parse().expect("size saving max"),
        stored_differently: f[13].parse().expect("differing rows"),
        saving_sum: f[14].parse().expect("saving sum"),
        size_saving_sum: f[15].parse().expect("size saving sum"),
    }
}

/// A committed prefix, columns a later migration drops, and the block it then appends.
struct BlockFixture {
    name: &'static str,
    prefix: Vec<Column>,
    dropped: Vec<Column>,
    block: Vec<Column>,
}

fn block_fixtures() -> Vec<BlockFixture> {
    // The two false negatives the #11 reviews measured, as a committed prefix plus a block.
    let mut wide = numbered("w", 21, "bigint");
    wide.push(col("t1", "timetz"));
    let mut cliff_prefix = numbered("b", 4, "bigint");
    cliff_prefix.extend(numbered("i", 4, "int4_alias_integer"));
    let mut cliff_block = Vec::new();
    for (stem, ty) in [("tz", "timetz"), ("m", "macaddr"), ("s", "smallint"), ("f", "boolean")] {
        cliff_block.extend(numbered(stem, 4, ty));
    }
    cliff_block.push(col("note", "text"));
    vec![
        BlockFixture {
            name: "blkwide25",
            prefix: wide,
            dropped: Vec::new(),
            block: vec![col("t2", "timetz"), col("s", "smallint"), col("note", "text")],
        },
        BlockFixture {
            name: "blkcliff25",
            prefix: cliff_prefix,
            dropped: Vec::new(),
            block: cliff_block,
        },
        // A nullable prefix column leaves the block two possible start residues.
        BlockFixture {
            name: "blknull",
            prefix: vec![col("id", "bigint"), ncol("flag", "boolean")],
            dropped: Vec::new(),
            block: vec![col("s", "smallint"), col("x", "bigint"), col("i", "int4_alias_integer")],
        },
        // The stack review's drop-then-add: n is dropped, its slot stays, and (x, y, z) is the
        // block behind it.
        BlockFixture {
            name: "blkdrop",
            prefix: vec![col("id", "bigint"), col("flag", "boolean")],
            dropped: vec![col("n", "smallint")],
            block: vec![
                col("x", "smallint"),
                col("y", "int4_alias_integer"),
                col("z", "boolean"),
            ],
        },
    ]
}

/// Run the tool over migration files written in order.
fn run_files(binary: &std::path::Path, files: &[(&str, String)], extra_args: &[&str]) -> std::process::Output {
    let dir = std::env::temp_dir().join(format!("rowdiet-xtask-{}-blocks", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    for (name, sql) in files {
        std::fs::write(dir.join(name), sql).expect("write migration");
    }
    Command::new(binary)
        .arg(&dir)
        .args(extra_args)
        .output()
        .expect("run rowdiet")
}

/// The migration after the committed one: drop the dropped columns, then append `block`.
fn append_sql(table: &str, dropped: &[Col<'_>], block: &[Col<'_>]) -> String {
    let mut parts: Vec<String> = dropped
        .iter()
        .map(|(name, _, _)| format!("DROP COLUMN {name}"))
        .collect();
    parts.extend(block.iter().map(|(name, ty, nullable)| {
        let constraint = if *nullable { "" } else { " NOT NULL" };
        format!("ADD COLUMN {name} {}{constraint}", sql_type_name(ty))
    }));
    format!("ALTER TABLE {table} {};", parts.join(", "))
}

/// Commit the prefix in a baseline, append the block, and hold the tool's block order to the
/// disk: built the same way after the same prefix, the suggested block must measure no worse
/// than the written one on any row (padding, or row size at the exact tier), and every row's
/// saving must sit inside the reported range.
#[allow(clippy::too_many_lines)]
fn run_block_fixture(pg: &Pg, binary: &std::path::Path, fixture: &BlockFixture, failures: &mut Vec<String>) {
    let cols = |columns: &[Column]| -> Vec<Col<'static>> {
        columns.iter().map(|c| (c.name, c.sql_type, c.nullable)).collect()
    };
    let head = cols(&fixture.prefix);
    let dropped = cols(&fixture.dropped);
    let block = cols(&fixture.block);
    let committed_cols: Vec<Col<'_>> = head.iter().chain(&dropped).copied().collect();
    let table = format!("{}{}", prefix(), fixture.name);
    let baseline = std::env::temp_dir().join(format!("rowdiet-xtask-{}-{}.json", std::process::id(), fixture.name));
    let baseline_arg = baseline.to_str().expect("utf-8 path");
    let create = create_table_sql(&table, &committed_cols);
    let committed = run_files(
        binary,
        &[("V1__create.sql", create.clone())],
        &["--fail-over", "0", "--update-baseline", "--baseline", baseline_arg],
    );
    assert!(committed.status.success(), "baseline write failed for {}", fixture.name);
    let gated = run_files(
        binary,
        &[
            ("V1__create.sql", create),
            ("V2__append.sql", append_sql(&table, &dropped, &block)),
        ],
        &["--format", "json", "--baseline", baseline_arg],
    );
    let report: serde_json::Value = serde_json::from_slice(&gated.stdout).expect("rowdiet JSON");
    let _ = std::fs::remove_file(&baseline);
    let found = &report["gate"]["blocks"][&table];
    if !found.is_object() {
        failures.push(format!("{}: no appended-block finding in the gate", fixture.name));
        return;
    }
    let avoidable = found["avoidable_bytes_per_row"].as_f64().expect("avoidable");
    let verdict = report["gate"]["verdicts"][&table]["verdict"].as_str().unwrap_or("-");
    if avoidable <= 0.0 || verdict != "block_not_dominance_optimal" {
        failures.push(format!(
            "{}: the wasteful block passed ({avoidable} B/row, verdict {verdict})",
            fixture.name
        ));
        return;
    }
    let suggested: Vec<Col<'_>> = found["suggested_order"]
        .as_array()
        .expect("block order")
        .iter()
        .map(|v| {
            let name = v.as_str().expect("name");
            block
                .iter()
                .copied()
                .find(|(c, _, _)| *c == name)
                .expect("block column")
        })
        .collect();
    let (saving_min, saving_max) = (
        found["dominance_saving"]["min"].as_i64().expect("saving min"),
        found["dominance_saving"]["max"].as_i64().expect("saving max"),
    );
    let written_table = format!("{table}_w");
    let suggested_table = format!("{table}_s");
    let build = |name: &str, order: &[Col<'_>]| {
        format!(
            "{}\n{}",
            create_table_sql(name, &committed_cols),
            append_sql(name, &dropped, order)
        )
    };
    let ddl = format!(
        "{}\n{}",
        build(&written_table, &block),
        build(&suggested_table, &suggested)
    );
    let model = analyze(binary, &ddl, &[]);
    pg.query(&format!(
        "DROP TABLE IF EXISTS {written_table}, {suggested_table};\n{ddl}"
    ))
    .expect("apply prefix and blocks");
    let written_all: Vec<Col<'_>> = head.iter().chain(&block).copied().collect();
    let suggested_all: Vec<Col<'_>> = head.iter().chain(&suggested).copied().collect();
    let exact = model["analysis"]["tables"][0]["tier"].as_str() == Some("exact");
    let null_order: Vec<&str> = written_all.iter().filter(|(_, _, n)| *n).map(|(n, _, _)| *n).collect();
    let long: Workload = written_all
        .iter()
        .filter(|(_, ty, _)| is_varlena(ty))
        .map(|(name, ty, _)| (*name, type_spec(ty, column_seed(name)).1))
        .collect();
    let stored = NullFill::Stored;
    let mut workloads: Vec<(&str, Workload, NullFill<'_>)> =
        vec![("short", Vec::new(), stored), ("long", long.clone(), stored)];
    if !null_order.is_empty() {
        let patterned = NullFill::Pattern {
            order: &null_order,
            first_bit: 0,
        };
        workloads.push(("short+nulls", Vec::new(), patterned));
        workloads.push(("long+nulls", long, patterned));
    }
    for (workload, overrides, nulls) in &workloads {
        let insert_w = insert_sql(&written_table, &written_all, workload, overrides, *nulls, 1200);
        let insert_s = insert_sql(&suggested_table, &suggested_all, workload, overrides, *nulls, 1200);
        pg.query(&format!(
            "TRUNCATE {written_table}, {suggested_table};\n{insert_w}\n{insert_s}"
        ))
        .expect("block insert");
        let paired = measure_pair(pg, (&written_table, &written_all), (&suggested_table, &suggested_all));
        for (index, measured) in [paired.current, paired.alternative].into_iter().enumerate() {
            let (lo, hi) = model_bounds(
                &model["analysis"]["tables"][index]["current"],
                *nulls != NullFill::Stored,
            );
            println!(
                "| {} | {} | {workload} | [{lo},{hi}] | {:.3} ({}-{}) | {} |",
                fixture.name,
                if index == 0 { "written block" } else { "suggested block" },
                measured.mean,
                measured.min,
                measured.max,
                measured.rows
            );
            if measured.min < lo || measured.max > hi {
                failures.push(format!(
                    "{}/{workload}: measured [{}-{}] outside model [{lo},{hi}]",
                    fixture.name, measured.min, measured.max
                ));
            }
        }
        let (lo, hi, unit) = if exact {
            (paired.size_saving_min, paired.size_saving_max, "row size")
        } else {
            (paired.saving_min, paired.saving_max, "padding")
        };
        println!(
            "| {} | block saving | {workload} | [{saving_min},{saving_max}] {unit} | {lo}..{hi} per row | - |",
            fixture.name
        );
        if lo < 0 {
            failures.push(format!(
                "{}/{workload}: the suggested block measures worse in {unit} on some row ({lo}..{hi})",
                fixture.name
            ));
        }
        if lo < saving_min || hi > saving_max {
            failures.push(format!(
                "{}/{workload}: per-row {unit} savings {lo}..{hi} outside the reported [{saving_min},{saving_max}]",
                fixture.name
            ));
        }
    }
}

/// `STORAGE PLAIN` under both parsers. pg-exact reads it and must not prove a PLAIN varchar(5)
/// short; sqlparser cannot parse it and must make no claim. The rows are loaded with COPY and
/// with an UPDATE, the two writes that store the 4-byte header under PLAIN, and every row must
/// sit inside pg-exact's bounds, while `(s, c)`, which the typmod alone would prove never worse,
/// measures worse.
fn run_plain_checks(pg: &Pg, binary: &std::path::Path, failures: &mut Vec<String>) {
    let table = format!("{}plain", prefix());
    let alt = format!("{table}_alt");
    let ddl = format!("CREATE TABLE {table} (c varchar(5) STORAGE PLAIN, s smallint);");
    let exact = analyze(binary, &ddl, &["--parser", "pg-exact"]);
    let t = &exact["analysis"]["tables"][0];
    let suggested: Vec<&str> = t["suggested_order"]
        .as_array()
        .expect("order")
        .iter()
        .map(|v| v.as_str().expect("name"))
        .collect();
    let avoidable = t["avoidable_bytes_per_row"].as_f64().expect("avoidable");
    let (lo, hi) = model_bounds(&t["current"], false);
    println!(
        "| plain (pg-exact) | report | - | [{lo},{hi}] | avoidable {avoidable}, order {} | - |",
        suggested.join(",")
    );
    if avoidable > 0.0 && suggested == ["s", "c"] {
        failures.push("plain: pg-exact recommends (s, c) for a PLAIN varchar(5)".into());
    }
    let sqlparser = analyze(binary, &ddl, &[]);
    let tables = sqlparser["analysis"]["tables"].as_array().map_or(0, Vec::len);
    if tables != 0 {
        failures.push(format!("plain: sqlparser analyzed {tables} table(s) it cannot parse"));
    }
    let cur: Vec<Col<'_>> = vec![("c", "varchar(5)", true), ("s", "smallint", true)];
    let swapped: Vec<Col<'_>> = vec![("s", "smallint", true), ("c", "varchar(5)", true)];
    let rows: String = (1..=1200)
        .map(|g| format!("{}\t{}\n", "x".repeat(g % 6), g % 7))
        .collect();
    pg.query(&format!(
        "DROP TABLE IF EXISTS {table}, {alt};\n{ddl}\nCREATE TABLE {alt} (s smallint, c varchar(5) STORAGE PLAIN);"
    ))
    .expect("plain tables");
    let loads = [
        (
            "copy",
            format!(
                "TRUNCATE {table}, {alt};\nCOPY {table} (c, s) FROM STDIN;\n{rows}\\.\nCOPY {alt} (c, s) FROM STDIN;\n{rows}\\.\n"
            ),
        ),
        (
            "update",
            format!(
                "TRUNCATE {table}, {alt};\nINSERT INTO {table} SELECT repeat('x', g % 6), g % 7 FROM generate_series(1, 1200) g;\n\
                 INSERT INTO {alt} SELECT s, c FROM {table};\nUPDATE {table} SET c = c || '';\nUPDATE {alt} SET c = c || '';"
            ),
        ),
    ];
    for (load, sql) in loads {
        pg.query(&sql).expect("plain load");
        let paired = measure_pair(pg, (&table, &cur), (&alt, &swapped));
        println!(
            "| plain | {load} | - | [{lo},{hi}] | written {:.3} ({}-{}), (s, c) {:.3}, saving {}..{} | {} |",
            paired.current.mean,
            paired.current.min,
            paired.current.max,
            paired.alternative.mean,
            paired.saving_min,
            paired.saving_max,
            paired.current.rows
        );
        if paired.current.min < lo || paired.current.max > hi {
            failures.push(format!(
                "plain/{load}: measured [{}-{}] outside pg-exact's bounds [{lo},{hi}]",
                paired.current.min, paired.current.max
            ));
        }
        if paired.saving_min >= 0 {
            failures.push(format!(
                "plain/{load}: (s, c) measures no worse on any row, so the PLAIN rows did not keep the 4-byte header"
            ));
        }
    }
}

/// The many-class false negative needs assume-typed columns that no live Postgres has, so it is
/// checked against the report alone: the capped search must still gate the fixed-prefix win and
/// must label its scope.
fn run_report_only_checks(binary: &std::path::Path, failures: &mut Vec<String>) {
    // Live PostgreSQL cannot realize this shape: pg_type holds exactly 9 distinct fixed
    // (typalign, typlen mod 8) classes among builtin base types, and reaching 10+ needs
    // assume-typed widths no real type has. The report is the only checkable surface.
    let types = [
        "boolean", "smallint", "integer", "bigint", "timetz", "macaddr", "w3c", "w5c", "w3s", "w5i",
    ];
    let mut cols = Vec::new();
    for (i, ty) in types.iter().enumerate() {
        cols.push(format!("c{i}a {ty} NOT NULL"));
        cols.push(format!("c{i}b {ty} NOT NULL"));
    }
    cols.push("note text NOT NULL".into());
    let sql = format!("CREATE TABLE {}cls ({});", prefix(), cols.join(", "));
    let value = analyze(
        binary,
        &sql,
        &[
            "--assume-type",
            "w3c=fixed:3:c",
            "--assume-type",
            "w5c=fixed:5:c",
            "--assume-type",
            "w3s=fixed:3:s",
            "--assume-type",
            "w5i=fixed:5:i",
        ],
    );
    let t = &value["analysis"]["tables"][0];
    let avoidable = t["avoidable_bytes_per_row"].as_f64().expect("avoidable");
    let scope = t["search_scope"].as_str().expect("scope");
    println!("| cls (report only) | current | - | avoidable {avoidable}, scope {scope} | - | - |");
    if avoidable <= 0.0 {
        failures.push("cls: capped search printed a clean verdict over deterministic waste".into());
    }
    if scope != "fixed_prefix" {
        failures.push(format!("cls: expected a capped-scope label, got {scope}"));
    }
}
