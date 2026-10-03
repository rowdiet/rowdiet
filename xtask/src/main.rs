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
//! (d) the gated headline never exceeds the dominance engine's proven maximum saving.
//!
//! Rows are generated deterministically, so the current and the alternative table hold the same
//! values row for row and pair by insertion order. Padding is the tuple length minus the header
//! and the stored bytes of every attribute (`heap_page_item_attrs`), which counts short headers,
//! compressed values and TOAST pointers as they sit on the page. Array workloads include
//! uncompressed short and long arrays, compressed inline arrays, and TOAST pointers.
//!
//! Skipped (exit 0, loud) when no reachable container, unless `ROWDIET_MEASURE_REQUIRE=1` (the CI
//! job sets it, so a missing database fails there). Container/user/db come from
//! `ROWDIET_MEASURE_CONTAINER` / `_USER` / `_DB` (defaults: condescending_tu, test, test). Every
//! table it creates carries the `ROWDIET_MEASURE_PREFIX` prefix (default `synb_`) and is dropped
//! afterwards, so concurrent users of one database keep apart. The measured binary is the one
//! cargo reports for this build, wherever `CARGO_TARGET_DIR` puts it, and the run stops if it is
//! older than any source file.

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

/// Two tables holding the same rows, paired by insertion order. `saving` is current minus
/// alternative padding per row.
#[derive(Debug, Clone, Copy)]
struct Paired {
    current: Measured,
    alternative: Measured,
    saving_min: i64,
    saving_max: i64,
    /// Rows the toaster stored differently in the two orders, left out of the savings.
    stored_differently: u64,
}

struct Column {
    name: &'static str,
    sql_type: &'static str,
}

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
}

fn col(name: &'static str, sql_type: &'static str) -> Column {
    Column { name, sql_type }
}

