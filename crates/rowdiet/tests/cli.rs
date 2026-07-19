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
    let boot = bin()
        .arg(fixtures("wasteful"))
        .args(["--fail-over", "0", "--update-baseline", "--baseline"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(boot.status.code(), Some(0), "{}", String::from_utf8_lossy(&boot.stderr));
    assert!(String::from_utf8_lossy(&boot.stdout).contains("baseline written"));
    let green = bin()
        .arg(fixtures("wasteful"))
        .arg("--baseline")
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(
        green.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&green.stdout)
    );
    let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    value["tables"]["account"]["bytes"] = 0.into();
    std::fs::write(&file, serde_json::to_string(&value).unwrap()).unwrap();
    let regressed = bin()
        .arg(fixtures("wasteful"))
        .arg("--baseline")
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(regressed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&regressed.stdout).contains("regression"));
    // Not a comma-boundary prefix of the real signature — a true non-append change. (A prefix
    // value here, e.g. "f1c", would legitimately pass via the grown-since-baseline rule.)
    value["tables"]["account"]["layout"] = "f16c".into();
    std::fs::write(&file, serde_json::to_string(&value).unwrap()).unwrap();
    let modified = bin()
        .arg(fixtures("wasteful"))
        .arg("--baseline")
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(modified.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&modified.stdout).contains("modified since baseline"));
    let accepted = bin()
        .arg(fixtures("wasteful"))
        .args(["--accept", "account", "--baseline"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(
        accepted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let regated = bin()
        .arg(fixtures("wasteful"))
        .arg("--baseline")
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(
        regated.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&regated.stdout)
    );
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
