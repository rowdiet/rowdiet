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
            let origin = format!("{}:{}", note.origin.source, note.origin.line);
            let _ = writeln!(out, "  {origin} [{}] {}", kind_label(note.kind), note.detail);
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
    let loc = format!("{}:{}", t.origin.source, t.origin.line);
    if t.ignored {
        let _ = writeln!(out, "∅ {} ({loc}) — ignored (rowdiet:ignore)", t.name);
        return;
    }
    // An empty modeled column set (partition child / LIKE / typed table) passes the gate
    // vacuously — say so instead of wearing an "optimal [exact]" chip it did not earn.
    if t.natts == 0 {
        let _ = writeln!(out, "◌ {} ({loc}) — no modeled columns — not analyzable", t.name);
        render_flags(out, t);
        return;
    }
    if t.avoidable_bytes_per_row == 0 {
        let detail = match (t.tier, t.current.padding) {
            (_, 0) => "optimal: zero padding".to_string(),
            (Tier::Exact, p) => format!("{p} B padding but footprint unchanged (MAXALIGN rounding) — nothing to gain"),
            (Tier::Estimate, p) => format!("{p} B scenario padding, none avoidable by reordering"),
        };
        let _ = writeln!(out, "✓ {} ({loc}) — {detail} [{}]", t.name, tier_label(t.tier));
        render_flags(out, t);
        render_verdict(out, t, verdict);
        return;
    }
    let _ = writeln!(
        out,
        "■ {} ({loc}) — {} columns — {}",
        t.name,
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
    let _ = writeln!(out, "  CREATE TABLE {} (", t.name);
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
        let _ = writeln!(
            out,
            "::{level} file={},line={},title={title}::table {}: {} B/row avoidable ({}) — suggested order: {}",
            t.origin.source,
            t.origin.line,
            t.name,
            t.avoidable_bytes_per_row,
            tier_label(t.tier),
            t.suggested_order.join(", ")
        );
    }
    for note in &analysis.notes {
        let level = match note.kind {
            NoteKind::SkippedStatement => "warning",
            _ => "notice",
        };
        let _ = writeln!(
            out,
            "::{level} file={},line={},title=rowdiet {}::{}",
            note.origin.source,
            note.origin.line,
            kind_label(note.kind),
            note.detail
        );
    }
    out
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
