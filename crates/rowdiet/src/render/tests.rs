use super::*;
use rowdiet_core::{analyze_sources, baseline, Baseline, BaselineEntry, Config, SqlSource};

fn analyze(sql: &str) -> Analysis {
    analyze_sources(
        &[SqlSource {
            name: "V1__init.sql".into(),
            sql: sql.into(),
        }],
        &Config::default(),
    )
}

/// 8 B/row avoidable, signature `f1c,f8d,f2s,f8d`.
fn sample() -> Analysis {
    analyze("CREATE TABLE account (active boolean NOT NULL, id bigint PRIMARY KEY, kind smallint NOT NULL, balance bigint NOT NULL);")
}

fn gate(analysis: &Analysis, fail_over: Option<u64>) -> GateOutcome {
    baseline::evaluate(analysis, fail_over, None)
}

fn baselined(analysis: &Analysis, entries: &[(&str, u64, &str)]) -> GateOutcome {
    let base = Baseline {
        rowdiet: "test".into(),
        fail_over: 0,
        tables: entries
            .iter()
            .map(|(name, bytes, layout)| {
                (
                    name.to_string(),
                    BaselineEntry {
                        bytes: *bytes,
                        layout: layout.to_string(),
                    },
                )
            })
            .collect(),
    };
    baseline::evaluate(analysis, None, Some(&base))
}

#[test]
fn text_report_mentions_the_essentials() {
    let analysis = sample();
    let rendered = text(&analysis, Some(1_000_000), true, &gate(&analysis, Some(0)));
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
    let analysis = analyze("CREATE TABLE ok (id bigint NOT NULL, n integer NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0)));
    assert!(rendered.contains("✓ ok"));
    assert!(rendered.contains("optimal: zero padding"));
    assert!(!rendered.contains("FAIL:"));
}

#[test]
fn empty_modeled_tables_are_not_called_optimal() {
    let analysis = analyze("CREATE TABLE c PARTITION OF elsewhere FOR VALUES FROM (1) TO (2);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0)));
    assert!(rendered.contains("◌ c"), "{rendered}");
    assert!(rendered.contains("not analyzable"), "{rendered}");
    assert!(!rendered.contains("✓ c"), "{rendered}");
    assert!(!rendered.contains("FAIL"), "{rendered}");
}

#[test]
fn partition_children_with_known_parent_render_real_analysis() {
    let analysis = analyze(
        "CREATE TABLE p (flag boolean NOT NULL, id bigint NOT NULL) PARTITION BY RANGE (id);\nCREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);",
    );
    let rendered = text(&analysis, None, false, &gate(&analysis, None));
    assert!(rendered.contains("✓ c") || rendered.contains("■ c"), "{rendered}");
    assert!(!rendered.contains("◌ c"), "{rendered}");
}

#[test]
fn baseline_verdicts_in_text_output() {
    let analysis = sample();
    let sig = &analysis.tables[0].layout_signature;
    let regressed = text(&analysis, None, false, &baselined(&analysis, &[("account", 4, sig)]));
    assert!(
        regressed.contains("✗ regression: 8 B/row exceeds the baselined allowance of 4"),
        "{regressed}"
    );
    assert!(regressed.contains("FAIL: 1 regression(s) vs baseline"), "{regressed}");
    let modified = text(&analysis, None, false, &baselined(&analysis, &[("account", 8, "f16c")]));
    assert!(modified.contains("✗ modified since baseline"), "{modified}");
    assert!(modified.contains("--accept account"), "{modified}");
    let ratchet = text(&analysis, None, false, &baselined(&analysis, &[("account", 12, sig)]));
    assert!(
        ratchet.contains("↓ ratchet: allowance 12 can tighten to 8"),
        "{ratchet}"
    );
    assert!(!ratchet.contains("FAIL"), "{ratchet}");
    let orphanish = text(
        &analysis,
        None,
        false,
        &baselined(&analysis, &[("account", 8, sig), ("ghost", 1, "vi")]),
    );
    assert!(
        orphanish.contains("orphaned entries (no matching table): ghost"),
        "{orphanish}"
    );
}

#[test]
fn grown_since_baseline_in_text_output() {
    let analysis = analyze(
        "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);
         ALTER TABLE t ADD COLUMN e boolean NOT NULL;
         ALTER TABLE t ADD COLUMN f bigint NOT NULL;",
    );
    let rendered = text(&analysis, None, false, &baselined(&analysis, &[("t", 0, "f4i,f8d")]));
    assert!(rendered.contains("✗ grown since baseline"), "{rendered}");
    assert!(
        rendered.contains("grown since baseline") && rendered.contains("FAIL:"),
        "{rendered}"
    );
}

#[test]
fn github_annotations() {
    let analysis = sample();
    let rendered = github(&analysis, &gate(&analysis, Some(0)));
    assert!(rendered.starts_with("::error file=V1__init.sql,line=1,title=rowdiet::table account"));
    let warn = github(&analysis, &gate(&analysis, None));
    assert!(warn.starts_with("::warning "));
    let sig = &analysis.tables[0].layout_signature;
    let regressed = github(&analysis, &baselined(&analysis, &[("account", 4, sig)]));
    assert!(
        regressed.starts_with("::error file=V1__init.sql,line=1,title=rowdiet regression::"),
        "{regressed}"
    );
}

#[test]
fn json_shape() {
    let analysis = sample();
    let rendered = json(&analysis, Some(0), &gate(&analysis, Some(0))).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["gate_exceeded"], true);
    assert_eq!(value["gate"]["exceeded"], true);
    assert_eq!(value["gate"]["verdicts"]["account"]["verdict"], "new_violation");
    assert_eq!(value["gate"]["verdicts"]["account"]["avoidable"], 8);
    assert_eq!(value["analysis"]["tables"][0]["avoidable_bytes_per_row"], 8);
    assert_eq!(value["analysis"]["tables"][0]["tier"], "exact");
    assert_eq!(value["analysis"]["tables"][0]["layout_signature"], "f1c,f8d,f2s,f8d");
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
