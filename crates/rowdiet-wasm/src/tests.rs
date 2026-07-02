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
    let out_ptr = rowdiet_lint(input_ptr, bytes.len());
    let json_len = u32::from_le_bytes(unsafe { *(out_ptr as *const [u8; 4]) }) as usize;
    let json = unsafe { std::slice::from_raw_parts(out_ptr.add(4), json_len) };
    let out: serde_json::Value = serde_json::from_slice(json).unwrap();
    assert_eq!(out["gate_exceeded"], true);
    rowdiet_free(input_ptr, bytes.len());
    rowdiet_free(out_ptr, 4 + json_len);
}
