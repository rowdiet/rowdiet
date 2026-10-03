use super::*;
use rowdiet_core::{Baseline, BaselineEntry, Config, SqlSource, analyze_sources, baseline};

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
    analyze(
        "CREATE TABLE account (active boolean NOT NULL, id bigint PRIMARY KEY, kind smallint NOT NULL, balance bigint NOT NULL);",
    )
}

fn gate(analysis: &Analysis, fail_over: Option<f64>) -> GateOutcome {
    baseline::evaluate(analysis, fail_over, false, None)
}

fn baselined(analysis: &Analysis, entries: &[(&str, f64, &str)]) -> GateOutcome {
    let base = Baseline {
        rowdiet: "test".into(),
        fail_over: 0.0,
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
    baseline::evaluate(analysis, None, false, Some(&base))
}

#[test]
fn text_report_mentions_the_essentials() {
    let analysis = sample();
    let rendered = text(&analysis, Some(1_000_000), true, &gate(&analysis, Some(0.0)));
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
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(rendered.contains("✓ ok"));
    assert!(rendered.contains("optimal: zero padding"));
    assert!(!rendered.contains("FAIL:"));
}

#[test]
fn empty_modeled_tables_are_not_called_optimal() {
    let analysis = analyze("CREATE TABLE c PARTITION OF elsewhere FOR VALUES FROM (1) TO (2);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
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
    let regressed = text(&analysis, None, false, &baselined(&analysis, &[("account", 4.0, sig)]));
    assert!(
        regressed.contains("✗ regression: 8.0 B/row exceeds the baselined allowance of 4"),
        "{regressed}"
    );
    assert!(regressed.contains("FAIL: 1 regression(s) vs baseline"), "{regressed}");
    let modified = text(
        &analysis,
        None,
        false,
        &baselined(&analysis, &[("account", 8.0, "f16c")]),
    );
    assert!(modified.contains("✗ modified since baseline"), "{modified}");
    assert!(modified.contains("--accept account"), "{modified}");
    let ratchet = text(&analysis, None, false, &baselined(&analysis, &[("account", 12.0, sig)]));
    assert!(
        ratchet.contains("↓ ratchet: allowance 12 can tighten to 8"),
        "{ratchet}"
    );
    assert!(!ratchet.contains("FAIL"), "{ratchet}");
    let orphanish = text(
        &analysis,
        None,
        false,
        &baselined(&analysis, &[("account", 8.0, sig), ("ghost", 1.0, "vi")]),
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
    let rendered = text(&analysis, None, false, &baselined(&analysis, &[("t", 0.0, "f4i,f8d")]));
    assert!(rendered.contains("✗ grown since baseline"), "{rendered}");
    assert!(
        rendered.contains("grown since baseline") && rendered.contains("FAIL:"),
        "{rendered}"
    );
}

#[test]
fn github_annotations() {
    let analysis = sample();
    let rendered = github(&analysis, &gate(&analysis, Some(0.0)));
    assert!(rendered.starts_with("::error file=V1__init.sql,line=1,title=rowdiet::table account"));
    let warn = github(&analysis, &gate(&analysis, None));
    assert!(warn.starts_with("::warning "));
    let sig = &analysis.tables[0].layout_signature;
    let regressed = github(&analysis, &baselined(&analysis, &[("account", 4.0, sig)]));
    assert!(
        regressed.starts_with("::error file=V1__init.sql,line=1,title=rowdiet regression::"),
        "{regressed}"
    );
}

#[test]
fn json_shape() {
    let analysis = sample();
    let rendered = json(&analysis, Some(0.0), &gate(&analysis, Some(0.0))).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["gate_exceeded"], true);
    assert_eq!(value["gate"]["exceeded"], true);
    assert_eq!(value["gate"]["verdicts"]["account"]["verdict"], "new_violation");
    assert_eq!(value["gate"]["verdicts"]["account"]["avoidable"], 8.0);
    assert_eq!(value["analysis"]["tables"][0]["avoidable_bytes_per_row"], 8.0);
    assert_eq!(value["analysis"]["tables"][0]["tier"], "exact");
    assert_eq!(value["analysis"]["tables"][0]["layout_signature"], "f1c,f8d,f2s,f8d");
}

#[test]
fn human_units() {
    assert_eq!(human_bytes(999.0), "999 B");
    assert_eq!(human_bytes(10.5), "10.5 B");
    assert_eq!(human_bytes(8_000_000.0), "8.0 MB");
    assert_eq!(human_bytes(12_500_000_000.0), "12.5 GB");
}

#[test]
fn quoting_only_when_needed() {
    assert_eq!(maybe_quote("plain_name2"), "plain_name2");
    assert_eq!(maybe_quote("Mixed"), "\"Mixed\"");
    assert_eq!(maybe_quote("select"), "\"select\"");
    assert_eq!(maybe_quote("user"), "\"user\"");
    assert_eq!(maybe_quote("name"), "name");
    assert_eq!(maybe_quote("1st"), "\"1st\"");
}

#[test]
fn github_escapes_properties_and_messages() {
    let analysis = analyze_sources(
        &[SqlSource {
            name: "V1__a,b:c.sql".into(),
            sql: r#"CREATE TABLE "we%ird" (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);"#
                .into(),
        }],
        &Config::default(),
    );
    let rendered = github(&analysis, &gate(&analysis, Some(0.0)));
    assert!(rendered.contains("file=V1__a%2Cb%3Ac.sql"), "{rendered}");
    assert!(rendered.contains("we%25ird"), "{rendered}");
    assert!(!rendered.contains("we%ird "), "{rendered}");
}

#[test]
fn github_budget_truncates_loudly() {
    let sql: String = (0..13)
        .map(|i| {
            format!("CREATE TABLE t{i:02} (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);\n")
        })
        .collect();
    let analysis = analyze(&sql);
    let rendered = github(&analysis, &gate(&analysis, Some(0.0)));
    assert_eq!(rendered.matches("::error ").count(), 10, "{rendered}");
    assert!(rendered.contains("3 annotation(s) suppressed"), "{rendered}");
    let under = github(&analysis, &gate(&analysis, None));
    assert_eq!(under.matches("::warning ").count(), 10, "{under}");
    assert!(under.contains("suppressed"), "{under}");
}

#[test]
fn github_step_summary_carries_the_full_report() {
    let sql: String = (0..13)
        .map(|i| {
            format!("CREATE TABLE t{i:02} (a int NOT NULL, b bigint NOT NULL, c int NOT NULL, d bigint NOT NULL);\n")
        })
        .collect();
    let analysis = analyze(&sql);
    let summary = github_step_summary(&analysis, &gate(&analysis, Some(0.0)));
    for i in 0..13 {
        assert!(
            summary.contains(&format!("| t{i:02} | 8.0 | - | complete | exact | **new violation** |")),
            "{summary}"
        );
    }
    assert!(
        summary.contains("FAIL: 13 table(s) over the fail-over gate"),
        "{summary}"
    );
    let baselined_summary = github_step_summary(
        &analysis,
        &baselined(&analysis, &[("t00", 4.0, &analysis.tables[0].layout_signature)]),
    );
    assert!(
        baselined_summary.contains("**regression** (allowed 4)"),
        "{baselined_summary}"
    );
}

#[test]
fn an_unverified_payload_model_reads_as_found_everywhere() {
    let analysis = analyze("CREATE TABLE i (a inet NOT NULL, b smallint NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(
        rendered.contains("no dominating reorder found (payload lengths unverified for inet)"),
        "{rendered}"
    );
    assert!(!rendered.contains("exists"), "{rendered}");
    let summary = github_step_summary(&analysis, &gate(&analysis, Some(0.0)));
    assert!(summary.contains("complete (payload model unverified)"), "{summary}");
    let json = json(&analysis, Some(0.0), &gate(&analysis, Some(0.0))).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["analysis"]["tables"][0]["dominance_search"], "superset");
    assert_eq!(value["analysis"]["tables"][0]["superset_types"][0], "inet");
}

fn command_lines(rendered: &str) -> Vec<&str> {
    rendered
        .split(['\n', '\r'])
        .filter(|line| line.trim_start().starts_with("::") || line.contains("##["))
        .collect()
}

#[test]
fn text_identifiers_cannot_open_workflow_commands() {
    let analysis = analyze(concat!(
        "CREATE TABLE \"evil\n::error file=README.md,line=1::X\" (a boolean NOT NULL, ",
        "\"c\n::stop-commands::tok\" bigint NOT NULL, \"x\r\n::add-mask::secret\" boolean NOT NULL, ",
        "\"y ##[error]v1\" bigint NOT NULL);\n",
        "CREATE TABLE n (a \"t\n::error::TYPE\" NOT NULL);\n",
        "ALTER TABLE \"ghost\r::warning::G\" ADD COLUMN z int;",
    ));
    let rendered = text(&analysis, None, true, &gate(&analysis, Some(0.0)));
    assert!(command_lines(&rendered).is_empty(), "{rendered}");
    assert!(!rendered.contains('\r'), "{rendered}");
    assert!(
        rendered.contains(r#"■ "evil\n::error file=README.md,line=1::X" (V1__init.sql:1)"#),
        "{rendered}"
    );
    assert!(rendered.contains(r"c\n::stop-commands::tok, "), "{rendered}");
    assert!(
        rendered.contains(r#"U&"x\000D\000A::add-mask::secret" BOOLEAN"#),
        "{rendered}"
    );
    assert!(rendered.contains(r#"U&"y ##\005Berror]v1" BIGINT"#), "{rendered}");
    assert!(
        rendered.contains(r#"type "t\n::error::TYPE" unresolvable"#),
        "{rendered}"
    );
    assert!(rendered.contains(r#"ALTER TABLE "ghost\r::warning::G""#), "{rendered}");
}

#[test]
fn a_source_path_cannot_open_a_note_line_as_a_command() {
    let analysis = analyze_sources(
        &[SqlSource {
            name: " ::error::x.sql".into(),
            sql: "CREATE TABLE t (a mystery NOT NULL);".into(),
        }],
        &Config::default(),
    );
    let rendered = text(&analysis, None, false, &gate(&analysis, None));
    assert!(command_lines(&rendered).is_empty(), "{rendered}");
    assert!(
        rendered.contains(r"   \u{3a}:error::x.sql:1 [unknown-type]"),
        "{rendered}"
    );
}

#[test]
fn escaping_leaves_ordinary_names_alone() {
    assert!(matches!(
        escape_text("Mixed Case ünïcode #[x] ## [y]"),
        Cow::Borrowed(_)
    ));
    assert_eq!(
        escape_text("a\tb\u{1b}[31m\u{85}\u{2028}"),
        r"a\tb\u{1b}[31m\u{85}\u{2028}"
    );
    assert_eq!(escape_text("###[x]"), r"###\u{5b}x]");
    assert_eq!(maybe_quote("we\"ird"), "\"we\"\"ird\"");
    assert_eq!(maybe_quote("a\\b\nc\""), r#"U&"a\\b\000Ac""""#);
}

#[test]
fn step_summary_cells_hold_one_line() {
    let analysis =
        analyze("CREATE TABLE \"a\r|b\" (x int NOT NULL, y bigint NOT NULL, z int NOT NULL, w bigint NOT NULL);");
    let summary = github_step_summary(&analysis, &gate(&analysis, Some(0.0)));
    assert!(!summary.contains('\r'), "{summary}");
    assert!(summary.contains(r#"| "a \|b" | 8.0 |"#), "{summary}");
}

#[test]
fn json_keeps_names_exact_without_the_bracket_prefix() {
    let analysis = analyze("CREATE TABLE \"t ##[error]x\n\" (a boolean NOT NULL, b bigint NOT NULL);");
    let rendered = json(&analysis, None, &gate(&analysis, None)).unwrap();
    assert!(!rendered.contains("##["), "{rendered}");
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["analysis"]["tables"][0]["name"], "t ##[error]x\n");
}

#[test]
fn suggested_ddl_keeps_hostile_table_and_type_names() {
    let analysis = analyze(concat!(
        "CREATE DOMAIN \"dom\n::error::D\" AS bigint;\n",
        "CREATE TABLE \"t\n::error::T\" (a boolean NOT NULL, b \"dom\n::error::D\" NOT NULL, ",
        "c boolean NOT NULL, d bigint NOT NULL);",
    ));
    let rendered = text(&analysis, None, true, &gate(&analysis, None));
    assert!(command_lines(&rendered).is_empty(), "{rendered}");
    assert!(
        rendered.contains(r#"  CREATE TABLE U&"t\000A::error::T" ("#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"      b U&"dom\000A::error::D" NOT NULL,"#),
        "{rendered}"
    );
}

#[test]
fn a_backslash_prints_doubled_so_it_never_reads_as_an_escape() {
    let analysis = analyze("CREATE TABLE \"lit\\n\" (a int NOT NULL); CREATE TABLE \"real\n\" (a int NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, None));
    assert!(rendered.contains(r#"✓ "lit\\n" "#), "{rendered}");
    assert!(rendered.contains(r#"✓ "real\n" "#), "{rendered}");
}

#[test]
fn sql_spelling_quotes_only_what_needs_it() {
    assert_eq!(sql_spelling(r#"public."A""b""#).as_deref(), Some(r#"public."A""b""#));
    assert_eq!(sql_spelling("\"a\nb\"[]").as_deref(), Some(r#"U&"a\000Ab"[]"#));
    assert_eq!(sql_spelling("\"x##[y\"").as_deref(), Some(r#"U&"x##\005By""#));
    assert_eq!(sql_spelling("bare\u{2028}name"), None);
    assert_eq!(sql_spelling("\"open"), None);
}

#[test]
fn a_capped_search_never_prints_a_checkmark_over_certain_padding() {
    let mut analysis = analyze("CREATE TABLE f (a boolean NOT NULL, b bigint NOT NULL);");
    let t = &mut analysis.tables[0];
    assert_eq!(t.current.padding, 7);
    t.avoidable_bytes_per_row = 0.0;
    t.search_scope = SearchScope::SortOnly;
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(rendered.starts_with("◐ f "), "{rendered}");
    assert!(!rendered.contains("nothing to gain"), "{rendered}");
    assert!(
        rendered.contains("7 B padding; no order the capped search tried saves a footprint rung (search capped: heuristic orders only)"),
        "{rendered}"
    );
}

#[test]
fn step_summary_cells_render_no_html_or_markdown_from_names() {
    let sql = [
        include_str!("../../tests/fixtures/markdown/V1__md.sql"),
        include_str!("../../tests/fixtures/markdown/V2__img.sql"),
        "CREATE TABLE d (a int, \"<img src=https://example.invalid/d.png>\" text);\n\
         ALTER TABLE d DROP COLUMN \"<img src=https://example.invalid/d.png>\";",
    ]
    .concat();
    let analysis = analyze(&sql);
    let summary = github_step_summary(&analysis, &gate(&analysis, Some(0.0)));
    let unescaped = summary.replace("\\<", "");
    for raw in ["<img", "<a ", "<b>", "```sql", "**rowdiet"] {
        assert!(!unescaped.contains(raw), "{raw} reached the summary:\n{summary}");
    }
    assert!(
        summary.contains(r"frontier: \<img src=https://example.invalid/a.png\>"),
        "{summary}"
    );
    assert!(
        summary.contains(r"column \<img src=https://example.invalid/d.png\> dropped"),
        "{summary}"
    );
}

#[test]
fn exact_tier_with_nulls_prints_both_row_scenarios() {
    let analysis = analyze(
        "CREATE TABLE t (b1 boolean NOT NULL, b2 boolean NOT NULL, n integer, z timetz NOT NULL);
         CREATE TABLE cols9 (c1 int8,c2 int8,c3 int8,c4 int8,c5 int8,c6 int8,c7 int8,c8 int8,c9 int8);",
    );
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(
        rendered.contains("current  : 2 B padding, 48 B/row footprint, 157 rows/8kB page without NULLs; 2-6 B padding, 48 B/row with NULLs (24 B header)"),
        "{rendered}"
    );
    assert!(rendered.contains("saves 0-8 B/row in every realization"), "{rendered}");
    assert!(
        rendered.contains("NULLs move later offsets in: n (NOT NULL removes that variable)"),
        "{rendered}"
    );
    assert!(rendered.contains(EXACT_NULLS_LABEL), "{rendered}");
    assert!(
        rendered.contains("✓ cols9 (V1__init.sql:2) — optimal: zero padding in every NULL pattern; 0 B padding, 96 B/row footprint, 81 rows/8kB page without NULLs; 0 B padding, 32-96 B/row with NULLs (32 B header)"),
        "{rendered}"
    );
}

#[test]
fn frontier_prints_the_rows_without_nulls() {
    let analysis = analyze("CREATE TABLE t (m macaddr, t text NOT NULL, s smallint NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(rendered.contains("frontier : m, s, t"), "{rendered}");
    assert!(
        rendered.contains("in rows without NULLs the alternative wins (saves 0-3 B/row)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("depends on payload lengths mod 8 and which columns hold NULL"),
        "a NULL variable is named as what decides: {rendered}"
    );
    assert!(!rendered.contains("FAIL:"), "a frontier never gates: {rendered}");
}

#[test]
fn an_exact_tier_frontier_names_nulls_and_row_size() {
    // Four nullable fixed columns: the reorder saves a row-size rung when every column is
    // stored and loses one in some NULL patterns, so it is a frontier decided by NULLs alone.
    let analysis = analyze("CREATE TABLE t (c0 macaddr, c1 macaddr, c2 boolean, c3 smallint);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(rendered.contains("frontier : c0, c3, c1, c2"), "{rendered}");
    assert!(
        rendered.contains("winner in row size depends on which columns hold NULL (-8 to 8 B/row)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("in rows without NULLs the alternative wins in row size (saves 8-8 B/row)"),
        "{rendered}"
    );
    assert!(!rendered.contains("payload"), "no varlena here: {rendered}");
}

#[test]
fn estimate_line_shows_the_no_null_range_when_it_differs() {
    let analysis = analyze("CREATE TABLE t (s smallint NOT NULL, n smallint, i integer NOT NULL, t text NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(
        rendered.contains(
            "current  : 0.0 B/row expected padding (0 B deterministic, range 0-2, 0-0 without NULLs, data-dependent)"
        ),
        "{rendered}"
    );
    assert!(rendered.contains(ESTIMATE_LABEL), "{rendered}");
    assert!(ESTIMATE_LABEL.contains("NULL"));
}

#[test]
fn a_budgeted_search_is_counted_and_can_fail_the_gate() {
    let cols: Vec<String> = (0..12).map(|i| format!("t{i} text, i{i} integer NOT NULL")).collect();
    let analysis = analyze(&format!("CREATE TABLE wide ({});", cols.join(", ")));
    assert!(
        analysis.tables[0].budgeted(),
        "{:?}",
        analysis.tables[0].dominance_search
    );
    let mut outcome = gate(&analysis, Some(1000.0));
    assert_eq!(outcome.budgeted_tables, 1);
    assert!(!outcome.degraded(), "a budget is not a parse degradation");
    let rendered = text(&analysis, None, false, &outcome);
    assert!(
        rendered.contains("budgeted: 1 table(s) where the dominance search hit its budget"),
        "{rendered}"
    );
    assert!(!outcome.exceeded);
    outcome.fail_on_budgeted();
    assert!(outcome.exceeded, "--fail-on-budgeted fails on a budgeted search");
    let rendered = text(&analysis, None, false, &outcome);
    assert!(
        rendered.contains("FAIL: 1 table(s) with a budgeted dominance search (--fail-on-budgeted)"),
        "{rendered}"
    );
    assert!(!rendered.contains("--fail-on-degraded)"), "{rendered}");
}

#[test]
fn a_budgeted_clean_table_that_can_pad_is_not_checked_off() {
    // Certain padding left behind a budgeted dominance search, as behind a capped order search.
    let analysis = analyze(
        "CREATE TABLE g990 (c0 int2, c1 varchar(20) NOT NULL, c2 int8, c3 text, c4 timestamptz, \
         c5 uuid, c6 int2, c7 int4);",
    );
    let t = &analysis.tables[0];
    assert!(t.budgeted() && t.avoidable_bytes_per_row == 0.0 && t.current.padding > 0);
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(rendered.starts_with("◐ g990"), "{rendered}");
}

#[test]
fn json_carries_the_null_model() {
    let analysis = analyze("CREATE TABLE t (b1 boolean NOT NULL, b2 boolean NOT NULL, n integer, z timetz NOT NULL);");
    let rendered = json(&analysis, Some(0.0), &gate(&analysis, Some(0.0))).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let t = &value["analysis"]["tables"][0];
    assert_eq!(t["null_variables"], serde_json::json!(["n"]));
    assert_eq!(
        t["current"]["with_nulls"],
        serde_json::json!({"t_hoff": 24, "footprint_min": 48, "footprint_max": 48})
    );
    assert_eq!(t["current"]["without_nulls"], serde_json::json!({"min": 2, "max": 2}));
    assert_eq!(t["current"]["padding"], 2);
    assert_eq!(
        (t["current"]["padding_min"].clone(), t["current"]["padding_max"].clone()),
        (2.into(), 6.into())
    );
    assert_eq!(t["dominance_saving"], serde_json::json!({"min": 0, "max": 8}));
}

#[test]
fn null_variable_names_print_escaped() {
    let analysis = analyze("CREATE TABLE t (\"n\n::error::NULLVAR\" smallint, i integer NOT NULL, x text NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(
        rendered.contains("NULLs move later offsets in: n\\n::error::NULLVAR"),
        "{rendered}"
    );
    assert!(
        !rendered.lines().any(|l| l.trim_start().starts_with("::")),
        "{rendered}"
    );
}
