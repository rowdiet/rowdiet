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
