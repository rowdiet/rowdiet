//! End-to-end tests of the compiled `rowdiet` binary against the fixture migration sets:
//! exit codes, output formats, gating, and baseline maintenance.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rowdiet"))
}

fn fixtures(sub: &str) -> String {
    format!("{}/tests/fixtures/{sub}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn text_output_and_gate_exit_code() {
    let out = bin().arg(fixtures("wasteful")).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("account"));
    assert!(stdout.contains("B/row avoidable"));
    let gated = bin()
        .arg(fixtures("wasteful"))
        .args(["--fail-over", "0"])
        .output()
        .unwrap();
    assert_eq!(gated.status.code(), Some(1));
}

#[test]
fn optimal_passes_gate() {
    let out = bin()
        .arg(fixtures("optimal"))
        .args(["--fail-over", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn json_format() {
    let out = bin()
        .arg(fixtures("wasteful"))
        .args(["--format", "json"])
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!value["analysis"]["tables"].as_array().unwrap().is_empty());
}

#[test]
fn github_format() {
    let out = bin()
        .arg(fixtures("wasteful"))
        .args(["--format", "github"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("::warning file="));
}

#[test]
fn stdin_source() {
    let mut child = bin()
        .arg("-")
        .args(["--fail-over", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"CREATE TABLE t (a int, b bigint, c int, d bigint);")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn version_order_folds_alters_after_create() {
    let out = bin()
        .arg(fixtures("wasteful"))
        .args(["--format", "json"])
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["analysis"]["tables"][0]["natts"], 6);
    assert_eq!(value["analysis"]["tables"][0]["tier"], "estimate");
}

#[test]
fn interleaved_varlenas_report_dominance_avoidable_waste() {
    // The issue-1 repro: same column multiset, opposite orders. Grouping dominates
    // interleaving (never worse in any storage-form/payload realization), so the reorder is a
    // gated finding; the control table passes clean with the expectation as display only.
    let out = bin()
        .arg(fixtures("varlena"))
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tables = value["analysis"]["tables"].as_array().unwrap();
    let interleaved = &tables[0];
    assert_eq!(interleaved["name"], "interleaved");
    assert_eq!(interleaved["current"]["padding"], 0);
    assert_eq!(interleaved["current"]["expected_padding"], 5.0);
    assert_eq!(interleaved["current"]["padding_min"], 0);
    assert_eq!(interleaved["current"]["padding_max"], 19);
    assert_eq!(interleaved["suggested"]["padding_max"], 12);
    assert_eq!(interleaved["avoidable_bytes_per_row"], 11.0);
    assert_eq!(interleaved["avoidable_deterministic"], 0);
    assert_eq!(interleaved["avoidable_dominance"], 11);
    assert_eq!(interleaved["dominance_saving"]["min"], 0);
    assert_eq!(interleaved["dominance_saving"]["max"], 11);
    assert_eq!(interleaved["search_scope"], "complete");
    let order: Vec<&str> = interleaved["suggested_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(order, ["score", "seen", "tag", "a", "b", "c", "d", "e"]);
    // The control table measures flat zero padding on disk; it passes with no finding and no
    // frontier, and the long-form bound stays visible as the range.
    let grouped = &tables[1];
    assert_eq!(grouped["name"], "grouped");
    assert_eq!(grouped["current"]["expected_padding"], 0.0);
    assert_eq!(grouped["current"]["padding_max"], 12);
    assert_eq!(grouped["avoidable_bytes_per_row"], 0.0);
    assert!(grouped["frontier"].is_null());
    assert_eq!(
        grouped["dominance_search"], "budgeted",
        "five distinct texts are past the sweep budget"
    );
    // Columns at data-dependent offsets claim no point placement.
    assert!(interleaved["columns"][6]["offset"].is_null(), "{interleaved}");
    assert!(interleaved["columns"][6]["pad_before"].is_null(), "{interleaved}");
    assert!(value["estimate_assumptions"].as_str().unwrap().contains("display-only"));
}

#[test]
fn varlena_text_output_states_the_policy_and_gates_fractionally() {
    let text = bin().arg(fixtures("varlena")).output().unwrap();
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(stdout.contains("■ interleaved"), "{stdout}");
    assert!(stdout.contains("✓ grouped"), "{stdout}");
    assert!(
        stdout.contains("order    : score, seen, tag, a, b, c, d, e"),
        "{stdout}"
    );
    assert!(stdout.contains("dominance-proven"), "{stdout}");
    assert!(
        stdout.contains("no dominating reorder found (dominance search budgeted)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("display-only"),
        "the policy must be stated where the numbers are shown: {stdout}"
    );
    let gated = bin()
        .arg(fixtures("varlena"))
        .args(["--fail-over", "0"])
        .output()
        .unwrap();
    assert_eq!(
        gated.status.code(),
        Some(1),
        "11 B/row dominance-proven must trip a zero gate"
    );
    let fractional = bin()
        .arg(fixtures("varlena"))
        .args(["--fail-over", "10.5"])
        .output()
        .unwrap();
    assert_eq!(fractional.status.code(), Some(1), "fail-over accepts fractions");
}

#[test]
fn frontier_is_reported_and_never_gated() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-frontier-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("V1__band.sql"),
        "CREATE TABLE band_b (k int8 NOT NULL, txt text NOT NULL, arr float8[] NOT NULL);",
    )
    .unwrap();
    let gated = bin().arg(&dir).args(["--fail-over", "0"]).output().unwrap();
    assert_eq!(gated.status.code(), Some(0), "frontier findings never gate");
    let stdout = String::from_utf8_lossy(&gated.stdout);
    assert!(
        stdout.contains("no dominating reorder exists"),
        "a completed sweep may state nonexistence: {stdout}"
    );
    assert!(stdout.contains("frontier :"), "{stdout}");
    assert!(stdout.contains("workload-dependent, not gated"), "{stdout}");
    assert!(stdout.contains("wins when"), "{stdout}");
    let json_out = bin().arg(&dir).args(["--format", "json"]).output().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json_out.stdout).unwrap();
    let frontier = &value["analysis"]["tables"][0]["frontier"];
    assert!(frontier["decided"].as_bool().unwrap(), "{frontier}");
    assert!(!frontier["bands"].as_array().unwrap().is_empty(), "{frontier}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_finite_fail_over_is_rejected() {
    // f64::from_str accepts nan and inf, and `avoidable > nan` is always false: without a
    // validator these silently disable the gate (exit 0 with no message).
    for bad in ["nan", "inf", "1e400", "-1"] {
        let out = bin()
            .arg(fixtures("wasteful"))
            .args(["--fail-over", bad])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "--fail-over {bad} must be rejected");
    }
    let ok = bin()
        .arg(fixtures("wasteful"))
        .args(["--fail-over", "0.5"])
        .output()
        .unwrap();
    assert_eq!(ok.status.code(), Some(1), "a fractional threshold still gates");
}

#[test]
fn missing_path_is_an_error() {
    let out = bin().arg("no/such/path.sql").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn cargo_subcommand_shim() {
    let ok = Command::new(env!("CARGO_BIN_EXE_cargo-rowdiet"))
        .args(["rowdiet", &fixtures("optimal"), "--fail-over", "0"])
        .output()
        .unwrap();
    assert_eq!(ok.status.code(), Some(0));
    let gated = Command::new(env!("CARGO_BIN_EXE_cargo-rowdiet"))
        .args(["rowdiet", &fixtures("wasteful"), "--fail-over", "0"])
        .output()
        .unwrap();
    assert_eq!(gated.status.code(), Some(1));
}

// Both drive `--parser pg-exact`, which a build without the feature rejects.
#[cfg(feature = "pg-exact")]
#[test]
fn pg_exact_parser_matches_default() {
    let default_run = bin()
        .arg(fixtures("wasteful"))
        .args(["--format", "json"])
        .output()
        .unwrap();
    let exact_run = bin()
        .arg(fixtures("wasteful"))
        .args(["--format", "json", "--parser", "pg-exact"])
        .output()
        .unwrap();
    let d: serde_json::Value = serde_json::from_slice(&default_run.stdout).unwrap();
    let e: serde_json::Value = serde_json::from_slice(&exact_run.stdout).unwrap();
    assert_eq!(
        d["analysis"]["tables"][0]["avoidable_bytes_per_row"],
        e["analysis"]["tables"][0]["avoidable_bytes_per_row"]
    );
    assert_eq!(d["analysis"]["tables"][0]["natts"], e["analysis"]["tables"][0]["natts"]);
}

#[test]
fn baseline_lifecycle() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-baseline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("baseline.json");
    let gate = |extra: &[&str]| {
        bin()
            .arg(fixtures("wasteful"))
            .args(extra)
            .arg("--baseline")
            .arg(&file)
            .output()
            .unwrap()
    };
    // Commit the table as V1 created it; V2's appended columns become the judged block.
    let boot = bin()
        .arg(format!("{}/V1__init.sql", fixtures("wasteful")))
        .args(["--fail-over", "0", "--update-baseline", "--baseline"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(boot.status.code(), Some(0), "{}", String::from_utf8_lossy(&boot.stderr));
    assert!(String::from_utf8_lossy(&boot.stdout).contains("baseline written"));
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(!written.contains("bytes"), "no allowance is stored: {written}");
    let block = gate(&[]);
    assert_eq!(block.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&block.stdout);
    assert!(
        stdout.contains("appended block (") && stdout.contains("is not dominance-optimal"),
        "{stdout}"
    );
    assert!(stdout.contains("block order: flags, note"), "{stdout}");
    let accepted = gate(&["--accept", "account"]);
    assert_eq!(
        accepted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let green = gate(&[]);
    assert_eq!(
        green.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&green.stdout)
    );
    // An older file's allowance loads and is ignored, loudly.
    let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    value["tables"]["account"]["bytes"] = 0.into();
    std::fs::write(&file, serde_json::to_string(&value).unwrap()).unwrap();
    let legacy = gate(&[]);
    assert_eq!(legacy.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&legacy.stdout).contains("byte allowances ignored for account"),
        "{}",
        String::from_utf8_lossy(&legacy.stdout)
    );
    // Not a comma-boundary prefix of the real signature: a true non-append change.
    value["tables"]["account"]["layout"] = "f16c".into();
    value["tables"]["account"]["columns"] = serde_json::json!(["id"]);
    std::fs::write(&file, serde_json::to_string(&value).unwrap()).unwrap();
    let modified = gate(&[]);
    assert_eq!(modified.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&modified.stdout).contains("modified since baseline"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn baseline_flag_conflicts_and_requirements() {
    let both = bin()
        .arg(fixtures("wasteful"))
        .args(["--baseline", "b.json", "--update-baseline", "--accept", "account"])
        .output()
        .unwrap();
    assert_eq!(both.status.code(), Some(2));
    let orphan_flag = bin()
        .arg(fixtures("wasteful"))
        .arg("--update-baseline")
        .output()
        .unwrap();
    assert_eq!(orphan_flag.status.code(), Some(2));
}

#[test]
fn github_step_summary_file_is_appended() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-summary-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let summary = dir.join("summary.md");
    std::fs::write(&summary, "# existing\n").unwrap();
    let out = bin()
        .arg(fixtures("wasteful"))
        .args(["--format", "github", "--fail-over", "0"])
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let content = std::fs::read_to_string(&summary).unwrap();
    assert!(content.starts_with("# existing\n"), "{content}");
    assert!(content.contains("## rowdiet"), "{content}");
    assert!(content.contains("| account |"), "{content}");
    std::fs::remove_dir_all(&dir).unwrap();
}

// Both drive `--parser pg-exact`, which a build without the feature rejects.
#[cfg(feature = "pg-exact")]
#[test]
fn baseline_is_portable_across_parser_backends() {
    // Reports key on the fold key, not the backend-dependent display spelling, so a baseline
    // written under one parser gates cleanly under the other (a confirmed pre-fix failure).
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-portable-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sql = dir.join("V1__m.sql");
    std::fs::write(
        &sql,
        "CREATE TABLE MyTable (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);",
    )
    .unwrap();
    let file = dir.join("b.json");
    let wrote = bin()
        .arg(&dir)
        .args(["--fail-over", "0", "--update-baseline", "--baseline"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(wrote.status.code(), Some(0));
    assert!(std::fs::read_to_string(&file).unwrap().contains("\"mytable\""));
    let gated = bin()
        .arg(&dir)
        .args(["--parser", "pg-exact", "--baseline"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(
        gated.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&gated.stdout)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fail_on_degraded_gates_skips() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-degraded-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("V1__b.sql"), "CREATE TABLE broken (a @@@ int);").unwrap();
    let lenient = bin().arg(&dir).args(["--fail-over", "0"]).output().unwrap();
    assert_eq!(lenient.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&lenient.stdout).contains("degraded:"));
    let strict = bin()
        .arg(&dir)
        .args(["--fail-over", "0", "--fail-on-degraded"])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn empty_directory_is_noted_and_gates_under_fail_on_degraded() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-test-{}-empty", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("readme.txt"), "not sql").unwrap();
    let out = bin().arg(&dir).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[empty-scan] no SQL files found"), "{stdout}");
    assert!(stdout.contains("1 path(s) matched no SQL files"), "{stdout}");
    let strict = bin().arg(&dir).arg("--fail-on-degraded").output().unwrap();
    assert_eq!(strict.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_file_arguments_keep_the_given_order() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-test-{}-order", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let create = dir.join("V1__create.sql");
    let alter = dir.join("V2__alter.sql");
    std::fs::write(&create, "CREATE TABLE o (a int NOT NULL);").unwrap();
    std::fs::write(&alter, "ALTER TABLE o ADD COLUMN b bigint NOT NULL;").unwrap();
    // Explicit files are analyzed in argument order, never version-sorted: alter-before-create
    // must leave the ALTER pointing at an unknown table.
    let reversed = bin().arg(&alter).arg(&create).output().unwrap();
    let reversed_stdout = String::from_utf8_lossy(&reversed.stdout);
    assert!(reversed_stdout.contains("unknown-table"), "{reversed_stdout}");
    let ordered = bin()
        .args(["--format", "json"])
        .arg(&create)
        .arg(&alter)
        .output()
        .unwrap();
    let ordered_stdout = String::from_utf8_lossy(&ordered.stdout);
    assert!(!ordered_stdout.contains("unknown_table"), "{ordered_stdout}");
    assert!(ordered_stdout.contains("\"natts\": 2"), "{ordered_stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Lines the GitHub runner would execute: a `::` command after leading whitespace, or `##[` anywhere.
fn workflow_commands(output: &str) -> Vec<String> {
    output
        .split(['\n', '\r'])
        .filter(|line| line.trim_start().starts_with("::") || line.contains("##["))
        .map(str::to_string)
        .collect()
}

#[test]
fn quoted_identifiers_cannot_inject_workflow_commands_in_text_output() {
    let out = bin()
        .arg(fixtures("injection"))
        .args(["--suggest", "--fail-over", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(workflow_commands(&stdout), Vec::<String>::new(), "{stdout}");
    for shape in ["::error", "::stop-commands::", "::add-mask::"] {
        assert!(
            stdout.contains(&format!(r"\n{shape}")),
            "{shape} not shown escaped:\n{stdout}"
        );
    }
}

#[test]
fn github_format_keeps_its_escaping_for_hostile_identifiers() {
    let out = bin()
        .arg(fixtures("injection"))
        .args(["--format", "github"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains('\r'), "{stdout}");
    for line in stdout.lines() {
        assert!(
            ["::warning ", "::error ", "::notice "]
                .iter()
                .any(|level| line.starts_with(level)),
            "a line is not a rowdiet annotation: {line}\n{stdout}"
        );
    }
    assert!(stdout.contains("c%0A::stop-commands::tok2"), "{stdout}");
    assert!(stdout.contains("\"evil%0A::error::TYPE-NOTE\""), "{stdout}");
}

#[test]
fn json_output_round_trips_hostile_identifiers() {
    let out = bin()
        .arg(fixtures("injection"))
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(
        workflow_commands(&String::from_utf8_lossy(&out.stdout)),
        Vec::<String>::new()
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = value["analysis"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"evil\r\n::add-mask::secret"), "{names:?}");
    assert!(names.contains(&"bracket ##[error]INJECTED-V1"), "{names:?}");
}

#[test]
fn frontier_band_and_unverified_type_names_print_escaped() {
    let out = bin().arg(fixtures("injection/V3__frontier.sql")).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(workflow_commands(&stdout), Vec::<String>::new(), "{stdout}");
    assert!(
        stdout.contains(r"frontier : t1\n::error::FRONTIER, v##\u{5b}error]BAND, t2"),
        "{stdout}"
    );
    assert!(
        stdout.contains(r"when v##\u{5b}error]BAND stores long form"),
        "{stdout}"
    );
    assert!(
        stdout.contains(r#"payload lengths unverified for "evil\n::error::UNVERIFIED-TYPE""#),
        "{stdout}"
    );
}

#[test]
fn budgeted_searches_gate_only_under_fail_on_budgeted() {
    let dir = std::env::temp_dir().join(format!("rowdiet-cli-test-{}-budgeted", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cols: Vec<String> = (0..12).map(|i| format!("t{i} text, i{i} integer NOT NULL")).collect();
    std::fs::write(
        dir.join("V1__wide.sql"),
        format!("CREATE TABLE wide ({});", cols.join(", ")),
    )
    .unwrap();
    let lenient = bin().arg(&dir).args(["--fail-over", "1000"]).output().unwrap();
    assert_eq!(lenient.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&lenient.stdout).contains("budgeted: 1 table(s)"));
    let strict = bin()
        .arg(&dir)
        .args(["--fail-over", "1000", "--fail-on-budgeted"])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}