fn numbered(prefix: &str, count: u32, sql_type: &'static str) -> Vec<Column> {
    (0..count)
        .map(|i| Column {
            name: Box::leak(format!("{prefix}{i}").into_boxed_str()),
            sql_type,
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn fixtures() -> Vec<Fixture> {
    let plain = |name, columns| Fixture {
        name,
        columns,
        boundary: None,
        toast_column: None,
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
        Err(e) if std::env::var("ROWDIET_MEASURE_REQUIRE").is_ok_and(|v| v == "1") => {
            eprintln!("measure: no reachable docker Postgres and ROWDIET_MEASURE_REQUIRE=1 ({e})");
            std::process::exit(1);
        }
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

/// Build the release binary and return the path cargo reports for it, so `CARGO_TARGET_DIR` and
/// build config cannot point the run at another checkout's binary. Stops when that binary is
/// older than a source file.
fn build_binary() -> std::path::PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(&cargo)
        .args(["build", "--release", "-p", "rowdiet", "--message-format=json"])
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo build");
    assert!(out.status.success(), "release build failed");
    let binary = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg["reason"] == "compiler-artifact" && msg["target"]["name"] == "rowdiet")
        .find_map(|msg| msg["executable"].as_str().map(std::path::PathBuf::from))
        .expect("cargo reported no rowdiet executable");
    let built = std::fs::metadata(&binary)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    // Cargo's dep-info names every source the binary was built from.
    let dep_info = binary.with_extension("d");
    let deps = std::fs::read_to_string(&dep_info).unwrap_or_else(|e| panic!("{}: {e}", dep_info.display()));
    let sources = deps
        .split_once(": ")
        .map_or("", |(_, rest)| rest)
        .replace("\\ ", "\u{0}");
    for source in sources.split_whitespace().map(|s| s.replace('\u{0}', " ")) {
        let modified = std::fs::metadata(&source).and_then(|m| m.modified());
        if modified.is_ok_and(|time| time > built) {
            panic!(
                "{} is older than {source}: the measured binary would not be this checkout's",
                binary.display()
            );
        }
    }
    binary
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

fn create_table_sql(table: &str, columns: &[(&str, &str)]) -> String {
    let cols: Vec<String> = columns
        .iter()
        .map(|(name, ty)| format!("{name} {} NOT NULL", sql_type_name(ty)))
        .collect();
    format!("CREATE TABLE {table} ({});", cols.join(", "))
}

#[allow(clippy::too_many_lines)]
fn run_fixture(pg: &Pg, binary: &std::path::Path, fixture: &Fixture, failures: &mut Vec<String>) {
    let current: Vec<(&str, &str)> = fixture.columns.iter().map(|c| (c.name, c.sql_type)).collect();
    let cur_table = format!("{}{}", prefix(), fixture.name);
    let report = analyze(binary, &create_table_sql(&cur_table, &current), &[]);
    let table_report = &report["analysis"]["tables"][0];
    let avoidable = table_report["avoidable_bytes_per_row"].as_f64().expect("avoidable");
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
    let alt: Option<Vec<(&str, &str)>> = alt_names.as_ref().map(|names| {
        names
            .iter()
            .map(|n| current.iter().copied().find(|(c, _)| c == n).expect("known column"))
            .collect()
    });
    let alt_table = format!("{}{}_alt", prefix(), fixture.name);
    let mut combined = create_table_sql(&cur_table, &current);
    if let Some(alt) = &alt {
        combined.push('\n');
        combined.push_str(&create_table_sql(&alt_table, alt));
    }
    let both = analyze(binary, &combined, &[]);
    let model_bounds = |index: usize| {
        let t = &both["analysis"]["tables"][index];
        (
            t["current"]["padding_min"].as_i64().expect("min"),
            t["current"]["padding_max"].as_i64().expect("max"),
        )
    };
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
    let workloads: Vec<(String, Workload)> = {
        let mut w = vec![("short".to_string(), Vec::new())];
        if let Some(toast) = fixture.toast_column {
            w.push(("toast".to_string(), vec![(toast, "repeat('y', 4096)".to_string())]));
        } else {
            w.push((
                "long".to_string(),
                fixture
                    .columns
                    .iter()
                    .filter(|c| is_varlena(c.sql_type))
                    .map(|c| (c.name, type_spec(c.sql_type, column_seed(c.name)).1))
                    .collect(),
            ));
        }
        if let Some((cur_wins, alt_wins)) = &fixture.boundary {
            w.push(("boundary-current".to_string(), cur_wins.clone()));
            w.push(("boundary-alt".to_string(), alt_wins.clone()));
        }
        w
    };
    let mut means: std::collections::BTreeMap<(String, bool), f64> = std::collections::BTreeMap::new();
    for (workload, overrides) in &workloads {
        let measured: Vec<(bool, Measured)> = match &alt {
            Some(alt_cols) => {
                let insert_cur = insert_sql(&cur_table, &current, workload, overrides, 1200);
                let insert_alt = insert_sql(&alt_table, alt_cols, workload, overrides, 1200);
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
                // Property (b): a recommendation is pointwise, so no row may measure worse.
                if avoidable > 0.0 && paired.saving_min < 0 {
                    failures.push(format!(
                        "{}/{workload}: the recommended order measures worse on some row (saving {}..{}, means {:.3} vs {:.3})",
                        fixture.name, paired.saving_min, paired.saving_max, paired.alternative.mean, paired.current.mean
                    ));
                }
                vec![(false, paired.current), (true, paired.alternative)]
            }
            None => {
                let insert = insert_sql(&cur_table, &current, workload, overrides, 1200);
                pg.query(&format!("TRUNCATE {cur_table};\n{insert}")).expect("insert");
                vec![(false, measure_table(pg, &cur_table, &current))]
            }
        };
        for (is_alt, measured) in measured {
            means.insert((workload.clone(), is_alt), measured.mean);
            let (model_min, model_max) = model_bounds(usize::from(is_alt));
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
    // Property (c), band half: every declared band holds row by row on a residue sweep inside it.
    if let (Some(alt_cols), true) = (&alt, table_report["frontier"].is_object()) {
        verify_bands(
            pg,
            fixture,
            table_report,
            &current,
            alt_cols,
            &cur_table,
            &alt_table,
            failures,
        );
    }
}

/// Rows of a band workload: every residue combination of the first three varlenas, four times
/// over with different array contents.
const BAND_ROWS: u64 = 2048;

/// Measure each declared frontier band on a workload that keeps exactly its `long_form` columns
/// long and sweeps every varlena's payload residue, then hold the band to its claim row by row:
/// each row's saving inside the band's bounds, the declared winner's signs (alternative never
/// worse and better somewhere, current the reverse, tie all zero, mixed both signs), and, where
/// no long array leaves a residue to compression, the bounds attained exactly.
#[allow(clippy::too_many_arguments)]
fn verify_bands(
    pg: &Pg,
    fixture: &Fixture,
    table_report: &serde_json::Value,
    current: &[(&str, &str)],
    alt_cols: &[(&str, &str)],
    cur_table: &str,
    alt_table: &str,
    failures: &mut Vec<String>,
) {
    let bands = table_report["frontier"]["bands"].as_array().expect("bands");
    let varlenas: Vec<&Column> = fixture.columns.iter().filter(|c| is_varlena(c.sql_type)).collect();
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
        let mut controllable = true;
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
        let insert_cur = insert_sql(cur_table, current, "band", &overrides, BAND_ROWS);
        let insert_alt = insert_sql(alt_table, alt_cols, "band", &overrides, BAND_ROWS);
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
        println!(
            "| {} | band | {band_label} -> {winner} [{min_saving},{max_saving}] | - | saving {}..{}, cur {:.3} vs alt {:.3} | {} |",
            fixture.name,
            paired.saving_min,
            paired.saving_max,
            paired.current.mean,
            paired.alternative.mean,
            paired.current.rows
        );
        let (lo, hi) = (paired.saving_min, paired.saving_max);
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

fn insert_sql(
    table: &str,
    columns: &[(&str, &str)],
    workload: &str,
    overrides: &[(&str, String)],
    rows: u64,
) -> String {
    let names: Vec<&str> = columns.iter().map(|(n, _)| *n).collect();
    let exprs: Vec<String> = columns
        .iter()
        .map(|(name, ty)| {
            if let Some((_, expr)) = overrides.iter().find(|(n, _)| n == name) {
                return expr.clone();
            }
            let (short, long) = type_spec(ty, column_seed(name));
            if workload != "short" && is_varlena(ty) {
                long
            } else {
                short
            }
        })
        .collect();
    format!(
        "INSERT INTO {table} ({}) SELECT {} FROM generate_series(1, {rows}) g ORDER BY g;",
        names.join(", "),
        exprs.join(", ")
    )
}

/// Per-row padding: the tuple length minus the header and every attribute's stored bytes. Each
/// row also carries its values (`key`, in column-name order, numbered within duplicates) to pair
/// it with the same row of another order, and its stored sizes (`forms`, same order) to tell
/// whether the toaster stored it the same way there.
fn row_pads_sql(table: &str, columns: &[(&str, &str)]) -> String {
    let mut by_name: Vec<(usize, &str)> = columns.iter().enumerate().map(|(i, (n, _))| (i, *n)).collect();
    by_name.sort_by_key(|(_, n)| *n);
    let key: Vec<String> = by_name.iter().map(|(_, n)| format!("t.{n}::text")).collect();
    let forms: Vec<String> = by_name
        .iter()
        .map(|(i, _)| format!("length(h.t_attrs[{}])", i + 1))
        .collect();
    format!(
        "SELECT k.key, row_number() OVER (PARTITION BY k.key ORDER BY h.p, h.lp) AS dup, \
                ARRAY[{forms}] AS forms, \
                h.lp_len - h.t_hoff - (SELECT sum(length(a)) FROM unnest(h.t_attrs) a) AS pad \
         FROM (SELECT p, (heap_page_item_attrs(get_raw_page('{table}', p::int), '{table}'::regclass)).* \
               FROM generate_series(0, pg_relation_size('{table}') / 8192 - 1) p) h \
         JOIN {table} t ON t.ctid = format('(%s,%s)', h.p, h.lp)::tid \
         CROSS JOIN LATERAL (SELECT concat_ws('|', {key}) AS key) k \
         WHERE h.lp_len > 0",
        forms = forms.join(", "),
        key = key.join(", ")
    )
}

fn measure_table(pg: &Pg, table: &str, columns: &[(&str, &str)]) -> Measured {
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
fn measure_pair(pg: &Pg, cur: (&str, &[(&str, &str)]), alt: (&str, &[(&str, &str)])) -> Paired {
    let sql = format!(
        "WITH c AS ({}), a AS ({}), j AS (SELECT c.pad AS cp, a.pad AS ap, c.forms = a.forms AS same \
                                     FROM c JOIN a USING (key, dup)) \
         SELECT (SELECT count(*) FROM j), (SELECT count(*) FROM c), (SELECT count(*) FROM a), \
                round(avg(cp)::numeric, 3), min(cp), max(cp), \
                round(avg(ap)::numeric, 3), min(ap), max(ap), \
                min(cp - ap) FILTER (WHERE same), max(cp - ap) FILTER (WHERE same), \
                count(*) FILTER (WHERE NOT same) \
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
        stored_differently: f[11].parse().expect("differing rows"),
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
