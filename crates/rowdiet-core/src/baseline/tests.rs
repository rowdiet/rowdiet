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

fn layout(signature: &str) -> CommittedLayout {
    CommittedLayout::parse(signature).expect("a layout")
}

fn entry(signature: &str) -> BaselineEntry {
    BaselineEntry::new(layout(signature))
}

fn baseline(fail_over: f64, tables: &[(&str, &str)]) -> Baseline {
    Baseline {
        rowdiet: "test".into(),
        fail_over,
        tables: tables
            .iter()
            .map(|(name, layout)| (name.to_string(), entry(layout)))
            .collect(),
    }
}

/// A prefix migration plus an appended block, as two files in apply order.
fn migrations(prefix: &str, block: &str) -> Analysis {
    analyze_sources(
        &[
            SqlSource::new("V1__create.sql", prefix),
            SqlSource::new("V2__append.sql", block),
        ],
        &Config::default(),
    )
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
    let outcome = evaluate(&analysis(WASTEFUL), None, false, None);
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
}

#[test]
fn fail_over_alone_flags_new_violation() {
    let outcome = evaluate(&analysis(WASTEFUL), Some(0.0), false, None);
    assert!(outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::NewViolation { avoidable: 8.0 });
    let lenient = evaluate(&analysis(WASTEFUL), Some(8.0), false, None);
    assert!(!lenient.exceeded);
}

