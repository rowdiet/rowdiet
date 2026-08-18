//! Repository tasks. `cargo run -p xtask -- measure` regression-tests the layout model's
//! claims against real tuples: it builds the release binary, applies DDL fixtures to a
//! disposable PostgreSQL (Docker, pageinspect), inserts short-heavy and long-heavy workloads,
//! and asserts the three properties from the decision-policy spec:
//!
//! (a) measured per-row padding falls inside every reported [min, max];
//! (b) whenever the tool recommends a reorder, the suggested order measures no worse than the
//!     current order on both workloads;
//! (c) every declared frontier boundary flips the measured winner when the workload crosses it.
//!
//! Skipped (exit 0, loud) when no reachable container: the harness is for maintainers and
//! Docker-equipped CI legs, and plain CI must not fail for lacking a database. Container/user/db
//! come from `ROWDIET_MEASURE_CONTAINER` / `_USER` / `_DB` (defaults: condescending_tu, test,
//! test). Every table it creates is prefixed `synb_` and dropped afterwards.

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

struct Column {
    name: &'static str,
    sql_type: &'static str,
}

/// Per-type SQL: (size expression, short-heavy generator, long-heavy generator). `g` is the
/// row counter, so widths sweep the payload residues deterministically.
fn type_spec(sql_type: &str, name: &str) -> (String, String, String) {
    let col = format!("t.{name}");
    match sql_type {
        "text" => (
            format!("pg_column_size({col})"),
            "repeat('x', (g % 16)::int)".to_string(),
            "repeat('x', (130 + (g * 7) % 110)::int)".to_string(),
        ),
        "float8[]" => (
            format!("pg_column_size({col})"),
            "(SELECT array_agg(random()) FROM generate_series(1, (1 + g % 2)::int))".to_string(),
            "(SELECT array_agg(random()) FROM generate_series(1, (25 + g % 16)::int))".to_string(),
        ),
        "bigint" => (8.to_string(), "42".into(), "42".into()),
        "int4_alias_integer" => (4.to_string(), "7".into(), "7".into()),
        "integer" => (4.to_string(), "7".into(), "7".into()),
        "smallint" => (2.to_string(), "1".into(), "1".into()),
        "boolean" => (1.to_string(), "true".into(), "true".into()),
        "float8" => (8.to_string(), "1.5".into(), "1.5".into()),
        "timestamp" => (8.to_string(), "now()::timestamp".into(), "now()::timestamp".into()),
        "timetz" => (12.to_string(), "now()::timetz".into(), "now()::timetz".into()),
        "macaddr" => (
            6.to_string(),
            "'08:00:2b:01:02:03'".into(),
            "'08:00:2b:01:02:03'".into(),
        ),
        other => panic!("no type spec for {other}"),
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
                    (
                        "arr",
                        "(SELECT array_agg(random()) FROM generate_series(1, (1 + g % 2)::int))".into(),
                    ),
                ],
                vec![
                    ("txt", "repeat('x', (g % 16)::int)".into()),
                    (
                        "arr",
                        "(SELECT array_agg(random()) FROM generate_series(1, (25 + g % 16)::int))".into(),
                    ),
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
                vec![(
                    "arr",
                    "(SELECT array_agg(random()) FROM generate_series(1, (1 + g % 2)::int))".into(),
                )],
                vec![(
                    "arr",
                    "(SELECT array_agg(random()) FROM generate_series(1, (25 + g % 16)::int))".into(),
                )],
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
        // TOAST: 4 kB EXTERNAL payloads store an 18-byte unaligned pointer; the pinned residue
        // sits inside the reported range and the dominating reorder must still measure better.
        Fixture {
            name: "toastish",
            columns: vec![col("t", "text"), col("x", "float8")],
            boundary: None,
            toast_column: Some("t"),
        },
    ];
    // The 25-column cap cliff: deterministic 24 B/row, must gate and must measure.
    let mut cliff = Vec::new();
    for i in 0..4u32 {
        cliff.push(Column {
            name: Box::leak(format!("tz{i}").into_boxed_str()),
            sql_type: "timetz",
        });
    }
    for i in 0..4u32 {
        cliff.push(Column {
            name: Box::leak(format!("m{i}").into_boxed_str()),
            sql_type: "macaddr",
        });
    }
    for i in 0..4u32 {
        cliff.push(Column {
            name: Box::leak(format!("b{i}").into_boxed_str()),
            sql_type: "bigint",
        });
    }
    for i in 0..4u32 {
        cliff.push(Column {
            name: Box::leak(format!("i{i}").into_boxed_str()),
            sql_type: "int4_alias_integer",
        });
    }
    for i in 0..4u32 {
        cliff.push(Column {
            name: Box::leak(format!("s{i}").into_boxed_str()),
            sql_type: "smallint",
        });
    }
    for i in 0..4u32 {
        cliff.push(Column {
            name: Box::leak(format!("f{i}").into_boxed_str()),
            sql_type: "boolean",
        });
    }
    cliff.push(col("note", "text"));
    out.push(Fixture {
        name: "cliff25",
        columns: cliff,
        boundary: None,
        toast_column: None,
    });
    // The wide-table false negative: 21 bigints push the whole-order search over budget; the
    // fixed-prefix repack must still be found, gated, and measured better.
    let mut wide = Vec::new();
    for i in 0..21u32 {
        wide.push(Column {
            name: Box::leak(format!("w{i}").into_boxed_str()),
            sql_type: "bigint",
        });
    }
    wide.push(col("t1", "timetz"));
    wide.push(col("t2", "timetz"));
    wide.push(col("s", "smallint"));
    wide.push(col("note", "text"));
    out.push(Fixture {
        name: "wide25",
        columns: wide,
        boundary: None,
        toast_column: None,
    });
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
    run_report_only_checks(&binary, &mut failures);
    pg.query("DO $$ DECLARE r record; BEGIN FOR r IN SELECT tablename FROM pg_tables WHERE tablename LIKE 'synb\\_%' LOOP EXECUTE 'DROP TABLE ' || quote_ident(r.tablename); END LOOP; END $$;")
        .expect("cleanup");
    let remaining = pg
        .query("SELECT count(*) FROM pg_tables WHERE tablename LIKE 'synb\\_%';")
        .expect("count");
    assert_eq!(remaining.trim(), "0", "synb_ tables must all be dropped");
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
    let cur_table = format!("synb_{}", fixture.name);
    let report = analyze(binary, &create_table_sql(&cur_table, &current), &[]);
    let table_report = &report["analysis"]["tables"][0];
    let avoidable = table_report["avoidable_bytes_per_row"].as_f64().expect("avoidable");
    // The alternative order comes from the tool itself: the recommendation when it gates, the
    // frontier order when it reports one.
    let alt_names: Option<Vec<String>> = if avoidable > 0.0 {
        Some(
            table_report["suggested_order"]
                .as_array()
                .expect("order")
                .iter()
                .map(|v| v.as_str().expect("name").to_string())
                .collect(),
        )
    } else if table_report["frontier"].is_object() {
        Some(
            table_report["frontier"]["order"]
                .as_array()
                .expect("frontier order")
                .iter()
                .map(|v| v.as_str().expect("name").to_string())
                .collect(),
        )
    } else {
        None
    };
    let alt: Option<Vec<(&str, &str)>> = alt_names.as_ref().map(|names| {
        names
            .iter()
            .map(|n| current.iter().copied().find(|(c, _)| c == n).expect("known column"))
            .collect()
    });
    let alt_table = format!("synb_{}_alt", fixture.name);
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
    let workloads: Vec<(String, Vec<(&str, String)>)> = {
        let mut w = vec![("short".to_string(), Vec::new())];
        if let Some(toast) = fixture.toast_column {
            w.push(("toast".to_string(), vec![(toast, "repeat('y', 4096)".to_string())]));
        } else {
            w.push((
                "long".to_string(),
                fixture
                    .columns
                    .iter()
                    .filter(|c| matches!(c.sql_type, "text" | "float8[]"))
                    .map(|c| (c.name, type_spec(c.sql_type, c.name).2))
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
        for (is_alt, table, columns) in [(false, &cur_table, Some(&current)), (true, &alt_table, alt.as_ref())] {
            let Some(columns) = columns else { continue };
            let insert = insert_sql(table, columns, workload, overrides, fixture.toast_column);
            pg.query(&format!("TRUNCATE {table};\n{insert}")).expect("insert");
            let toast_now = fixture.toast_column.filter(|_| workload == "toast");
            let measured = measure_table(pg, table, columns, toast_now);
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
            if measured.min < model_min || measured.max > model_max {
                failures.push(format!(
                    "{}/{table}/{workload}: measured [{}-{}] outside model [{model_min},{model_max}]",
                    fixture.name, measured.min, measured.max
                ));
            }
        }
    }
    if avoidable > 0.0 {
        for workload in ["short", "long", "toast"] {
            let (Some(cur), Some(alt_mean)) = (
                means.get(&(workload.to_string(), false)),
                means.get(&(workload.to_string(), true)),
            ) else {
                continue;
            };
            if *alt_mean > cur + 0.05 {
                failures.push(format!(
                    "{}/{workload}: recommended order measures worse ({alt_mean:.3} vs {cur:.3})",
                    fixture.name
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
}

fn insert_sql(
    table: &str,
    columns: &[(&str, &str)],
    workload: &str,
    overrides: &[(&str, String)],
    toast_column: Option<&str>,
) -> String {
    let names: Vec<&str> = columns.iter().map(|(n, _)| *n).collect();
    let exprs: Vec<String> = columns
        .iter()
        .map(|(name, ty)| {
            if let Some((_, expr)) = overrides.iter().find(|(n, _)| n == name) {
                return expr.clone();
            }
            let (_, short, long) = type_spec(ty, name);
            let long_pick = workload != "short" && toast_column != Some(*name);
            if long_pick && matches!(*ty, "text" | "float8[]") {
                long
            } else {
                short
            }
        })
        .collect();
    format!(
        "INSERT INTO {table} ({}) SELECT {} FROM generate_series(1, 1200) g;",
        names.join(", "),
        exprs.join(", ")
    )
}

fn measure_table(pg: &Pg, table: &str, columns: &[(&str, &str)], toast_column: Option<&str>) -> Measured {
    let sizes: Vec<String> = columns
        .iter()
        .map(|(name, ty)| {
            if toast_column == Some(*name) {
                "18".to_string()
            } else {
                type_spec(ty, name).0
            }
        })
        .collect();
    let sql = format!(
        "SELECT count(*), round(avg(pad)::numeric, 3), min(pad), max(pad) FROM (
           SELECT h.lp_len - h.t_hoff - ({sizes}) AS pad
           FROM (SELECT p, (heap_page_items(get_raw_page('{table}', p::int))).*
                 FROM generate_series(0, pg_relation_size('{table}')/8192 - 1) p) h
           JOIN {table} t ON t.ctid = format('(%s,%s)', h.p, h.lp)::tid
         ) x;",
        sizes = sizes.join(" + "),
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

/// The many-class false negative needs assume-typed columns that no live Postgres has, so it is
/// checked against the report alone: the capped search must still gate the fixed-prefix win and
/// must label its scope.
fn run_report_only_checks(binary: &std::path::Path, failures: &mut Vec<String>) {
    let types = [
        "boolean", "smallint", "integer", "bigint", "timetz", "macaddr", "w3c", "w5c", "w3s", "w5i",
    ];
    let mut cols = Vec::new();
    for (i, ty) in types.iter().enumerate() {
        cols.push(format!("c{i}a {ty} NOT NULL"));
        cols.push(format!("c{i}b {ty} NOT NULL"));
    }
    cols.push("note text NOT NULL".into());
    let sql = format!("CREATE TABLE synb_cls ({});", cols.join(", "));
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
