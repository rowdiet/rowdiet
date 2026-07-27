//! The public result model, and the fold→layout assembly that fills it.
//!
//! Reporting contract: for fixed-width-only tables everything is byte-exact —
//! the headline is the MAXALIGN-rounded footprint delta, and a reorder that does not cross an
//! 8-byte rung reports zero avoidable bytes. Tables with varlena columns get long-form-scenario
//! numbers, labeled as estimates and never claimed as guaranteed savings.

use crate::fold::{FoldedTable, Note, NoteKind, Origin};
use crate::layout::{self, ColumnKind, Tier, Walk};

/// The complete result of one analysis run — what renderers, gates, and adapters consume.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Analysis {
    /// One report per table, in creation order (ignored tables included and marked).
    pub tables: Vec<TableReport>,
    /// Degradation and context notes, in encounter order.
    pub notes: Vec<Note>,
}

impl Analysis {
    /// The tables the gate considers: every analyzed table not exempted by `rowdiet:ignore`,
    /// in creation order. The same filter [`baseline::evaluate`](crate::baseline::evaluate)
    /// applies — hand-written gates should start here so they cannot drift from it.
    pub fn gated_tables(&self) -> impl Iterator<Item = &TableReport> {
        self.tables.iter().filter(|table| !table.ignored)
    }

    /// The largest [`avoidable_bytes_per_row`](TableReport::avoidable_bytes_per_row) among
    /// gated tables — 0 when every layout is tight (or nothing was analyzed). The number a
    /// zero-tolerance test asserts on. A clean maximum still says nothing about skipped
    /// statements, so pair it with a look at [`notes`](Self::notes) or use
    /// [`baseline::evaluate`](crate::baseline::evaluate) with `fail_on_degraded`.
    pub fn worst_avoidable(&self) -> u64 {
        self.gated_tables()
            .map(|table| table.avoidable_bytes_per_row)
            .max()
            .unwrap_or(0)
    }

    /// True when the analysis is degraded in a way rowdiet recognizes: a statement was skipped, a
    /// gated table is incomplete, or a scanned path held no SQL. The analysis-level twin of
    /// [`GateOutcome::degraded`](crate::baseline::GateOutcome::degraded), which it always agrees
    /// with (pinned by a test).
    ///
    /// Reach for this as a backstop *beside* explicit per-note-kind checks, not instead of them:
    /// an explicit filter names the failing class in its panic message, which a boolean cannot —
    /// but it also silently misses any degradation kind a later release adds, which this method,
    /// kept current by rowdiet, does not.
    pub fn degraded(&self) -> bool {
        self.notes
            .iter()
            .any(|note| matches!(note.kind, NoteKind::SkippedStatement | NoteKind::EmptyScan))
            || self.gated_tables().any(|table| table.incomplete)
    }
}

/// One table's full analysis: identity, provenance, per-column layout, current-vs-suggested
/// numbers, and the avoidable-bytes headline the gate acts on.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableReport {
    /// The fold key — lowercased unless the DDL quoted the name. Stable across parser
    /// backends, which makes it the identity that baselines and gate verdicts key on (display
    /// spelling is backend-dependent and cosmetic).
    pub name: String,
    /// The name as written in the DDL (first backend-dependent spelling seen).
    pub display: String,
    /// The statement that created the table.
    pub origin: Origin,
    /// Statements that changed the table after creation (consecutive duplicates collapsed).
    pub altered_in: Vec<Origin>,
    /// Exempted via `rowdiet:ignore` — listed, but outside gate and baseline.
    pub ignored: bool,
    /// The model is known partial: skipped or unexpanded DDL touched this table.
    pub incomplete: bool,
    /// How solid the numbers are; see [`Tier`].
    pub tier: Tier,
    /// Live column count (dropped attribute slots are in `dropped_columns`).
    pub natts: usize,
    /// Some column is nullable: real rows may then carry a null bitmap the canonical no-NULL
    /// scenario does not count.
    pub any_nullable: bool,
    /// Per-column detail, in current physical order, with offsets from the current walk.
    pub columns: Vec<ColumnReport>,
    /// Numbers for the order as written.
    pub current: OrderStats,
    /// Numbers for the suggested order; identical to `current` when nothing is avoidable.
    pub suggested: OrderStats,
    /// Column names (display spelling) in suggested order; the original order when nothing is
    /// avoidable.
    pub suggested_order: Vec<String>,
    /// The headline number gates compare: footprint delta (exact tier) or scenario-padding
    /// delta (estimate tier) between current and suggested order. 0 = reordering gains nothing.
    pub avoidable_bytes_per_row: u64,
    /// Type spellings that resolved by assumption, sorted and deduplicated — the table's
    /// numbers are only as good as those assumptions.
    pub assumed_types: Vec<String>,
    /// Columns dropped across the migration series. When nonzero, every NEW row still carries
    /// a null bitmap sized by the original attribute count (Postgres keeps dropped attribute
    /// slots), and the exact-tier footprint includes that header cost.
    pub dropped_columns: usize,
    /// Canonical fingerprint of the as-written layout: the ordered resolved-kind sequence and
    /// nothing else (column names and nullability do not enter the avoidable computation, so
    /// renames and SET/DROP NOT NULL do not change it). Baseline entries expire against this.
    pub layout_signature: String,
}

