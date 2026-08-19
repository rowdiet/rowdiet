//! Output renderers: human text, GitHub Actions annotations, JSON.

use rowdiet_core::layout::SearchScope;
use rowdiet_core::report::{BandWinner, DominanceScope, Frontier};
use rowdiet_core::{Analysis, ColumnReport, GateOutcome, NoteKind, OrderStats, TableReport, TableVerdict, Tier};
use std::collections::BTreeMap;
use std::fmt::Write as _;

pub fn text(analysis: &Analysis, rows: Option<u64>, suggest: bool, gate: &GateOutcome) -> String {
    let mut out = String::new();
    for table in &analysis.tables {
        render_table(&mut out, table, rows, suggest, gate.verdicts.get(&table.name).copied());
    }
    if !analysis.notes.is_empty() {
        let _ = writeln!(out, "notes:");
        for note in &analysis.notes {
            // Origin's Display prints the bare source for path-level notes (line 0).
            let _ = writeln!(out, "  {} [{}] {}", note.origin, kind_label(note.kind), note.detail);
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
    let mut regressions = 0u32;
    let mut grown = 0u32;
    let mut modified = 0u32;
    let mut ratchets = 0u32;
    for verdict in gate.verdicts.values() {
        match verdict {
            TableVerdict::NewViolation { .. } => new_violations += 1,
            TableVerdict::Regression { .. } => regressions += 1,
            TableVerdict::GrownSinceBaseline { .. } => grown += 1,
            TableVerdict::ModifiedSinceBaseline { .. } => modified += 1,
            TableVerdict::RatchetOpportunity { .. } => ratchets += 1,
            // Counted in gate.incomplete_tables and reported on the degraded line below.
            TableVerdict::Pass | TableVerdict::Incomplete => {}
        }
    }
    if gate.skipped_statements > 0 || gate.incomplete_tables > 0 || gate.empty_scans > 0 {
        let _ = writeln!(
            out,
            "degraded: {} statement(s) skipped, {} table(s) incomplete, {} path(s) matched no SQL files — pass --fail-on-degraded to gate on this",
            gate.skipped_statements, gate.incomplete_tables, gate.empty_scans
        );
    }
    if gate.exceeded {
        let mut parts = Vec::new();
        if new_violations > 0 {
            parts.push(format!("{new_violations} table(s) over the fail-over gate"));
        }
        if regressions > 0 {
            parts.push(format!("{regressions} regression(s) vs baseline"));
        }
        if grown > 0 {
            parts.push(format!("{grown} grown since baseline"));
        }
        if modified > 0 {
            parts.push(format!("{modified} modified since baseline"));
        }
        if parts.is_empty() {
            parts.push("degraded analysis (--fail-on-degraded)".to_string());
        }
        let _ = writeln!(out, "FAIL: {}", parts.join(", "));
    }
    if ratchets > 0 {
        let _ = writeln!(
            out,
            "baseline: {ratchets} table(s) now beat their allowance — tighten via --accept <table> or --update-baseline"
        );
    }
    if !gate.orphaned.is_empty() {
        let _ = writeln!(
            out,
            "baseline: orphaned entries (no matching table): {}",
            gate.orphaned.join(", ")
        );
    }
    if !gate.expired.is_empty() {
        let _ = writeln!(
            out,
            "baseline: expired entries (layout changed, table within fail-over): {}",
            gate.expired.join(", ")
        );
    }
}

fn render_table(out: &mut String, t: &TableReport, rows: Option<u64>, suggest: bool, verdict: Option<TableVerdict>) {
    let loc = &t.origin;
    if t.ignored {
        let _ = writeln!(out, "∅ {} ({loc}) — ignored (rowdiet:ignore)", t.display);
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
        let _ = writeln!(out, "◌ {} ({loc}) — {detail} — not analyzable", t.display);
        render_flags(out, t);
        return;
    }
    if t.avoidable_bytes_per_row == 0.0 {
        let detail = match (t.tier, t.current.padding) {
            (Tier::Estimate, _) => format!("{}, {}{}", stats_line(&t.current), verdict_phrase(t), scope_note(t)),
            (Tier::Exact, 0) => format!("optimal: zero padding{}", scope_note(t)),
            (Tier::Exact, p) => {
                format!(
                    "{p} B padding but footprint unchanged (MAXALIGN rounding) — nothing to gain{}",
                    scope_note(t)
                )
            }
            // Incomplete tables return above as "not analyzable"; unreachable here in practice.
            (Tier::Unknown, _) => "columns not fully known".to_string(),
        };
        let _ = writeln!(out, "✓ {} ({loc}) — {detail} [{}]", t.display, tier_label(t.tier));
        render_frontier(out, t);
        render_flags(out, t);
        render_verdict(out, t, verdict);
        return;
    }
    let _ = writeln!(
        out,
        "■ {} ({loc}) — {} columns — {}{}",
        t.display,
        t.natts,
        tier_label(t.tier),
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
    let _ = writeln!(out, "  order    : {}", t.suggested_order.join(", "));
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
    render_verdict(out, t, verdict);
    if suggest {
        render_suggestion(out, t);
    }
}

fn render_verdict(out: &mut String, t: &TableReport, verdict: Option<TableVerdict>) {
    match verdict {
        Some(TableVerdict::Regression { avoidable, allowed }) => {
            let _ = writeln!(
                out,
                "  ✗ regression: {avoidable:.1} B/row exceeds the baselined allowance of {allowed}"
            );
        }
        Some(TableVerdict::GrownSinceBaseline { allowed, .. }) => {
            let _ = writeln!(
                out,
                "  ✗ grown since baseline: appended columns push waste past the allowance of {allowed} — \
                 reorder them in the appending migration, or --accept {}",
                t.name
            );
        }
        Some(TableVerdict::ModifiedSinceBaseline { .. }) => {
            let _ = writeln!(
                out,
                "  ✗ modified since baseline: the allowance expired — meet fail-over or re-accept with --accept {}",
                t.name
            );
        }
        Some(TableVerdict::RatchetOpportunity { avoidable, allowed }) => {
            let _ = writeln!(
                out,
                "  ↓ ratchet: allowance {allowed} can tighten to {avoidable} — --accept {}",
                t.name
            );
        }
        Some(TableVerdict::Pass | TableVerdict::NewViolation { .. } | TableVerdict::Incomplete) | None => {}
    }
}

/// The clean-table phrase: an exhaustive sweep proves absence, a budgeted one only reports it.
fn verdict_phrase(t: &TableReport) -> &'static str {
    match t.dominance_search {
        DominanceScope::Exhaustive => "no dominating reorder exists",
        DominanceScope::Budgeted => "no dominating reorder found (dominance search budgeted)",
    }
}

/// Suffix naming an incomplete search scope; a capped search must say so wherever it would
/// otherwise read as proof.
fn scope_note(t: &TableReport) -> &'static str {
    match t.search_scope {
        SearchScope::Complete => "",
        SearchScope::FixedPrefix => " (search capped: fixed prefix exact, varlena placement heuristic)",
        SearchScope::SortOnly => " (search capped: heuristic sort only)",
    }
}

/// The workload-dependent alternative: both orders, worst cases, and the decision boundary by
/// storage-form band. Reported only; the gate never sees it.
fn render_frontier(out: &mut String, t: &TableReport) {
    let Some(frontier) = &t.frontier else { return };
    let _ = writeln!(
        out,
        "  frontier : {} — worst case {} B/row vs current {} B/row (workload-dependent, not gated)",
        frontier.order.join(", "),
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
    for band in &frontier.bands {
        if printed >= FRONTIER_BAND_LINE_CAP {
            break;
        }
        let condition = match band.long_form.as_slice() {
            [] => "when every varlena stays short or TOAST".to_string(),
            [one] => format!("when {one} stores long form"),
            many => format!("when {} store long form", many.join(", ")),
        };
        let line = match band.winner {
            BandWinner::Alternative => Some(format!(
                "alternative wins {condition} (saves {}-{} B/row)",
                band.min_saving, band.max_saving
            )),
            BandWinner::Current => Some(format!(
                "current wins {condition} (by {}-{} B/row)",
                -band.max_saving, -band.min_saving
            )),
            BandWinner::Mixed => Some(format!(
                "winner depends on payload lengths mod 8 {condition} ({} to {} B/row)",
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
        (Some(fp), Some(rp)) => format!("{} B padding, {fp} B/row footprint, {rp} rows/8kB page", s.padding),
        _ if s.padding_min == s.padding_max => format!("{} B padding/row", s.padding),
        _ => format!(
            "{:.1} B/row expected padding ({} B deterministic, range {}-{}, data-dependent)",
            s.expected_padding, s.padding, s.padding_min, s.padding_max
        ),
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
        let _ = writeln!(out, "  ⚠ assumed varlena/int-aligned (teach via --assume-type): {list}");
    }
}

fn render_suggestion(out: &mut String, t: &TableReport) {
    let _ = writeln!(
        out,
        "  -- rowdiet suggestion (column order only — re-attach defaults/constraints/options):"
    );
    let _ = writeln!(out, "  CREATE TABLE {} (", t.display);
    let by_name: BTreeMap<&str, &ColumnReport> = t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let last = t.suggested_order.len().saturating_sub(1);
    for (i, name) in t.suggested_order.iter().enumerate() {
        if let Some(col) = by_name.get(name.as_str()) {
            let not_null = if col.not_null { " NOT NULL" } else { "" };
            let comma = if i == last { "" } else { "," };
            let _ = writeln!(out, "      {} {}{not_null}{comma}", maybe_quote(name), col.type_display);
        }
    }
    let _ = writeln!(out, "  );");
}

fn maybe_quote(ident: &str) -> String {
    let plain = !ident.is_empty()
        && !ident.starts_with(|c: char| c.is_ascii_digit())
        && ident
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        ident.to_string()
    } else {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Exact => "exact — fixed-width only",
        Tier::Estimate => ESTIMATE_LABEL,
        Tier::Unknown => "unknown — columns not fully known",
    }
}

/// The estimate tier's label states the decision policy and the display model wherever a
/// number is shown: the gate and the reorder advice rest on deterministic pads and dominance
/// (never worse in any storage-form/payload realization); expected values are display-only
/// figures under the stated model (varlena pads scored at the short/TOAST form, offset
/// residues uniform). The printed min/max range bounds all storage forms with no assumption.
const ESTIMATE_LABEL: &str = "estimate — gates on deterministic and dominance-proven padding; expected values are display-only (short-form, uniform-offset model)";

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
        let level = if verdict.is_some_and(TableVerdict::failing) {
            "error"
        } else {
            "warning"
        };
        let title = match verdict {
            Some(TableVerdict::Regression { .. }) => "rowdiet regression",
            Some(TableVerdict::GrownSinceBaseline { .. }) => "rowdiet grown-since-baseline",
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
            tier_label(t.tier),
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
            Some(TableVerdict::Regression { allowed, .. }) => format!("**regression** (allowed {allowed})"),
            Some(TableVerdict::GrownSinceBaseline { allowed, .. }) => {
                format!("**grown since baseline** (allowed {allowed})")
            }
            Some(TableVerdict::ModifiedSinceBaseline { .. }) => "**modified since baseline**".to_string(),
            Some(TableVerdict::RatchetOpportunity { allowed, .. }) => format!("ratchet (allowed {allowed})"),
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
    s.replace('|', "\\|").replace('\n', " ")
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
    serde_json::to_string_pretty(&value)
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
