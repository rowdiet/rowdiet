//! Output renderers: human text, GitHub Actions annotations, JSON.

use rowdiet_core::dominance::Measure;
use rowdiet_core::layout::SearchScope;
use rowdiet_core::report::{BandWinner, BlockFinding, DominanceScope, Frontier};
use rowdiet_core::{Analysis, ColumnReport, GateOutcome, NoteKind, OrderStats, TableReport, TableVerdict, Tier};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub fn text(analysis: &Analysis, rows: Option<u64>, suggest: bool, gate: &GateOutcome) -> String {
    let mut out = String::new();
    for table in &analysis.tables {
        render_table(&mut out, table, rows, suggest, gate);
    }
    if !analysis.notes.is_empty() {
        let _ = writeln!(out, "notes:");
        for note in &analysis.notes {
            // Origin's Display prints the bare source for path-level notes (line 0).
            let _ = writeln!(
                out,
                "  {} [{}] {}",
                escape_line_start(&note.origin.to_string()),
                kind_label(note.kind),
                escape_text(&note.detail)
            );
        }
    }
    let wasteful = analysis
        .tables
        .iter()
        .filter(|t| !t.ignored && t.avoidable_bytes_per_row > 0.0)
        .count();
    let skipped = analysis
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::SkippedStatement)
        .count();
    let _ = writeln!(
        out,
        "{} table(s) analyzed — {wasteful} with avoidable waste, {skipped} statement(s) skipped",
        analysis.tables.len()
    );
    render_gate_summary(&mut out, gate);
    out
}

fn render_gate_summary(out: &mut String, gate: &GateOutcome) {
    let mut new_violations = 0u32;
    let mut blocks = 0u32;
    let mut modified = 0u32;
    for verdict in gate.verdicts.values() {
        match verdict {
            TableVerdict::NewViolation { .. } => new_violations += 1,
            TableVerdict::BlockNotDominanceOptimal { .. } => blocks += 1,
            TableVerdict::ModifiedSinceBaseline { .. } => modified += 1,
            // Counted in gate.incomplete_tables and reported on the degraded line below.
            TableVerdict::Pass | TableVerdict::Incomplete => {}
        }
    }
    if gate.degraded() {
        let _ = writeln!(
            out,
            "degraded: {} statement(s) skipped, {} table(s) incomplete, {} path(s) matched no SQL files — pass --fail-on-degraded to gate on this",
            gate.skipped_statements, gate.incomplete_tables, gate.empty_scans
        );
    }
    if gate.budgeted_tables > 0 {
        let _ = writeln!(
            out,
            "budgeted: {} table(s) where the dominance search hit its budget; findings there stand, clean verdicts cover the searched candidates only — pass --fail-on-budgeted to gate on this",
            gate.budgeted_tables
        );
    }
    if gate.exceeded {
        let mut parts = Vec::new();
        if new_violations > 0 {
            parts.push(format!("{new_violations} table(s) over the fail-over gate"));
        }
        if blocks > 0 {
            parts.push(format!("{blocks} appended block(s) not dominance-optimal"));
        }
        if modified > 0 {
            parts.push(format!("{modified} modified since baseline"));
        }
        if parts.is_empty() {
            parts.push("degraded analysis (--fail-on-degraded)".to_string());
        }
        let _ = writeln!(out, "FAIL: {}", parts.join(", "));
    }
    if !gate.ignored_allowances.is_empty() {
        let _ = writeln!(
            out,
            "baseline: byte allowances ignored for {} (appended blocks are judged by dominance now; \
             --update-baseline or --accept drops them)",
            escape_text(&gate.ignored_allowances.join(", "))
        );
    }
    if !gate.orphaned.is_empty() {
        let _ = writeln!(
            out,
            "baseline: orphaned entries (no matching table): {}",
            escape_text(&gate.orphaned.join(", "))
        );
    }
    if !gate.expired.is_empty() {
        let _ = writeln!(
            out,
            "baseline: expired entries (layout changed, table within fail-over): {}",
            escape_text(&gate.expired.join(", "))
        );
    }
}