/// One column, as placed in the table's current physical order.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ColumnReport {
    /// Column name (display spelling).
    pub name: String,
    /// Declared type as written.
    pub type_display: String,
    /// Declared or implied NOT NULL.
    pub not_null: bool,
    /// False when the type resolved by assumption (listed in [`TableReport::assumed_types`]).
    pub known_type: bool,
    /// Resolved storage class.
    pub kind: ColumnKind,
    /// Padding before this column in the current order, bytes.
    pub pad_before: u64,
    /// Data start within the data area, bytes (tuple header not included).
    pub offset: u64,
}

/// Layout numbers for one column order — [`TableReport::current`] and
/// [`TableReport::suggested`] each hold one.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OrderStats {
    /// Inter-column alignment padding, bytes per row (meaningful at both tiers).
    pub padding: u64,
    /// Whole-row on-disk size, header included and MAXALIGN-rounded, bytes. None at the
    /// estimate tier — varlena payloads make it unknowable.
    pub footprint: Option<u64>,
    /// Rows of that footprint per 8 kB heap page. None at the estimate tier.
    pub rows_per_page: Option<u64>,
}

pub(crate) fn build(table: FoldedTable) -> TableReport {
    let kinds: Vec<ColumnKind> = table.columns.iter().map(|c| c.kind).collect();
    let tier = layout::tier(&kinds);
    // Dropped attributes are stored as NULL in every subsequent row, so the bitmap (sized by
    // the ORIGINAL natts) is unconditionally present; the all-non-NULL scenario otherwise
    // starts data at MAXALIGN(23) = 24.
    let t_hoff = if table.dropped_count > 0 {
        layout::null_thoff(kinds.len() + table.dropped_count)
    } else {
        layout::bare_thoff()
    };
    let current_walk = layout::walk(&kinds);
    let mut order = layout::suggested_order(&kinds);
    let ordered_kinds: Vec<ColumnKind> = order.iter().map(|&i| kinds[i]).collect();
    let mut suggested_walk = layout::walk(&ordered_kinds);
    if suggested_walk.padding > current_walk.padding {
        order = (0..kinds.len()).collect();
        suggested_walk = current_walk.clone();
    }
    let current = stats(tier, &current_walk, t_hoff);
    let suggested = stats(tier, &suggested_walk, t_hoff);
    let avoidable = match tier {
        Tier::Exact => current
            .footprint
            .unwrap_or(0)
            .saturating_sub(suggested.footprint.unwrap_or(0)),
        Tier::Estimate => current.padding.saturating_sub(suggested.padding),
    };
    // With nothing avoidable the suggestion IS the current order; the stats must say the same
    // thing, or the JSON contradicts itself (suggested.padding 0 beside the original order).
    let (final_order, suggested): (Vec<usize>, OrderStats) = if avoidable == 0 {
        ((0..kinds.len()).collect(), current.clone())
    } else {
        (order, suggested)
    };
    let suggested_order = final_order.iter().map(|&i| table.columns[i].display.clone()).collect();
    let columns = table
        .columns
        .iter()
        .zip(&current_walk.columns)
        .map(|(c, w)| ColumnReport {
            name: c.display.clone(),
            type_display: c.type_display.clone(),
            not_null: c.not_null,
            known_type: c.known_type,
            kind: c.kind,
            pad_before: w.pad_before,
            offset: w.offset,
        })
        .collect();
    let mut assumed_types: Vec<String> = table
        .columns
        .iter()
        .filter(|c| !c.known_type)
        .map(|c| c.type_display.clone())
        .collect();
    assumed_types.sort();
    assumed_types.dedup();
    let any_nullable = table.columns.iter().any(|c| !c.not_null);
    let layout_signature = layout_signature(&kinds);
    TableReport {
        name: table.key,
        display: table.display,
        origin: table.origin,
        altered_in: table.altered_in,
        ignored: table.ignored,
        incomplete: table.incomplete,
        tier,
        natts: kinds.len(),
        any_nullable,
        columns,
        current,
        suggested,
        suggested_order,
        avoidable_bytes_per_row: avoidable,
        assumed_types,
        dropped_columns: table.dropped_count,
        layout_signature,
    }
}

/// Canonical signature of a kind sequence: `f{len}{align}` per fixed column, `v{align}` per
/// varlena (`p` appended when typmod-proven short), comma-joined — e.g. `f8d,f4i,vi,vip`.
/// Stored verbatim in baseline entries: self-describing in diffs, and free of hash-stability
/// concerns across releases. `ADD COLUMN` appends, so growth keeps the old signature as a
/// comma-boundary prefix — the property the baseline gate's prefix rule relies on.
pub fn layout_signature(kinds: &[ColumnKind]) -> String {
    let parts: Vec<String> = kinds
        .iter()
        .map(|kind| match kind {
            ColumnKind::Fixed { len, align } => format!("f{len}{}", align_letter(*align)),
            ColumnKind::Varlena { align, proven_short } => {
                let p = if *proven_short { "p" } else { "" };
                format!("v{}{p}", align_letter(*align))
            }
        })
        .collect();
    parts.join(",")
}

fn align_letter(align: layout::Align) -> char {
    match align {
        layout::Align::Char => 'c',
        layout::Align::Short => 's',
        layout::Align::Int => 'i',
        layout::Align::Double => 'd',
    }
}

fn stats(tier: Tier, walk: &Walk, t_hoff: u64) -> OrderStats {
    match tier {
        Tier::Exact => {
            let footprint = layout::footprint_at(t_hoff, walk.scenario_end);
            OrderStats {
                padding: walk.padding,
                footprint: Some(footprint),
                rows_per_page: Some(layout::rows_per_page(footprint)),
            }
        }
        Tier::Estimate => OrderStats {
            padding: walk.padding,
            footprint: None,
            rows_per_page: None,
        },
    }
}
