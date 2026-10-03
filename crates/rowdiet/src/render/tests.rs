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

fn baselined(analysis: &Analysis, entries: &[(&str, &str)]) -> GateOutcome {
    let base = Baseline {
        rowdiet: "test".into(),
        fail_over: 0.0,
        tables: entries
            .iter()
            .map(|(name, layout)| {
                (
                    name.to_string(),
                    BaselineEntry::new(baseline::CommittedLayout::parse(layout).unwrap()),
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
    let committed = text(&analysis, None, false, &baselined(&analysis, &[("account", sig)]));
    assert!(
        !committed.contains("FAIL"),
        "a committed layout is not judged again: {committed}"
    );
    let modified = text(&analysis, None, false, &baselined(&analysis, &[("account", "f16c")]));
    assert!(modified.contains("✗ modified since baseline"), "{modified}");
    assert!(modified.contains("--accept account"), "{modified}");
    let orphanish = text(
        &analysis,
        None,
        false,
        &baselined(&analysis, &[("account", sig), ("ghost", "vi")]),
    );
    assert!(
        orphanish.contains("orphaned entries (no matching table): ghost"),
        "{orphanish}"
    );
}

/// (bool, bigint) appended to a committed (int, bigint): the block order is printed, the
/// prefix stays.
fn grown() -> Analysis {
    analyze(
        "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);
         ALTER TABLE t ADD COLUMN e boolean NOT NULL;
         ALTER TABLE t ADD COLUMN f bigint NOT NULL;
         ALTER TABLE t ADD COLUMN g boolean NOT NULL;
         ALTER TABLE t ADD COLUMN h bigint NOT NULL;",
    )
}

#[test]
fn a_wasteful_block_prints_its_block_order() {
    let analysis = grown();
    let rendered = text(&analysis, None, false, &baselined(&analysis, &[("t", "f4i,f8d")]));
    assert!(
        rendered.contains(
            "✗ appended block (V1__init.sql:2, 4 column(s) after a committed prefix of 2) is not dominance-optimal"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("block order: f, h, e, g → 8.0 B/row avoidable"),
        "{rendered}"
    );
    assert!(
        rendered.contains("FAIL: 1 appended block(s) not dominance-optimal"),
        "{rendered}"
    );
    let accepted = text(
        &analysis,
        None,
        false,
        &baselined(&analysis, &[("t", &analysis.tables[0].layout_signature)]),
    );
    assert!(!accepted.contains("FAIL"), "{accepted}");
}

#[test]
fn an_optimal_block_says_so() {
    let analysis = analyze(
        "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);
         ALTER TABLE t ADD COLUMN c bigint NOT NULL, ADD COLUMN d integer NOT NULL;",
    );
    let rendered = text(&analysis, None, false, &baselined(&analysis, &[("t", "f4i,f8d")]));
    assert!(
        rendered.contains("✓ appended block (V1__init.sql:2, 2 column(s) after a committed prefix of 2): no dominating block order exists"),
        "{rendered}"
    );
    assert!(!rendered.contains("FAIL"), "{rendered}");
}

#[test]
fn legacy_allowances_are_reported_as_ignored() {
    let analysis = sample();
    let mut entry = BaselineEntry::committing(&analysis.tables[0]);
    entry.legacy_bytes = Some(8.0);
    let base = Baseline {
        rowdiet: "old".into(),
        fail_over: 0.0,
        tables: [("account".to_string(), entry)].into_iter().collect(),
    };
    let rendered = text(
        &analysis,
        None,
        false,
        &baseline::evaluate(&analysis, None, false, Some(&base)),
    );
    assert!(
        rendered.contains("baseline: byte allowances ignored for account"),
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
    let grown = grown();
    let block = github(&grown, &baselined(&grown, &[("t", "f4i,f8d")]));
    assert!(
        block.starts_with("::error file=V1__init.sql,line=2,title=rowdiet block-not-dominance-optimal::table t: appended block e, f, g, h"),
        "{block}"
    );
    assert!(block.contains("block order: f, h, e, g"), "{block}");
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
    let grown = grown();
    let baselined_summary = github_step_summary(&grown, &baselined(&grown, &[("t", "f4i,f8d")]));
    assert!(
        baselined_summary.contains("**block not dominance-optimal** (block order: f, h, e, g)"),
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
    assert!(
        rendered.contains("pass --fail-on-budgeted to gate on this"),
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
    assert!(!rendered.contains("pass --fail-on-budgeted"), "{rendered}");
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

#[test]
fn block_lines_print_no_workflow_command() {
    // The stack review's D2-3b/c: a block column and a baseline key that each carry a newline
    // and a workflow command.
    let analysis = analyze(
        "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);
         ALTER TABLE t ADD COLUMN \"x\n::error file=x.sql,line=1::rowdiet passed\" boolean NOT NULL;
         ALTER TABLE t ADD COLUMN f bigint NOT NULL;
         ALTER TABLE t ADD COLUMN g boolean NOT NULL;
         ALTER TABLE t ADD COLUMN h bigint NOT NULL;",
    );
    let mut base = Baseline {
        rowdiet: "test".into(),
        fail_over: 0.0,
        tables: [(
            "t".to_string(),
            BaselineEntry::new(baseline::CommittedLayout::parse("f4i,f8d").unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let mut ghost = BaselineEntry::new(baseline::CommittedLayout::parse("f8d").unwrap());
    ghost.legacy_bytes = Some(1.0);
    base.tables
        .insert("ghost\n::warning file=y.sql,line=1::injected".to_string(), ghost);
    let outcome = baseline::evaluate(&analysis, None, false, Some(&base));
    let rendered = text(&analysis, None, false, &outcome);
    assert!(rendered.contains("block order: f, h, x\\n::error"), "{rendered}");
    assert!(rendered.contains("ghost\\n::warning"), "{rendered}");
    let commands = rendered.lines().filter(|l| l.trim_start().starts_with("::")).count();
    assert_eq!(commands, 0, "{rendered}");
}

#[test]
fn a_block_from_two_migrations_names_both() {
    let analysis = analyze_sources(
        &[
            SqlSource::new("V1__t.sql", "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);"),
            SqlSource::new("V2__u.sql", "ALTER TABLE t ADD COLUMN e boolean NOT NULL;"),
            SqlSource::new("V3__w.sql", "ALTER TABLE t ADD COLUMN f bigint NOT NULL;"),
        ],
        &Config::default(),
    );
    let rendered = text(&analysis, None, false, &baselined(&analysis, &[("t", "f4i,f8d")]));
    assert!(
        rendered.contains("appended block (V2__u.sql:1, V3__w.sql:1, 2 column(s) after a committed prefix of 2)"),
        "{rendered}"
    );
}

#[test]
fn drop_then_add_reports_the_block_behind_the_dropped_slot() {
    let analysis = analyze(
        "CREATE TABLE t (id bigint NOT NULL, flag boolean NOT NULL, n smallint NOT NULL);
         ALTER TABLE t DROP COLUMN n;
         ALTER TABLE t ADD COLUMN x smallint NOT NULL, ADD COLUMN y integer NOT NULL, ADD COLUMN z boolean NOT NULL;",
    );
    let rendered = text(&analysis, None, false, &baselined(&analysis, &[("t", "f8d,f1c,f2s")]));
    assert!(
        rendered.contains("3 column(s) after a committed prefix of 2 and 1 dropped slot(s)) is not dominance-optimal"),
        "{rendered}"
    );
    assert!(
        rendered.contains("block order: z, x, y → 8.0 B/row avoidable"),
        "{rendered}"
    );
}

#[test]
fn a_block_order_cell_renders_names_as_text() {
    let analysis = analyze(
        "CREATE TABLE t (a int NOT NULL, b bigint NOT NULL);
         ALTER TABLE t ADD COLUMN \"<img src=x>\" boolean NOT NULL;
         ALTER TABLE t ADD COLUMN f bigint NOT NULL;
         ALTER TABLE t ADD COLUMN g boolean NOT NULL;
         ALTER TABLE t ADD COLUMN h bigint NOT NULL;",
    );
    let summary = github_step_summary(&analysis, &baselined(&analysis, &[("t", "f4i,f8d")]));
    let cell = summary
        .lines()
        .find(|l| l.contains("block not dominance-optimal"))
        .expect("a block row");
    assert!(cell.contains("\\<img src=x\\>"), "{cell}");
}

#[test]
fn a_frontier_prints_the_query_that_settles_it() {
    let analysis = analyze("CREATE TABLE t (t text NOT NULL, m macaddr NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(
        rendered.contains("settle it on rows like yours (pageinspect, superuser):"),
        "{rendered}"
    );
    assert!(
        rendered.contains("               WITH RECURSIVE rel AS ("),
        "{rendered}"
    );
    assert!(
        rendered.contains("bytes_saved totals current minus alternative"),
        "{rendered}"
    );
}

#[test]
fn a_table_name_cannot_close_the_summary_fence() {
    // The stack review's D2-1: a name with a newline and a fence line rendered HTML outside the
    // code block.
    let analysis = analyze(
        "CREATE TABLE \"x\n```\n<img src=https://example.invalid/p.png>\" (t text NOT NULL, m macaddr NOT NULL);",
    );
    let summary = github_step_summary(&analysis, &gate(&analysis, Some(0.0)));
    let section = &summary[summary.find("### Settling").expect("a settling section")..];
    assert!(section.contains("\\<img"), "the heading escapes the name: {section}");
    let fences: Vec<&str> = section.lines().filter(|l| l.starts_with("```")).collect();
    assert_eq!(
        fences,
        vec!["````sql", "````"],
        "a fence longer than the name's backticks: {section}"
    );
    let open = section.find("````sql").unwrap();
    let close = section[open + 7..].find("\n````").unwrap() + open + 7;
    let outside = format!("{}{}", &section[..open], &section[close..]);
    assert!(!outside.replace("\\<", "").contains("<img"), "{outside}");
}

#[test]
fn a_settling_query_prints_no_workflow_command() {
    let analysis =
        analyze("CREATE TABLE \"q\n::error::QUERY ##[error]BRACKET\" (t text NOT NULL, m macaddr NOT NULL);");
    let rendered = text(&analysis, None, false, &gate(&analysis, Some(0.0)));
    assert!(
        rendered.contains("c.relname = U&'q\\000A::error::QUERY ##\\005Berror]BRACKET'"),
        "{rendered}"
    );
    assert!(!rendered.contains("##["), "{rendered}");
    assert!(
        !rendered.lines().any(|l| l.trim_start().starts_with("::")),
        "{rendered}"
    );
}