fn render_table(out: &mut String, t: &TableReport, rows: Option<u64>, suggest: bool, gate: &GateOutcome) {
    let verdict = gate.verdicts.get(&t.name).copied();
    let block = gate.blocks.get(&t.name);
    let loc = escape_text(&t.origin.to_string()).into_owned();
    let display = escape_text(&t.display);
    if t.ignored {
        let _ = writeln!(out, "∅ {display} ({loc}) — ignored (rowdiet:ignore)");
        return;
    }
    // A table we could not fully model (unexpanded LIKE/INHERITS/typed table, or a partition
    // child of an unknown parent) has no footprint to compare — report it as not analyzable
    // rather than letting it pass the gate vacuously as "optimal".
    if t.incomplete {
        let detail = if t.natts == 0 {
            "no modeled columns"
        } else {
            "columns not fully known"
        };
        let _ = writeln!(out, "◌ {display} ({loc}) — {detail} — not analyzable");
        render_flags(out, t);
        return;
    }
    if t.avoidable_bytes_per_row == 0.0 {
        // Certain padding a capped search left in place is not a clean result.
        let capped_waste = t.search_scope != SearchScope::Complete && t.current.padding > 0;
        let detail = match (t.tier, t.current.padding) {
            (Tier::Estimate, _) => format!("{}, {}{}", stats_line(&t.current), verdict_phrase(t), scope_note(t)),
            (Tier::Exact, _) if t.current.with_nulls.is_some() && t.current.padding_max == 0 => {
                format!(
                    "optimal: zero padding in every NULL pattern{}; {}",
                    scope_note(t),
                    stats_line(&t.current)
                )
            }
            (Tier::Exact, _) if t.current.with_nulls.is_some() => {
                format!("{}, {}{}", stats_line(&t.current), verdict_phrase(t), scope_note(t))
            }
            (Tier::Exact, 0) => format!("optimal: zero padding{}", scope_note(t)),
            (Tier::Exact, p) if capped_waste => {
                format!(
                    "{p} B padding; no order the capped search tried saves a footprint rung{}",
                    scope_note(t)
                )
            }
            (Tier::Exact, p) => {
                format!(
                    "{p} B padding but footprint unchanged (MAXALIGN rounding) — nothing to gain{}",
                    scope_note(t)
                )
            }
            // Incomplete tables return above as "not analyzable"; unreachable here in practice.
            (Tier::Unknown, _) => "columns not fully known".to_string(),
        };
        let mark = if capped_waste { "◐" } else { "✓" };
        let _ = writeln!(out, "{mark} {display} ({loc}) — {detail} [{}]", tier_label(t));
        render_frontier(out, t);
        render_flags(out, t);
        render_verdict(out, t, verdict, block);
        return;
    }
    let _ = writeln!(
        out,
        "■ {display} ({loc}) — {} columns — {}{}",
        t.natts,
        tier_label(t),
        scope_note(t)
    );
    let _ = writeln!(out, "  current  : {}", stats_line(&t.current));
    let basis = match t.dominance_saving {
        Some(saving) => format!(
            " ({} B deterministic + {} B dominance-proven; saves {}-{} B/row in every realization)",
            t.avoidable_deterministic, t.avoidable_dominance, saving.min, saving.max
        ),
        None => String::new(),
    };
    let _ = writeln!(
        out,
        "  suggested: {} → {:.1} B/row avoidable{basis}",
        stats_line(&t.suggested),
        t.avoidable_bytes_per_row
    );
    let _ = writeln!(out, "  order    : {}", escape_text(&t.suggested_order.join(", ")));
    if let Some(n) = rows {
        // A dominance finding is a range; extrapolating only its maximum would overstate it.
        match t.dominance_saving {
            Some(saving) if saving.min == 0 && saving.max > 0 => {
                let _ = writeln!(
                    out,
                    "  × {n} rows ≈ up to {}",
                    human_bytes(saving.max as f64 * n as f64)
                );
            }
            Some(saving) if saving.min != saving.max => {
                let _ = writeln!(
                    out,
                    "  × {n} rows ≈ {} to {}",
                    human_bytes(saving.min as f64 * n as f64),
                    human_bytes(saving.max as f64 * n as f64)
                );
            }
            _ => {
                let _ = writeln!(
                    out,
                    "  × {n} rows ≈ {}",
                    human_bytes(t.avoidable_bytes_per_row * n as f64)
                );
            }
        }
    }
    render_flags(out, t);
    render_verdict(out, t, verdict, block);
    if suggest {
        render_suggestion(out, t);
    }
}

