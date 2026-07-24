use super::*;
use crate::{Config, SqlSource, analyze_sources};

/// 8 B/row avoidable (56 → 48 footprint), signature `f4i,f8d,f4i,f8d`.
const WASTEFUL: &str = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);";
const WASTEFUL_SIG: &str = "f4i,f8d,f4i,f8d";

fn analysis(sql: &str) -> Analysis {
    analyze_sources(
        &[SqlSource {
            name: "V1__t.sql".into(),
            sql: sql.into(),
        }],
        &Config::default(),
    )
}

fn baseline(fail_over: u64, tables: &[(&str, u64, &str)]) -> Baseline {
    Baseline {
        rowdiet: "test".into(),
        fail_over,
        tables: tables
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
    }
}

#[test]
fn layout_signatures_emitted() {
    let a = analysis(WASTEFUL);
    assert_eq!(a.tables[0].layout_signature, WASTEFUL_SIG);
    let b = analysis("CREATE TABLE v (a text, b varchar(20), c bigint NOT NULL);");
    assert_eq!(b.tables[0].layout_signature, "vi,vip,f8d");
}

#[test]
fn no_gate_without_fail_over_or_baseline() {
    let outcome = evaluate(&analysis(WASTEFUL), None, None);
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
}

#[test]
fn fail_over_alone_flags_new_violation() {
    let outcome = evaluate(&analysis(WASTEFUL), Some(0), None);
    assert!(outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::NewViolation { avoidable: 8 });
    let lenient = evaluate(&analysis(WASTEFUL), Some(8), None);
    assert!(!lenient.exceeded);
}

