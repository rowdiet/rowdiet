//! Output renderers: human text, GitHub Actions annotations, JSON.

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
        .filter(|t| !t.ignored && t.avoidable_bytes_per_row > 0)
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
            TableVerdict::Pass => {}
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
    // An empty modeled column set (partition child / LIKE / typed table) passes the gate
    // vacuously — report it as not analyzable rather than as "optimal [exact]".
    if t.natts == 0 {
        let _ = writeln!(out, "◌ {} ({loc}) — no modeled columns — not analyzable", t.display);
        render_flags(out, t);
        return;
    }
    if t.avoidable_bytes_per_row == 0 {
        let detail = match (t.tier, t.current.padding) {
            (_, 0) => "optimal: zero padding".to_string(),
            (Tier::Exact, p) => format!("{p} B padding but footprint unchanged (MAXALIGN rounding) — nothing to gain"),
            (Tier::Estimate, p) => format!("{p} B scenario padding, none avoidable by reordering"),
        };
        let _ = writeln!(out, "✓ {} ({loc}) — {detail} [{}]", t.display, tier_label(t.tier));
        render_flags(out, t);
        render_verdict(out, t, verdict);
        return;
    }
    let _ = writeln!(
        out,
        "■ {} ({loc}) — {} columns — {}",
        t.display,
        t.natts,
        tier_label(t.tier)
    );
    let _ = writeln!(out, "  current  : {}", stats_line(&t.current));
    let _ = writeln!(
        out,
        "  suggested: {} → {} B/row avoidable",
        stats_line(&t.suggested),
        t.avoidable_bytes_per_row
    );
    let _ = writeln!(out, "  order    : {}", t.suggested_order.join(", "));
    if let Some(n) = rows {
        let _ = writeln!(
            out,
            "  × {n} rows ≈ {}",
            human_bytes(t.avoidable_bytes_per_row.saturating_mul(n))
        );
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
                "  ✗ regression: {avoidable} B/row exceeds the baselined allowance of {allowed}"
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
        Some(TableVerdict::Pass | TableVerdict::NewViolation { .. }) | None => {}
    }
}

fn stats_line(s: &OrderStats) -> String {
    match (s.footprint, s.rows_per_page) {
        (Some(fp), Some(rp)) => format!("{} B padding, {fp} B/row footprint, {rp} rows/8kB page", s.padding),
        _ => format!("{} B padding/row (long-form scenario)", s.padding),
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
        Tier::Estimate => "estimate — long-form varlena scenario",
    }
}

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

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1e6)
    } else if bytes >= 1_000 {
        format!("{:.1} kB", bytes as f64 / 1e3)
    } else {
        format!("{bytes} B")
    }
}

pub fn github(analysis: &Analysis, gate: &GateOutcome) -> String {
    let mut out = String::new();
    let mut budget = AnnotationBudget::new();
    for t in &analysis.tables {
        if t.ignored || t.avoidable_bytes_per_row == 0 {
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
        let message = format!(
            "table {}: {} B/row avoidable ({}) — suggested order: {}",
            t.name,
            t.avoidable_bytes_per_row,
            tier_label(t.tier),
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
    let _ = writeln!(out, "| table | avoidable B/row | tier | verdict | origin |");
    let _ = writeln!(out, "|---|---:|---|---|---|");
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
        };
        let tier = match t.tier {
            Tier::Exact => "exact",
            Tier::Estimate => "estimate",
        };
        let _ = writeln!(
            out,
            "| {} | {} | {tier} | {verdict} | {}:{} |",
            markdown_cell(&t.display),
            t.avoidable_bytes_per_row,
            markdown_cell(&t.origin.source),
            t.origin.line
        );
    }
    let ignored = analysis.tables.iter().filter(|t| t.ignored).count();
    let _ = writeln!(out);
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

pub fn json(analysis: &Analysis, fail_over: Option<u64>, gate: &GateOutcome) -> Result<String, String> {
    let value = serde_json::json!({
        "rowdiet": env!("CARGO_PKG_VERSION"),
        "fail_over": fail_over,
        "gate_exceeded": gate.exceeded,
        "gate": serde_json::to_value(gate).map_err(|e| e.to_string())?,
        "analysis": serde_json::to_value(analysis).map_err(|e| e.to_string())?,
    });
    serde_json::to_string_pretty(&value)
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