fn render_verdict(out: &mut String, t: &TableReport, verdict: Option<TableVerdict>, block: Option<&BlockFinding>) {
    if let Some(block) = block {
        render_block(
            out,
            t,
            block,
            matches!(verdict, Some(TableVerdict::BlockNotDominanceOptimal { .. })),
        );
    }
    if let Some(TableVerdict::ModifiedSinceBaseline { .. }) = verdict {
        let _ = writeln!(
            out,
            "  ✗ modified since baseline: the committed layout changed; meet fail-over or re-accept with --accept {}",
            escape_text(&t.name)
        );
    }
}

/// The appended block of a baselined table: the one order still free, judged with the
/// committed prefix in place.
fn render_block(out: &mut String, t: &TableReport, block: &BlockFinding, failing: bool) {
    let dropped = block.committed_slots - block.prefix_columns;
    let appended = format!(
        "{} column(s) after a committed prefix of {}{}",
        block.columns.len(),
        block.prefix_columns,
        if dropped > 0 {
            format!(" and {dropped} dropped slot(s)")
        } else {
            String::new()
        }
    );
    // One location per migration file: the first statement there that appends to the block.
    let mut origins: Vec<String> = Vec::new();
    for (i, origin) in block.origins.iter().enumerate() {
        if block.origins[..i].iter().all(|o| o.source != origin.source) {
            origins.push(origin.to_string());
        }
    }
    let origins = escape_text(&origins.join(", ")).into_owned();
    if block.avoidable_bytes_per_row == 0.0 {
        let claim = match block.dominance_search {
            DominanceScope::Exhaustive => "no dominating block order exists",
            DominanceScope::Budgeted => "no dominating block order found (dominance search budgeted)",
            DominanceScope::Superset => "no dominating block order found (payload lengths unverified)",
        };
        let _ = writeln!(out, "  ✓ appended block ({origins}, {appended}): {claim}");
    } else {
        let mark = if failing { "✗" } else { "↓" };
        let _ = writeln!(
            out,
            "  {mark} appended block ({origins}, {appended}) is not dominance-optimal: reorder it where it is \
             appended (the committed columns stay), or --accept {}",
            escape_text(&t.name)
        );
        let basis = match block.dominance_saving {
            Some(saving) => format!(
                "; saves {}-{} B/row in every realization ({} B deterministic + {} B dominance-proven)",
                saving.min, saving.max, block.avoidable_deterministic, block.avoidable_dominance
            ),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "    block order: {} → {:.1} B/row avoidable{basis}",
            escape_text(&block.suggested_order.join(", ")),
            block.avoidable_bytes_per_row
        );
    }
    if let Some(frontier) = &block.frontier {
        render_frontier_body(out, t, frontier, "    block frontier");
    }
}

/// The clean-table phrase: an exhaustive sweep proves absence, a budgeted one or one over an
/// unverified payload model only reports it.
fn verdict_phrase(t: &TableReport) -> String {
    match t.dominance_search {
        DominanceScope::Exhaustive => "no dominating reorder exists".to_string(),
        DominanceScope::Budgeted => "no dominating reorder found (dominance search budgeted)".to_string(),
        DominanceScope::Superset => format!(
            "no dominating reorder found (payload lengths unverified for {})",
            escape_text(&t.superset_types.join(", "))
        ),
    }
}

/// Suffix naming an incomplete search scope; a capped search must say so wherever it would
/// otherwise read as proof.
fn scope_note(t: &TableReport) -> &'static str {
    match t.search_scope {
        SearchScope::Complete => "",
        SearchScope::FixedPrefix => " (search capped: fixed prefix exact, varlena placement heuristic)",
        SearchScope::SortOnly => " (search capped: heuristic orders only)",
    }
}

/// The workload-dependent alternative: both orders, worst cases, and the decision boundary by
/// storage-form band. Reported only; the gate never sees it.
fn render_frontier(out: &mut String, t: &TableReport) {
    if let Some(frontier) = &t.frontier {
        render_frontier_body(out, t, frontier, "  frontier ");
    }
}

