//! The public result model, and the fold→layout assembly that fills it.
//!
//! Reporting contract: for fixed-width-only tables everything is byte-exact —
//! the headline is the MAXALIGN-rounded footprint delta, and a reorder that does not cross an
//! 8-byte rung reports zero avoidable bytes. Tables with varlena columns get long-form-scenario
//! numbers, labeled as estimates and never claimed as guaranteed savings.

use crate::fold::{FoldedTable, Note, Origin};
use crate::layout::{self, ColumnKind, Tier, Walk};

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Analysis {
    pub tables: Vec<TableReport>,
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableReport {
    pub name: String,
    pub origin: Origin,
    pub altered_in: Vec<Origin>,
    pub ignored: bool,
    pub incomplete: bool,
    pub tier: Tier,
    pub natts: usize,
    pub any_nullable: bool,
    pub columns: Vec<ColumnReport>,
    pub current: OrderStats,
    pub suggested: OrderStats,
    pub suggested_order: Vec<String>,
    pub avoidable_bytes_per_row: u64,
    pub assumed_types: Vec<String>,
    /// Canonical fingerprint of the as-written layout: the ordered resolved-kind sequence and
    /// nothing else (column names and nullability do not enter the avoidable computation, so
    /// renames and SET/DROP NOT NULL do not change it). Baseline entries expire against this.
    pub layout_signature: String,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ColumnReport {
    pub name: String,
    pub type_display: String,
    pub not_null: bool,
    pub known_type: bool,
    pub kind: ColumnKind,
    pub pad_before: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OrderStats {
    pub padding: u64,
    pub footprint: Option<u64>,
    pub rows_per_page: Option<u64>,
}

pub(crate) fn build(table: FoldedTable) -> TableReport {
    let kinds: Vec<ColumnKind> = table.columns.iter().map(|c| c.kind).collect();
    let tier = layout::tier(&kinds);
    let current_walk = layout::walk(&kinds);
    let mut order = layout::suggested_order(&kinds);
    let ordered_kinds: Vec<ColumnKind> = order.iter().map(|&i| kinds[i]).collect();
    let mut suggested_walk = layout::walk(&ordered_kinds);
    if suggested_walk.padding > current_walk.padding {
        order = (0..kinds.len()).collect();
        suggested_walk = current_walk.clone();
    }
    let current = stats(tier, &current_walk);
    let suggested = stats(tier, &suggested_walk);
    let avoidable = match tier {
        Tier::Exact => current
            .footprint
            .unwrap_or(0)
            .saturating_sub(suggested.footprint.unwrap_or(0)),
        Tier::Estimate => current.padding.saturating_sub(suggested.padding),
    };
    let final_order: Vec<usize> = if avoidable == 0 {
        (0..kinds.len()).collect()
    } else {
        order
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
        name: table.display,
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

fn stats(tier: Tier, walk: &Walk) -> OrderStats {
    match tier {
        Tier::Exact => {
            let footprint = layout::footprint(walk.scenario_end);
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