#[test]
fn a_committed_layout_is_never_judged_again() {
    // The whole table is the committed prefix: nothing is free to reorder, so 8 B/row of
    // committed waste passes at fail-over 0.
    let base = baseline(0.0, &[("t", WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(WASTEFUL), None, false, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
    assert!(outcome.blocks.is_empty());
    assert!(outcome.orphaned.is_empty());
    assert!(outcome.expired.is_empty());
}

#[test]
fn an_aligned_append_passes() {
    let grown = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        ALTER TABLE t ADD COLUMN e bigint NOT NULL;";
    let a = analysis(grown);
    assert_eq!(a.tables[0].layout_signature, "f4i,f8d,f4i,f8d,f8d");
    let base = baseline(0.0, &[("t", WASTEFUL_SIG)]);
    let outcome = evaluate(&a, None, false, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
    let block = &outcome.blocks["t"];
    assert_eq!(block.prefix_columns, 4);
    assert_eq!(block.columns, vec!["e"]);
    assert_eq!(block.avoidable_bytes_per_row, 0.0);
    assert!(outcome.expired.is_empty());
}

#[test]
fn a_wasteful_block_fails_with_its_block_order() {
    // (bool, bigint, bool, bigint) appended after a prefix ending at offset 32: the written
    // block pads 14 bytes; (bigint, bigint, bool, bool) pads none and the row shrinks from 88
    // to 80 bytes, the exact-tier saving. The prefix is not reordered.
    let grown = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        ALTER TABLE t ADD COLUMN e boolean NOT NULL;
        ALTER TABLE t ADD COLUMN f bigint NOT NULL;
        ALTER TABLE t ADD COLUMN g boolean NOT NULL;
        ALTER TABLE t ADD COLUMN h bigint NOT NULL;";
    let base = baseline(0.0, &[("t", WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(grown), None, false, Some(&base));
    assert!(outcome.exceeded);
    assert_eq!(
        outcome.verdicts["t"],
        TableVerdict::BlockNotDominanceOptimal {
            avoidable: 8.0,
            appended: 4
        }
    );
    let block = &outcome.blocks["t"];
    assert_eq!(block.suggested_order, vec!["f", "h", "e", "g"]);
    assert_eq!(block.avoidable_deterministic, 8);
    assert_eq!(block.origins.len(), 4, "one origin per appending statement");
    assert_eq!(
        block.origins[0].line, 2,
        "points at the statements that appended the block"
    );
}

/// The two false negatives the #11 reviews measured, as committed prefixes with an appended
/// block: the block-scoped search finds each fix without touching the prefix.
mod review_false_negatives {
    use super::*;

    #[test]
    fn wide25_block_takes_the_deterministic_two_byte_fix() {
        // 21 bigints and a timetz committed; (timetz, smallint, text) appended. Written, the
        // second timetz pads 4 behind the first and the text may pad 2 more; (smallint, timetz,
        // text) pads a deterministic 2 and leaves the text aligned.
        let prefix_cols: Vec<String> = (0..21)
            .map(|i| format!("b{i} bigint NOT NULL"))
            .chain(["t1 timetz NOT NULL".to_string()])
            .collect();
        let prefix = format!("CREATE TABLE wide25 ({});", prefix_cols.join(", "));
        let block = "ALTER TABLE wide25 ADD COLUMN t2 timetz NOT NULL, ADD COLUMN s smallint NOT NULL, ADD COLUMN note text NOT NULL;";
        let committed = analysis(&prefix);
        let base = build_from(&committed, 0.0, "test");
        let a = migrations(&prefix, block);
        let outcome = evaluate(&a, None, false, Some(&base));
        let found = &outcome.blocks["wide25"];
        assert_eq!(found.prefix_columns, 22);
        assert_eq!(found.suggested_order, vec!["s", "t2", "note"]);
        assert_eq!(
            found.dominance_saving,
            Some(crate::report::SavingRange { min: 2, max: 4 })
        );
        assert_eq!((found.avoidable_deterministic, found.avoidable_dominance), (2, 2));
        assert_eq!(found.dominance_search, crate::report::DominanceScope::Exhaustive);
        assert!(outcome.exceeded);
    }

    #[test]
    fn cliff25_block_takes_the_full_fix() {
        // 24 fixed columns and a text, 25 in all: the aligned bigints and ints committed, the
        // timetz, macaddr, smallint, and boolean columns appended grouped by type, which pads 18
        // deterministic bytes (and 0-2 at the text). The block's order space is too large to
        // sweep, so the search poles find the 12-byte repack and the clean claim is "found".
        let mut prefix_cols: Vec<String> = Vec::new();
        for i in 0..4 {
            prefix_cols.push(format!("b{i} bigint NOT NULL"));
        }
        for i in 0..4 {
            prefix_cols.push(format!("i{i} integer NOT NULL"));
        }
        let prefix = format!("CREATE TABLE cliff25 ({});", prefix_cols.join(", "));
        let mut block_cols: Vec<String> = Vec::new();
        for (name, ty) in [("tz", "timetz"), ("m", "macaddr"), ("s", "smallint"), ("f", "boolean")] {
            for i in 0..4 {
                block_cols.push(format!("ADD COLUMN {name}{i} {ty} NOT NULL"));
            }
        }
        block_cols.push("ADD COLUMN note text NOT NULL".into());
        let block = format!("ALTER TABLE cliff25 {};", block_cols.join(", "));
        let base = build_from(&analysis(&prefix), 0.0, "test");
        let a = migrations(&prefix, &block);
        assert_eq!(a.tables[0].natts, 25);
        let outcome = evaluate(&a, None, false, Some(&base));
        let found = &outcome.blocks["cliff25"];
        assert_eq!(found.prefix_columns, 8);
        assert_eq!(found.avoidable_deterministic, 12, "{found:#?}");
        assert_eq!(
            found.dominance_saving,
            Some(crate::report::SavingRange { min: 12, max: 12 })
        );
        assert_eq!(found.search_scope, crate::layout::SearchScope::Complete);
        assert!(outcome.exceeded);
    }
}

#[test]
fn non_append_change_expires_the_entry() {
    let base = baseline(0.0, &[("t", "f16c")]);
    let outcome = evaluate(&analysis(WASTEFUL), None, false, Some(&base));
    assert!(outcome.exceeded);
    assert_eq!(
        outcome.verdicts["t"],
        TableVerdict::ModifiedSinceBaseline { avoidable: 8.0 }
    );
    assert!(outcome.expired.is_empty());
}

#[test]
fn reordered_to_clean_reports_expired_entry() {
    let reordered = "CREATE TABLE t (b bigint NOT NULL, d bigint NOT NULL, a int NOT NULL, c int NOT NULL);";
    let base = baseline(0.0, &[("t", WASTEFUL_SIG)]);
    let outcome = evaluate(&analysis(reordered), None, false, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
    assert_eq!(outcome.expired, vec!["t".to_string()]);
}

#[test]
fn explicit_fail_over_overrides_the_files() {
    let base = baseline(8.0, &[]);
    assert!(!evaluate(&analysis(WASTEFUL), None, false, Some(&base)).exceeded);
    let strict = evaluate(&analysis(WASTEFUL), Some(0.0), false, Some(&base));
    assert!(strict.exceeded);
    assert_eq!(strict.verdicts["t"], TableVerdict::NewViolation { avoidable: 8.0 });
}

#[test]
fn orphaned_entries_reported_not_failed() {
    let base = baseline(0.0, &[("ghost", "f4i")]);
    let sql = "CREATE TABLE t (b bigint NOT NULL, a int NOT NULL, c int NOT NULL);";
    let outcome = evaluate(&analysis(sql), None, false, Some(&base));
    assert!(!outcome.exceeded);
    assert_eq!(outcome.orphaned, vec!["ghost".to_string()]);
}

#[test]
fn ignored_tables_stay_outside_gate_and_baseline() {
    let sql = "CREATE TABLE ig ( -- rowdiet:ignore
        a int NOT NULL, b bigint NOT NULL);";
    let base = baseline(0.0, &[("ig", "f4i,f8d")]);
    let outcome = evaluate(&analysis(sql), None, false, Some(&base));
    assert!(!outcome.exceeded);
    assert!(!outcome.verdicts.contains_key("ig"));
    assert_eq!(outcome.orphaned, vec!["ig".to_string()]);
}

#[test]
fn build_from_commits_every_modeled_table() {
    let sql = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        CREATE TABLE u (a bigint NOT NULL);
        CREATE TABLE w (LIKE elsewhere);";
    let base = build_from(&analysis(sql), 0.0, "1.2.3");
    assert_eq!(base.rowdiet, "1.2.3");
    assert_eq!(base.fail_over, 0.0);
    assert_eq!(
        base.tables.len(),
        2,
        "incomplete tables have no committed layout to record"
    );
    assert_eq!(base.tables["t"].layout, layout(WASTEFUL_SIG));
    assert_eq!(base.tables["t"].columns, vec!["a", "b", "c", "d"]);
    assert_eq!(base.tables["u"].layout, layout("f8d"));
}

#[test]
fn accept_records_the_written_block() {
    let sql = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        CREATE TABLE u (a bigint NOT NULL);";
    let a = analysis(sql);
    let mut base = baseline(0.0, &[("t", "f1c"), ("u", "f1c"), ("ghost", "f4i")]);
    accept_tables(&mut base, &a, &["t".into(), "u".into()]).unwrap();
    assert_eq!(base.tables["t"].layout, layout(WASTEFUL_SIG));
    assert_eq!(base.tables["t"].columns, vec!["a", "b", "c", "d"]);
    assert_eq!(base.tables["u"].layout, layout("f8d"));
    assert!(base.tables.contains_key("ghost"));
    let err = accept_tables(&mut base, &a, &["nope".into()]).unwrap_err();
    assert!(err.contains("nope"));
}

#[test]
fn an_accepted_block_becomes_the_next_prefix() {
    let grown = "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);
        ALTER TABLE t ADD COLUMN e boolean NOT NULL, ADD COLUMN f bigint NOT NULL,
            ADD COLUMN g boolean NOT NULL, ADD COLUMN h bigint NOT NULL;";
    let a = analysis(grown);
    let mut base = baseline(0.0, &[("t", WASTEFUL_SIG)]);
    assert!(evaluate(&a, None, false, Some(&base)).exceeded);
    accept_tables(&mut base, &a, &["t".into()]).unwrap();
    let outcome = evaluate(&a, None, false, Some(&base));
    assert!(!outcome.exceeded);
    assert!(outcome.blocks.is_empty());
}

#[test]
fn relation_compares_slot_by_slot() {
    let rel = |a: &str, b: &str| relation(&layout(a), &layout(b));
    assert!(matches!(rel("f8d,f4i", "f8d,f4i"), SignatureRelation::Match));
    assert!(matches!(
        rel("f8d", "f8d,f4i"),
        SignatureRelation::Grown { committed_slots: 1 }
    ));
    assert!(matches!(
        rel("f8d,vi", "f8d,vi,f4i"),
        SignatureRelation::Grown { committed_slots: 2 }
    ));
    // `vi` → `vip` is a typmod change on a committed column, not an append.
    assert!(matches!(rel("f8d,vi", "f8d,vip,f4i"), SignatureRelation::Different));
    assert!(matches!(rel("", "vi"), SignatureRelation::Grown { committed_slots: 0 }));
    assert!(matches!(rel("f8d,f4i", "f8d"), SignatureRelation::Different));
    // A committed column dropped since keeps its slot: still the committed prefix.
    assert!(matches!(rel("f8d,f1c,f2s", "f8d,f1c,-"), SignatureRelation::Match));
    assert!(matches!(
        rel("f8d,f1c,f2s", "f8d,f1c,-,f2s,f4i,f1c"),
        SignatureRelation::Grown { committed_slots: 3 }
    ));
    // A dropped slot never comes back.
    assert!(matches!(rel("f8d,-", "f8d,f4i"), SignatureRelation::Different));
}

#[test]
fn layouts_parse_or_fail_loudly() {
    assert_eq!(layout("f8d,-,vip,vc").slots(), ["f8d", "-", "vip", "vc"]);
    for bad in ["garbage", "f8", "fxd", "v", "vq", "f8d,", "vpi", "f8d,,f4i"] {
        assert!(CommittedLayout::parse(bad).is_err(), "{bad}");
    }
}

#[cfg(feature = "serde")]
#[test]
fn a_file_that_lists_a_table_twice_is_rejected() {
    let json = r#"{"fail_over": 0, "tables": {"t": {"layout": "f8d"}, "t": {"layout": "f8d,f4i"}}}"#;
    let err = serde_json::from_str::<Baseline>(json).unwrap_err().to_string();
    assert!(err.contains("listed twice"), "{err}");
    let garbage = r#"{"fail_over": 0, "tables": {"t": {"layout": "garbage"}}}"#;
    assert!(serde_json::from_str::<Baseline>(garbage).is_err());
}

#[test]
fn drop_then_add_is_judged_as_the_appended_block() {
    // The stack review's D3-1: (id, flag, n) committed, then n dropped and (x, y, z) added.
    // PostgreSQL keeps n's slot, so x, y and z are the block, behind (id, flag) and the dropped
    // slot. Written, the row pads 1 before y; (z, x, y) pads nothing (measured 41 B tuples and
    // 48 B page spacing as written, 40 B and 40 B reordered).
    let sql = "CREATE TABLE t (id bigint NOT NULL, flag boolean NOT NULL, n smallint NOT NULL);
        ALTER TABLE t DROP COLUMN n;
        ALTER TABLE t ADD COLUMN x smallint NOT NULL, ADD COLUMN y integer NOT NULL, ADD COLUMN z boolean NOT NULL;";
    let a = analysis(sql);
    assert_eq!(a.tables[0].layout_signature, "f8d,f1c,-,f2s,f4i,f1c");
    let base = baseline(0.0, &[("t", "f8d,f1c,f2s")]);
    let outcome = evaluate(&a, None, false, Some(&base));
    assert!(outcome.exceeded, "{:?}", outcome.verdicts);
    let block = &outcome.blocks["t"];
    assert_eq!(block.columns, vec!["x", "y", "z"]);
    assert_eq!((block.committed_slots, block.prefix_columns), (3, 2));
    assert_eq!(block.suggested_order, vec!["z", "x", "y"]);
    assert_eq!(block.avoidable_bytes_per_row, 8.0);
}

#[test]
fn empty_scan_counts_as_degradation() {
    let mut with_note = analysis("CREATE TABLE t (a bigint NOT NULL);");
    with_note.notes.push(crate::fold::Note::empty_scan("migrations"));
    let lenient = evaluate(&with_note, Some(0.0), false, None);
    assert_eq!(lenient.empty_scans, 1);
    assert!(!lenient.exceeded);
    let strict = evaluate(&with_note, Some(0.0), true, None);
    assert!(strict.exceeded);
}

#[test]
fn outcome_degraded_mirrors_the_fail_on_degraded_condition() {
    let clean = evaluate(&analysis(WASTEFUL), Some(100.0), false, None);
    assert!(!clean.degraded());
    let mut with_note = analysis(WASTEFUL);
    with_note.notes.push(crate::fold::Note::empty_scan("migrations"));
    let lenient = evaluate(&with_note, Some(100.0), false, None);
    assert!(lenient.degraded(), "an empty scan is degradation");
    assert!(!lenient.exceeded);
    let strict = evaluate(&with_note, Some(100.0), true, None);
    assert!(strict.exceeded, "fail_on_degraded escalates exactly degraded()");
}

#[test]
fn analysis_degraded_agrees_with_gate_outcome() {
    // The Analysis-level twin must never diverge from the gate's own degraded() — across a clean
    // run, an empty scan (note-driven), and an incomplete table (flag-driven).
    let clean = analysis(WASTEFUL);
    assert!(!clean.degraded());
    let mut empty = analysis(WASTEFUL);
    empty.notes.push(crate::fold::Note::empty_scan("migrations"));
    assert!(empty.degraded(), "an empty scan is degradation");
    let incomplete = analysis("CREATE TABLE t (a int NOT NULL, a bigint NOT NULL);");
    assert!(incomplete.degraded(), "a duplicate-column table is left incomplete");
    for a in [&clean, &empty, &incomplete] {
        assert_eq!(
            a.degraded(),
            evaluate(a, Some(100.0), false, None).degraded(),
            "{:#?}",
            a.notes
        );
    }
}

// Display promises the serde `verdict` tag; a divergence would let logs and JSON name the
// same verdict differently.
#[cfg(feature = "serde")]
#[test]
fn verdict_display_matches_serde_tag() {
    // Every variant — extend when the enum grows.
    let all = [
        TableVerdict::Pass,
        TableVerdict::Incomplete,
        TableVerdict::NewViolation { avoidable: 1.0 },
        TableVerdict::BlockNotDominanceOptimal {
            avoidable: 2.0,
            appended: 3,
        },
        TableVerdict::ModifiedSinceBaseline { avoidable: 2.0 },
    ];
    for verdict in all {
        let json = serde_json::to_value(verdict).unwrap();
        assert_eq!(json["verdict"].as_str().unwrap(), verdict.to_string(), "{verdict:?}");
    }
}

mod fractional_fail_over {
    use super::*;

    /// Fixed columns interleaved among varlenas: grouping them first dominates (never worse in
    /// any realization) and can save up to 10 B/row, which is the gated number.
    const VARLENA_WASTE: &str =
        "CREATE TABLE t (a text NOT NULL, tag int4 NOT NULL, b text NOT NULL, score float8 NOT NULL);";

    #[test]
    fn dominance_avoidable_gates_against_fractional_fail_over() {
        let a = analysis(VARLENA_WASTE);
        assert_eq!(a.tables[0].avoidable_bytes_per_row, 10.0);
        assert_eq!(a.tables[0].avoidable_dominance, 10);
        let strict = evaluate(&a, Some(0.0), false, None);
        assert!(strict.exceeded);
        assert_eq!(strict.verdicts["t"], TableVerdict::NewViolation { avoidable: 10.0 });
        let lenient = evaluate(&a, Some(10.0), false, None);
        assert!(!lenient.exceeded);
        let fractional_gate = evaluate(&a, Some(9.5), false, None);
        assert!(fractional_gate.exceeded, "fail-over accepts fractions");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn old_files_with_allowances_load_and_the_allowance_is_ignored() {
        // A file from before block gating: its byte allowance no longer decides anything, the
        // gate says so, and the next write drops it.
        let a = analysis(VARLENA_WASTE);
        let sig = a.tables[0].layout_signature.as_str();
        let base: Baseline = serde_json::from_str(&format!(
            "{{\"rowdiet\":\"old\",\"fail_over\":0,\"tables\":{{\"t\":{{\"bytes\":9.5,\"layout\":\"{sig}\"}}}}}}"
        ))
        .unwrap();
        assert_eq!(base.tables["t"].legacy_bytes, Some(9.5));
        let outcome = evaluate(&a, None, false, Some(&base));
        assert!(
            !outcome.exceeded,
            "the committed layout passes whatever the allowance said"
        );
        assert_eq!(outcome.verdicts["t"], TableVerdict::Pass);
        assert_eq!(outcome.ignored_allowances, vec!["t".to_string()]);
        let written = serde_json::to_string(&base).unwrap();
        assert!(!written.contains("bytes"), "{written}");
    }
}
