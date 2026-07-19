use super::*;

const INPUT: &str = r#"{
  "sources": [{"name": "V1__m.sql", "sql": "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);"}],
  "fail_over": 0
}"#;

#[test]
fn lint_json_end_to_end() {
    let out: serde_json::Value = serde_json::from_str(&lint_json(INPUT)).unwrap();
    assert_eq!(out["gate_exceeded"], true);
    assert_eq!(out["analysis"]["tables"][0]["avoidable_bytes_per_row"], 8);
    let expected_parser = if cfg!(feature = "pg-exact") {
        "pg-exact"
    } else {
        "sqlparser"
    };
    assert_eq!(out["parser"], expected_parser);
}

#[test]
fn bad_input_reports_error() {
    let out: serde_json::Value = serde_json::from_str(&lint_json("not json")).unwrap();
    assert!(out["error"].as_str().unwrap().starts_with("bad input JSON"));
    let out: serde_json::Value = serde_json::from_str(&lint_json(r#"{"sources": [], "assume": ["nope"]}"#)).unwrap();
    assert!(out["error"].as_str().unwrap().contains("assume-type"));
}

#[test]
fn assume_specs_apply() {
    let input = r#"{
      "sources": [{"name": "V1__t.sql", "sql": "CREATE TABLE t (v wat_type NOT NULL, id bigint NOT NULL);"}],
      "assume": ["wat_type=fixed:8:d"]
    }"#;
    let out: serde_json::Value = serde_json::from_str(&lint_json(input)).unwrap();
    assert_eq!(out["analysis"]["tables"][0]["tier"], "exact");
    assert_eq!(out["analysis"]["notes"].as_array().unwrap().len(), 0);
}

#[test]
fn abi_roundtrip() {
    let bytes = INPUT.as_bytes();
    let input_ptr = rowdiet_alloc(bytes.len());
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), input_ptr, bytes.len()) };
    let (out_ptr, out_len) = unsafe { lint_raw(input_ptr, bytes.len()) };
    let json = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
    let out: serde_json::Value = serde_json::from_slice(json).unwrap();
    assert_eq!(out["gate_exceeded"], true);
    rowdiet_free(input_ptr, bytes.len());
    rowdiet_free(out_ptr, out_len);
}

#[test]
fn packed_return_layout() {
    assert_eq!(pack(0x1234, 0x56), 0x0000_1234_0000_0056);
    assert_eq!(pack(u32::MAX, u32::MAX), u64::MAX);
}

#[test]
fn baseline_input_gates_and_reports_verdicts() {
    let baselined = r#"{
      "sources": [{"name": "V1__m.sql", "sql": "CREATE TABLE m (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);"}],
      "baseline": {"fail_over": 0, "tables": {"m": {"bytes": 8, "layout": "f4i,f8d,f4i,f8d"}}}
    }"#;
    let out: serde_json::Value = serde_json::from_str(&lint_json(baselined)).unwrap();
    assert_eq!(out["gate_exceeded"], false);
    assert_eq!(out["gate"]["verdicts"]["m"]["verdict"], "pass");
    assert_eq!(out["fail_over"], 0);
    assert_eq!(out["analysis"]["tables"][0]["layout_signature"], "f4i,f8d,f4i,f8d");
    let tightened = baselined.replace("\"bytes\": 8", "\"bytes\": 4");
    let out: serde_json::Value = serde_json::from_str(&lint_json(&tightened)).unwrap();
    assert_eq!(out["gate_exceeded"], true);
    assert_eq!(out["gate"]["verdicts"]["m"]["verdict"], "regression");
    assert_eq!(out["gate"]["verdicts"]["m"]["allowed"], 4);
}