fn render_frontier_body(out: &mut String, t: &TableReport, frontier: &Frontier, label: &str) {
    let _ = writeln!(
        out,
        "{label}: {} — worst case {} B/row vs current {} B/row (workload-dependent, not gated)",
        escape_text(&frontier.order.join(", ")),
        frontier.alternative_worst,
        frontier.current_worst
    );
    if frontier.current_deterministic != frontier.alternative_deterministic {
        let _ = writeln!(
            out,
            "             deterministic padding: alternative {} B vs current {} B",
            frontier.alternative_deterministic, frontier.current_deterministic
        );
    }
    if !frontier.decided {
        let _ = writeln!(
            out,
            "             decision boundary not computed (too many varlenas to enumerate)"
        );
        return;
    }
    let printable = frontier.bands.iter().filter(|b| b.winner != BandWinner::Tie).count();
    let mut printed = 0usize;
    let has_varlena = t.columns.iter().any(|c| !c.kind.is_fixed());
    // What a mixed band's winner turns on: payload lengths, NULLs, or both.
    let varies = match (has_varlena, t.null_variables.is_empty()) {
        (true, true) => "payload lengths mod 8",
        (true, false) => "payload lengths mod 8 and which columns hold NULL",
        (false, _) => "which columns hold NULL",
    };
    let unit = match frontier.measure {
        Measure::Padding => "",
        Measure::RowSize => " in row size",
    };
    for band in &frontier.bands {
        if printed >= FRONTIER_BAND_LINE_CAP {
            break;
        }
        let condition = match band.long_form.as_slice() {
            [] if has_varlena => " when every varlena stays short or TOAST".to_string(),
            [] => String::new(),
            [one] => format!(" when {} stores long form", escape_text(one)),
            many => format!(" when {} store long form", escape_text(&many.join(", "))),
        };
        let line = match band.winner {
            BandWinner::Alternative => Some(format!(
                "alternative wins{unit}{condition} (saves {}-{} B/row)",
                band.min_saving, band.max_saving
            )),
            BandWinner::Current => Some(format!(
                "current wins{unit}{condition} (by {}-{} B/row)",
                -band.max_saving, -band.min_saving
            )),
            BandWinner::Mixed => Some(format!(
                "winner{unit} depends on {varies}{condition} ({} to {} B/row)",
                band.min_saving, band.max_saving
            )),
            BandWinner::Tie => None,
        };
        if let Some(line) = line {
            let _ = writeln!(out, "             {line}");
            printed += 1;
        }
    }
    if printable > printed {
        let _ = writeln!(
            out,
            "             ... {} further band(s) elided (all bands are in --format json)",
            printable - printed
        );
    }
    if let Some(rows) = &frontier.without_nulls {
        let line = match rows.winner {
            BandWinner::Alternative => format!(
                "in rows without NULLs the alternative wins{unit} (saves {}-{} B/row)",
                rows.min_saving, rows.max_saving
            ),
            BandWinner::Current => format!(
                "in rows without NULLs the current order wins{unit} (by {}-{} B/row)",
                -rows.max_saving, -rows.min_saving
            ),
            BandWinner::Mixed => format!(
                "in rows without NULLs the winner{unit} depends on payload lengths mod 8 ({} to {} B/row)",
                rows.min_saving, rows.max_saving
            ),
            BandWinner::Tie => match frontier.measure {
                Measure::Padding => "rows without NULLs pad identically in both orders".to_string(),
                Measure::RowSize => "rows without NULLs are the same size in both orders".to_string(),
            },
        };
        let _ = writeln!(out, "             {line}");
    }
    let _ = render_frontier_assumption_free(out, frontier);
}

/// Decision-boundary lines shown before the block elides into a summary: past this a band
/// listing carries no decision the reader can hold in their head.
const FRONTIER_BAND_LINE_CAP: usize = 6;

/// Frontier bands carry no model assumption, but say so once to keep the block self-contained.
fn render_frontier_assumption_free(out: &mut String, frontier: &Frontier) -> std::fmt::Result {
    if frontier.bands.iter().all(|b| b.winner == BandWinner::Tie) {
        writeln!(out, "             identical padding in every storage form")?;
    }
    Ok(())
}

fn stats_line(s: &OrderStats) -> String {
    match (s.footprint, s.rows_per_page) {
        (Some(fp), Some(rp)) => {
            let Some(rows) = s.with_nulls else {
                return format!("{} B padding, {fp} B/row footprint, {rp} rows/8kB page", s.padding);
            };
            // Rows without NULLs are byte-exact; rows with one carry the bitmap header.
            let padding = if s.padding_min == s.padding_max {
                format!("{} B padding", s.padding_min)
            } else {
                format!("{}-{} B padding", s.padding_min, s.padding_max)
            };
            let footprint = if rows.footprint_min == rows.footprint_max {
                rows.footprint_min.to_string()
            } else {
                format!("{}-{}", rows.footprint_min, rows.footprint_max)
            };
            format!(
                "{} B padding, {fp} B/row footprint, {rp} rows/8kB page without NULLs; \
                 {padding}, {footprint} B/row with NULLs ({} B header)",
                s.padding, rows.t_hoff
            )
        }
        _ if s.padding_min == s.padding_max => format!("{} B padding/row", s.padding),
        _ => {
            let without = match s.without_nulls {
                Some(b) => format!(", {}-{} without NULLs", b.min, b.max),
                None => String::new(),
            };
            format!(
                "{:.1} B/row expected padding ({} B deterministic, range {}-{}{without}, data-dependent)",
                s.expected_padding, s.padding, s.padding_min, s.padding_max
            )
        }
    }
}

