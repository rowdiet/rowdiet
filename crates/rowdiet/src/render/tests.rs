use super::*;
use rowdiet_core::{analyze_sources, Config, SqlSource};

fn sample() -> Analysis {
    let sql = "CREATE TABLE account (active boolean NOT NULL, id bigint PRIMARY KEY, kind smallint NOT NULL, balance bigint NOT NULL);";
    analyze_sources(
        &[SqlSource {
            name: "V1__init.sql".into(),
            sql: sql.into(),
        }],
        &Config::default(),
    )
}

#[test]
fn text_report_mentions_the_essentials() {
    let rendered = text(&sample(), Some(1_000_000), true, Some(0));
    assert!(rendered.contains("account"));
    assert!(rendered.contains("V1__init.sql:1"));
    assert!(rendered.contains("B/row avoidable"));
    assert!(rendered.contains("order    : id, balance, kind, active"));
    assert!(rendered.contains("≈ 8.0 MB"));
    assert!(rendered.contains("CREATE TABLE account ("));
    assert!(rendered.contains("FAIL:"));
}

#[test]
fn optimal_table_is_a_checkmark_line() {
    let sql = "CREATE TABLE ok (id bigint NOT NULL, n integer NOT NULL);";
    let analysis = analyze_sources(
        &[SqlSource {
            name: "V1__ok.sql".into(),
            sql: sql.into(),
        }],
        &Config::default(),
    );
    let rendered = text(&analysis, None, false, Some(0));
    assert!(rendered.contains("✓ ok"));
    assert!(rendered.contains("optimal: zero padding"));
    assert!(!rendered.contains("FAIL:"));
}

#[test]
fn empty_modeled_tables_are_not_called_optimal() {
    let sql = "CREATE TABLE p (a int NOT NULL) PARTITION BY RANGE (a);\nCREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);";
    let analysis =
        analyze_sources(&[SqlSource { name: "V1__p.sql".into(), sql: sql.into() }], &Config::default());
    let rendered = text(&analysis, None, false, Some(0));
    assert!(rendered.contains("◌ c"), "{rendered}");
    assert!(rendered.contains("not analyzable"), "{rendered}");
    assert!(!rendered.contains("✓ c"), "{rendered}");
    assert!(!rendered.contains("FAIL"), "{rendered}");
}

#[test]
fn github_annotations() {
    let rendered = github(&sample(), Some(0));
    assert!(rendered.starts_with("::error file=V1__init.sql,line=1,title=rowdiet::table account"));
    let warn = github(&sample(), None);
    assert!(warn.starts_with("::warning "));
}

#[test]
fn json_shape() {
    let rendered = json(&sample(), Some(0), true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["gate_exceeded"], true);
    assert_eq!(value["analysis"]["tables"][0]["avoidable_bytes_per_row"], 8);
    assert_eq!(value["analysis"]["tables"][0]["tier"], "exact");
}

#[test]
fn human_units() {
    assert_eq!(human_bytes(999), "999 B");
    assert_eq!(human_bytes(8_000_000), "8.0 MB");
    assert_eq!(human_bytes(12_500_000_000), "12.5 GB");
}

#[test]
fn quoting_only_when_needed() {
    assert_eq!(maybe_quote("plain_name2"), "plain_name2");
    assert_eq!(maybe_quote("Mixed"), "\"Mixed\"");
    assert_eq!(maybe_quote("select"), "select");
    assert_eq!(maybe_quote("1st"), "\"1st\"");
}