#[test]
fn baselined_table_passes_at_its_allowance() {
    let base = baseline(0, &[("t", 8, WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(WASTEFUL), None, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
    assert!(outcome.orphaned.is_empty());
    assert!(outcome.expired.is_empty());
}

#[test]
fn tightened_allowance_flags_regression() {
    let base = baseline(0, &[("t", 4, WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(WASTEFUL), None, Some(&base));
    assert!(outcome.exceeded);
    assert_eq!(
        outcome.verdicts["t"],
        TableVerdict::Regression {
            avoidable: 8,
            allowed: 4
        }
    );
}

#[test]
fn improvement_is_a_ratchet_opportunity_not_auto_tightened() {
    let base = baseline(0, &[("t", 12, WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(WASTEFUL), None, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(
        outcome.verdicts["t"],
        TableVerdict::RatchetOpportunity {
            avoidable: 8,
            allowed: 12
        }
    );
}

#[test]
fn appended_column_keeps_the_allowance_alive() {
    // The brownfield ADD COLUMN scenario: the applied table cannot be rewritten, and the aligned
    // append adds no avoidable waste — the entry must keep passing, not expire into a failure.
    let grown = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        ALTER TABLE t ADD COLUMN e bigint NOT NULL;";
    let a = analysis(grown);
    assert_eq!(a.tables[0].layout_signature, "f4i,f8d,f4i,f8d,f8d");
    let base = baseline(0, &[("t", 8, WASTEFUL_SIG)]);
    let outcome = evaluate(&a, None, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
    assert!(outcome.expired.is_empty());
}

#[test]
fn wasteful_append_flags_grown_not_modified() {
    let grown = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        ALTER TABLE t ADD COLUMN e boolean NOT NULL;
        ALTER TABLE t ADD COLUMN f bigint NOT NULL;
        ALTER TABLE t ADD COLUMN g boolean NOT NULL;
        ALTER TABLE t ADD COLUMN h bigint NOT NULL;";
    let base = baseline(0, &[("t", 8, WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(grown), None, Some(&base));
    assert!(outcome.exceeded);
    assert_eq!(
        outcome.verdicts["t"],
        TableVerdict::GrownSinceBaseline {
            avoidable: 16,
            allowed: 8
        }
    );
}

#[test]
fn non_append_change_expires_the_allowance() {
    let base = baseline(0, &[("t", 8, "f16c")]);
    let outcome = evaluate(&analysis(WASTEFUL), None, Some(&base));
    assert!(outcome.exceeded);
    assert_eq!(
        outcome.verdicts["t"],
        TableVerdict::ModifiedSinceBaseline { avoidable: 8 }
    );
    assert!(outcome.expired.is_empty());
}

#[test]
fn reordered_to_clean_reports_expired_entry() {
    let reordered = "CREATE TABLE t (b bigint NOT NULL, d bigint NOT NULL, a int NOT NULL, c int NOT NULL);";
    let base = baseline(0, &[("t", 8, WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(reordered), None, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
    assert_eq!(outcome.expired, vec!["t".to_string()]);
}

#[test]
fn explicit_fail_over_overrides_the_files() {
    let base = baseline(8, &[]);
    assert!(!evaluate(&analysis(WASTEFUL), None, Some(&base)).exceeded);
    let strict = evaluate(&analysis(WASTEFUL), Some(0), Some(&base));
    assert!(strict.exceeded);
    assert_eq!(strict.verdicts["t"], TableVerdict::NewViolation { avoidable: 8 });
}

#[test]
fn orphaned_entries_reported_not_failed() {
    let base = baseline(0, &[("ghost", 4, "f4i")]);
    let sql = "CREATE TABLE t (b bigint NOT NULL, a int NOT NULL, c int NOT NULL);";
    let outcome = evaluate(&analysis(sql), None, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.orphaned, vec!["ghost".to_string()]);
}

#[test]
fn ignored_tables_stay_outside_gate_and_baseline() {
    let sql = "CREATE TABLE ig ( -- rowdiet:ignore
        a int NOT NULL, b bigint NOT NULL);";
    let base = baseline(0, &[("ig", 0, "f4i,f8d")]);
    let outcome = evaluate(&analysis(sql), None, Some(&base));
    assert!(!outcome.exceeded);
    assert!(!outcome.verdicts.contains_key("ig"));
    assert_eq!(outcome.orphaned, vec!["ig".to_string()]);
}

#[test]
fn build_from_records_only_debt() {
    let sql = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        CREATE TABLE u (a bigint NOT NULL);";
    let base = build_from(&analysis(sql), 0, "1.2.3");
    assert_eq!(base.rowdiet, "1.2.3");
    assert_eq!(base.fail_over, 0);
    assert_eq!(base.tables.len(), 1);
    assert_eq!(
        base.tables["t"],
        BaselineEntry {
            bytes: 8,
            layout: WASTEFUL_SIG.into()
        }
    );
}

#[test]
fn accept_refreshes_named_entries_and_prunes_clean_ones() {
    let sql = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        CREATE TABLE u (a bigint NOT NULL);";
    let a = analysis(sql);
    let mut base = baseline(0, &[("t", 99, "stale"), ("u", 5, "stale"), ("ghost", 1, "f4i")]);
    accept_tables(&mut base, &a, &["t".into(), "u".into()]).unwrap();
    assert_eq!(
        base.tables["t"],
        BaselineEntry {
            bytes: 8,
            layout: WASTEFUL_SIG.into()
        }
    );
    assert!(!base.tables.contains_key("u"));
    assert!(base.tables.contains_key("ghost"));
    let err = accept_tables(&mut base, &a, &["nope".into()]).unwrap_err();
    assert!(err.contains("nope"));
}

#[test]
fn prefix_relation_respects_comma_boundaries() {
    assert!(matches!(relation("f8d,f4i", "f8d,f4i"), SignatureRelation::Match));
    assert!(matches!(relation("f8d", "f8d,f4i"), SignatureRelation::Grown));
    assert!(matches!(relation("f8d,vi", "f8d,vi,f4i"), SignatureRelation::Grown));
    // `vi` → `vip` is a typmod change on the last column, not an append.
    assert!(matches!(
        relation("f8d,vi", "f8d,vip,f4i"),
        SignatureRelation::Different
    ));
    assert!(matches!(relation("", "vi"), SignatureRelation::Different));
    assert!(matches!(relation("f8d,f4i", "f8d"), SignatureRelation::Different));
}