fn render_flags(out: &mut String, t: &TableReport) {
    if t.incomplete {
        let _ = writeln!(
            out,
            "  ⚠ incomplete: a statement affecting this table was skipped or not expanded"
        );
    }
    if !t.assumed_types.is_empty() {
        let list = t.assumed_types.join(", ");
        let _ = writeln!(
            out,
            "  ⚠ assumed varlena/int-aligned (teach via --assume-type): {}",
            escape_text(&list)
        );
    }
    if !t.null_variables.is_empty() && (t.current.without_nulls.is_some() || t.current.with_nulls.is_some()) {
        let _ = writeln!(
            out,
            "  NULLs move later offsets in: {} (NOT NULL removes that variable)",
            escape_text(&t.null_variables.join(", "))
        );
    }
}

fn render_suggestion(out: &mut String, t: &TableReport) {
    let _ = writeln!(
        out,
        "  -- rowdiet suggestion (column order only — re-attach defaults/constraints/options):"
    );
    let by_name: BTreeMap<&str, &ColumnReport> = t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let columns: Option<Vec<(String, String, bool)>> = t
        .suggested_order
        .iter()
        .filter_map(|name| by_name.get(name.as_str()))
        .map(|col| sql_spelling(&col.type_display).map(|ty| (maybe_quote(&col.name), ty, col.not_null)))
        .collect();
    let (Some(table), Some(columns)) = (sql_spelling(&t.display), columns) else {
        let _ = writeln!(
            out,
            "  -- withheld: a table or type name here has characters a log line cannot carry; see the order line"
        );
        return;
    };
    let _ = writeln!(out, "  CREATE TABLE {table} (");
    let last = columns.len().saturating_sub(1);
    for (i, (name, ty, not_null)) in columns.iter().enumerate() {
        let not_null = if *not_null { " NOT NULL" } else { "" };
        let comma = if i == last { "" } else { "," };
        let _ = writeln!(out, "      {name} {ty}{not_null}{comma}");
    }
    let _ = writeln!(out, "  );");
}

/// A name or type spelling as SQL that names the same object on one log line: every quoted
/// identifier needing escapes becomes `U&"..."`. None when an escape would fall outside quotes.
fn sql_spelling(spelling: &str) -> Option<String> {
    let mut out = String::with_capacity(spelling.len());
    let mut chars = spelling.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
            continue;
        }
        let mut ident = String::new();
        loop {
            match chars.next()? {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    ident.push('"');
                }
                '"' => break,
                c => ident.push(c),
            }
        }
        if matches!(escape_text(&ident), Cow::Owned(_)) {
            out.push_str(&unicode_quote(&ident));
        } else {
            let _ = write!(out, "\"{}\"", ident.replace('"', "\"\""));
        }
    }
    matches!(escape_text(&out.replace('\\', "")), Cow::Borrowed(_)).then_some(out)
}

fn maybe_quote(ident: &str) -> String {
    let plain = !ident.is_empty()
        && !ident.starts_with(|c: char| c.is_ascii_digit())
        && ident
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        ident.to_string()
    } else if matches!(escape_text(ident), Cow::Owned(_)) {
        unicode_quote(ident)
    } else {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

/// `U&"..."` spelling: the exact name, printed without control characters or `##[`.
fn unicode_quote(ident: &str) -> String {
    let mut out = String::from("U&\"");
    for c in ident.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\"\""),
            '[' if out.ends_with("##") => out.push_str("\\005B"),
            c if is_line_hazard(c) => {
                let _ = write!(out, "\\{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One log line with no workflow command in it: control characters and `##[` print as escapes,
/// and a backslash doubles so an escape never reads like a name's own text.
pub(crate) fn escape_text(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c == '\\' || is_line_hazard(c)) && !s.contains("##[") {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '[' if out.ends_with("##") => out.push_str("\\u{5b}"),
            c if is_line_hazard(c) => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// [`escape_text`] that also escapes a leading `::`, for text that opens a line.
fn escape_line_start(s: &str) -> Cow<'_, str> {
    let escaped = escape_text(s);
    let body = escaped.trim_start();
    if !body.starts_with("::") {
        return escaped;
    }
    let indent = escaped.len() - body.len();
    Cow::Owned(format!("{}\\u{{3a}}{}", &escaped[..indent], &body[1..]))
}

fn is_line_hazard(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

fn tier_label(t: &TableReport) -> &'static str {
    match t.tier {
        Tier::Exact if t.null_variables.is_empty() => "exact — fixed-width only",
        Tier::Exact => EXACT_NULLS_LABEL,
        Tier::Estimate => ESTIMATE_LABEL,
        Tier::Unknown => "unknown — columns not fully known",
    }
}

/// The estimate tier's label states the decision policy and the display model wherever a
/// number is shown: the gate and the reorder advice rest on deterministic pads and dominance
/// (never worse in any storage-form, payload, or NULL realization); expected values are
/// display-only figures under the stated model (varlena pads scored at the short/TOAST form,
/// offset residues uniform, no NULLs). The printed min/max range bounds every realization with
/// no assumption.
const ESTIMATE_LABEL: &str = "estimate — gates on deterministic and dominance-proven padding over every storage form, payload length, and NULL; expected values are display-only (short-form, uniform-offset, no-NULL model)";

/// An all-fixed table with nullable columns: every row is byte-exact, but which columns a row
/// stores varies, so the gate takes the row-size saving over every NULL pattern.
const EXACT_NULLS_LABEL: &str =
    "exact per NULL pattern — fixed-width only, gates on the row-size saving over every NULL pattern";

fn kind_label(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::SkippedStatement => "skipped",
        NoteKind::AlterUnknownTable => "unknown-table",
        NoteKind::AlterSkippedTable => "skipped-table",
        NoteKind::CtasSkipped => "ctas",
        NoteKind::IncompleteColumns => "incomplete",
        NoteKind::DroppedColumn => "dropped-column",
        NoteKind::UnknownType => "unknown-type",
        NoteKind::Redefined => "redefined",
        NoteKind::DuplicateColumn => "duplicate-column",
        NoteKind::UnknownColumn => "unknown-column",
        NoteKind::DoBlockDdl => "do-block",
        NoteKind::UnusedIgnoreMarker => "unused-ignore",
        NoteKind::TempTableSkipped => "temp-table",
        NoteKind::EmptyScan => "empty-scan",
    }
}

fn human_bytes(bytes: f64) -> String {
    if bytes >= 1e9 {
        format!("{:.1} GB", bytes / 1e9)
    } else if bytes >= 1e6 {
        format!("{:.1} MB", bytes / 1e6)
    } else if bytes >= 1e3 {
        format!("{:.1} kB", bytes / 1e3)
    } else {
        format!("{bytes} B")
    }
}

pub fn github(analysis: &Analysis, gate: &GateOutcome) -> String {
    let mut out = String::new();
    let mut budget = AnnotationBudget::new();
    for t in &analysis.tables {
        if t.ignored || t.avoidable_bytes_per_row == 0.0 {
            continue;
        }
        let verdict = gate.verdicts.get(&t.name).copied();
        // A failing block is reported at the statement that appended it, below.
        if matches!(verdict, Some(TableVerdict::BlockNotDominanceOptimal { .. })) {
            continue;
        }
        let level = if verdict.is_some_and(TableVerdict::failing) {
            "error"
        } else {
            "warning"
        };
        let title = match verdict {
            Some(TableVerdict::ModifiedSinceBaseline { .. }) => "rowdiet modified-since-baseline",
            _ => "rowdiet",
        };
        let saving = match t.dominance_saving {
            Some(range) => format!("; saves {}-{} B/row in every realization", range.min, range.max),
            None => String::new(),
        };
        let message = format!(
            "table {}: {:.1} B/row avoidable{saving}; {}{} — suggested order: {}",
            t.name,
            t.avoidable_bytes_per_row,
            tier_label(t),
            scope_note(t),
            t.suggested_order.join(", ")
        );
        budget.emit(
            &mut out,
            level,
            &format!(
                "::{level} file={},line={},title={}::{}",
                escape_property(&t.origin.source),
                t.origin.line,
                escape_property(title),
                escape_message(&message)
            ),
        );
    }
    for (name, block) in &gate.blocks {
        if !matches!(
            gate.verdicts.get(name),
            Some(TableVerdict::BlockNotDominanceOptimal { .. })
        ) {
            continue;
        }
        let saving = match block.dominance_saving {
            Some(range) => format!("; saves {}-{} B/row in every realization", range.min, range.max),
            None => String::new(),
        };
        let message = format!(
            "table {name}: appended block {} is not dominance-optimal ({:.1} B/row avoidable{saving}); block order: {}; \
             the committed prefix stays",
            block.columns.join(", "),
            block.avoidable_bytes_per_row,
            block.suggested_order.join(", ")
        );
        budget.emit(
            &mut out,
            "error",
            &format!(
                "::error file={},line={},title={}::{}",
                escape_property(&block.origins[0].source),
                block.origins[0].line,
                escape_property("rowdiet block-not-dominance-optimal"),
                escape_message(&message)
            ),
        );
    }
    for note in &analysis.notes {
        let level = match note.kind {
            NoteKind::SkippedStatement => "warning",
            _ => "notice",
        };
        budget.emit(
            &mut out,
            level,
            &format!(
                "::{level} file={},line={},title={}::{}",
                escape_property(&note.origin.source),
                note.origin.line,
                escape_property(&format!("rowdiet {}", kind_label(note.kind))),
                escape_message(&note.detail)
            ),
        );
    }
    budget.finish(&mut out);
    out
}

/// The Actions runner decodes `%25`/`%0D`/`%0A` in annotation message data — a literal `%`
/// (e.g. a `format('%I')` template quoted in a note) mis-decodes unless escaped, and newlines
/// terminate the command.
fn escape_message(s: &str) -> String {
    let truncated = if s.len() > MESSAGE_CAP {
        let mut end = MESSAGE_CAP;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    } else {
        s
    };
    truncated.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// Property values (`file=`, `title=`) additionally decode `%3A`/`%2C`, so a comma or colon in
/// a source path silently corrupts property parsing unless escaped.
fn escape_property(s: &str) -> String {
    escape_message(s).replace(':', "%3A").replace(',', "%2C")
}

/// The runner keeps at most 10 annotations per severity per step and drops overflow silently;
/// messages are capped at 4096 chars. Truncation here is loud instead: a final notice reports
/// the count, and the step summary (uncapped) carries the full report.
const ANNOTATION_CAP: usize = 10;
const MESSAGE_CAP: usize = 4000;

struct AnnotationBudget {
    errors: usize,
    warnings: usize,
    notices: usize,
    dropped: usize,
}

impl AnnotationBudget {
    fn new() -> Self {
        Self {
            errors: 0,
            warnings: 0,
            notices: 0,
            dropped: 0,
        }
    }

    fn emit(&mut self, out: &mut String, level: &str, line: &str) {
        let count = match level {
            "error" => &mut self.errors,
            "warning" => &mut self.warnings,
            _ => &mut self.notices,
        };
        // Notices stop one early so the suppression notice below always fits the runner cap.
        let cap = if level == "notice" {
            ANNOTATION_CAP - 1
        } else {
            ANNOTATION_CAP
        };
        if *count < cap {
            *count += 1;
            let _ = writeln!(out, "{line}");
        } else {
            self.dropped += 1;
        }
    }

    fn finish(&self, out: &mut String) {
        if self.dropped > 0 {
            let _ = writeln!(
                out,
                "::notice title=rowdiet::{} annotation(s) suppressed (the runner keeps 10 per severity per step) — \
                 the full report is in the step summary and the text/json output",
                self.dropped
            );
        }
    }
}

/// Markdown for `$GITHUB_STEP_SUMMARY`: the full, uncapped report — the annotation budget
/// above stays honest because everything it drops is here.
pub fn github_step_summary(analysis: &Analysis, gate: &GateOutcome) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## rowdiet\n");
    let _ = writeln!(
        out,
        "| table | avoidable B/row | saves B/row | scope | tier | verdict | origin |"
    );
    let _ = writeln!(out, "|---|---:|---|---|---|---|---|");
    for t in &analysis.tables {
        if t.ignored {
            continue;
        }
        let verdict = match gate.verdicts.get(&t.name).copied() {
            Some(TableVerdict::Pass) | None => "pass".to_string(),
            Some(TableVerdict::NewViolation { .. }) => "**new violation**".to_string(),
            Some(TableVerdict::BlockNotDominanceOptimal { .. }) => format!(
                "**block not dominance-optimal** (block order: {})",
                gate.blocks
                    .get(&t.name)
                    .map(|b| markdown_cell(&b.suggested_order.join(", ")))
                    .unwrap_or_default()
            ),
            Some(TableVerdict::ModifiedSinceBaseline { .. }) => "**modified since baseline**".to_string(),
            Some(TableVerdict::Incomplete) => "incomplete".to_string(),
        };
        let tier = match t.tier {
            Tier::Exact => "exact",
            Tier::Estimate => "estimate",
            Tier::Unknown => "unknown",
        };
        let verdict = match &t.frontier {
            Some(frontier) => format!(
                "{verdict} (frontier: {} — worst {} vs {})",
                markdown_cell(&frontier.order.join(", ")),
                frontier.alternative_worst,
                frontier.current_worst
            ),
            None => verdict,
        };
        let saving = match t.dominance_saving {
            Some(range) => format!("{}-{}", range.min, range.max),
            None => "-".to_string(),
        };
        let scope = match t.search_scope {
            SearchScope::Complete => match t.dominance_search {
                DominanceScope::Exhaustive => "complete",
                DominanceScope::Budgeted => "complete (dominance budgeted)",
                DominanceScope::Superset => "complete (payload model unverified)",
            },
            SearchScope::FixedPrefix => "capped: fixed prefix",
            SearchScope::SortOnly => "capped: sort only",
        };
        let _ = writeln!(
            out,
            "| {} | {:.1} | {saving} | {scope} | {tier} | {verdict} | {}:{} |",
            markdown_cell(&t.display),
            t.avoidable_bytes_per_row,
            markdown_cell(&t.origin.source),
            t.origin.line
        );
    }
    let ignored = analysis.tables.iter().filter(|t| t.ignored).count();
    let _ = writeln!(out);
    if analysis.tables.iter().any(|t| !t.ignored && t.tier == Tier::Estimate) {
        let _ = writeln!(out, "{ESTIMATE_LABEL}.\n");
    }
    if analysis
        .tables
        .iter()
        .any(|t| !t.ignored && t.tier == Tier::Exact && !t.null_variables.is_empty())
    {
        let _ = writeln!(out, "{EXACT_NULLS_LABEL}.\n");
    }
    if ignored > 0 {
        let _ = writeln!(out, "{ignored} table(s) ignored via rowdiet:ignore.\n");
    }
    if !analysis.notes.is_empty() {
        let _ = writeln!(out, "<details><summary>{} note(s)</summary>\n", analysis.notes.len());
        for note in &analysis.notes {
            let _ = writeln!(
                out,
                "- `{}:{}` [{}] {}",
                markdown_cell(&note.origin.source),
                note.origin.line,
                kind_label(note.kind),
                markdown_cell(&note.detail)
            );
        }
        let _ = writeln!(out, "\n</details>\n");
    }
    let mut gate_line = String::new();
    render_gate_summary(&mut gate_line, gate);
    if gate_line.is_empty() {
        let _ = writeln!(out, "Gate: ok.");
    } else {
        let _ = writeln!(out, "```\n{gate_line}```");
    }
    out
}

fn markdown_cell(s: &str) -> String {
    escape_text(s).replace('|', "\\|")
}

pub fn json(analysis: &Analysis, fail_over: Option<f64>, gate: &GateOutcome) -> Result<String, String> {
    let mut value = serde_json::json!({
        "rowdiet": env!("CARGO_PKG_VERSION"),
        "fail_over": fail_over,
        "gate_exceeded": gate.exceeded,
        "gate": serde_json::to_value(gate).map_err(|e| e.to_string())?,
        "analysis": serde_json::to_value(analysis).map_err(|e| e.to_string())?,
    });
    // Estimate-tier numbers are meaningless without their model; state it in the payload
    // whenever such a table is present.
    if analysis.tables.iter().any(|t| !t.ignored && t.tier == Tier::Estimate) {
        value["estimate_assumptions"] = serde_json::Value::String(ESTIMATE_LABEL.to_string());
    }
    // `##[` only occurs inside strings, where `\u005b` decodes back to the same bracket.
    serde_json::to_string_pretty(&value)
        .map(|s| s.replace("##[", "##\\u005b") + "\n")
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
