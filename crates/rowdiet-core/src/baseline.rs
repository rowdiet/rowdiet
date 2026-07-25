//! Freeze-known-debt gating for brownfield adoption. A baseline file records, per table, an
//! accepted avoidable-bytes allowance pinned to the table's layout signature: new tables must
//! meet `fail_over`, baselined tables must not exceed their allowance, and improvements surface
//! as ratchet opportunities — tightening stays an explicit maintenance act, never automatic.
//!
//! Allowances are sticky to the layout, not just the name. A signature change means the table
//! was touched, and the response depends on how: `ADD COLUMN` appends, leaving the old signature
//! as a comma-boundary prefix of the new one — the allowance survives (the accepted layout is
//! intact; only the append itself is judged), because expiring it would demand a full-table
//! rewrite of an already-applied table. Any other change (reorder, drop, type change) implies a
//! rewriting migration was taken anyway, so the allowance expires and the table must meet
//! `fail_over` or be re-accepted deliberately.

use crate::report::{Analysis, TableReport};
use std::collections::BTreeMap;

/// In-memory form of a baseline file (JSON on disk): the default gate plus per-table accepted
/// debt. Built by [`build_from`], maintained by [`accept_tables`], consumed by [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Baseline {
    /// Version of rowdiet that wrote the file (informational).
    #[cfg_attr(feature = "serde", serde(default))]
    pub rowdiet: String,
    /// Default gate for tables without an entry — i.e. for every table added after baselining.
    pub fail_over: u64,
    /// Accepted-debt entries, keyed by the fold key ([`TableReport::name`]). The maintenance
    /// ops record only tables over `fail_over` — within it, no allowance is needed.
    pub tables: BTreeMap<String, BaselineEntry>,
}

/// One table's accepted allowance, pinned to the layout it was accepted for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BaselineEntry {
    /// Accepted avoidable bytes/row, in force while `layout` still describes the table.
    pub bytes: u64,
    /// The table's [`crate::report::layout_signature`] at acceptance time.
    pub layout: String,
}

/// Per-table gate result. `allowed` is the baselined allowance that applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "verdict", rename_all = "snake_case")
)]
pub enum TableVerdict {
    /// Within the applicable limit (allowance or `fail_over`), or no limit is in force.
    Pass,
    /// No baseline entry and avoidable exceeds `fail_over`.
    NewViolation {
        /// Current avoidable bytes/row.
        avoidable: u64,
    },
    /// Entry applies (signature unchanged) but avoidable exceeds the allowance. With an
    /// unchanged layout the number can only move through hand-tightened entries or analyzer
    /// improvements — either way the recorded allowance is the contract.
    Regression {
        /// Current avoidable bytes/row.
        avoidable: u64,
        /// The baselined allowance in force.
        allowed: u64,
    },
    /// Columns were appended (old signature is a prefix of the current one) and the appended
    /// columns themselves pushed avoidable past the still-active allowance. Actionable while the
    /// appending migration is unapplied — reorder the new columns — or acceptable explicitly.
    GrownSinceBaseline {
        /// Current avoidable bytes/row.
        avoidable: u64,
        /// The baselined allowance in force.
        allowed: u64,
    },
    /// The layout changed in a non-append way, expiring the allowance, and the table does not
    /// meet `fail_over`. Re-accept deliberately or fix the layout in the rewriting migration.
    ModifiedSinceBaseline {
        /// Current avoidable bytes/row.
        avoidable: u64,
    },
    /// Passing, and better than the allowance — the entry can be tightened (explicitly).
    RatchetOpportunity {
        /// Current avoidable bytes/row.
        avoidable: u64,
        /// The baselined allowance the table now beats.
        allowed: u64,
    },
}

impl TableVerdict {
    /// True for the verdict kinds that fail the gate; [`Pass`](Self::Pass) and
    /// [`RatchetOpportunity`](Self::RatchetOpportunity) do not.
    pub fn failing(self) -> bool {
        matches!(
            self,
            Self::NewViolation { .. }
                | Self::Regression { .. }
                | Self::GrownSinceBaseline { .. }
                | Self::ModifiedSinceBaseline { .. }
        )
    }
}

/// What [`evaluate`] concluded: the overall pass/fail plus everything a renderer needs to say why.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct GateOutcome {
    /// The gate failed: some verdict is failing, or the analysis degraded under
    /// `fail_on_degraded`. The CLI's exit-1 signal.
    pub exceeded: bool,
    /// Statements the parser skipped (loudly noted, but a gate that only counts avoidable
    /// bytes would stay green over them — these counts make the degradation visible to
    /// automation, and `fail_on_degraded` turns them into a failure).
    pub skipped_statements: usize,
    /// Non-ignored tables marked incomplete (skipped or unexpanded DDL touched them).
    pub incomplete_tables: usize,
    /// Scanned paths that matched no SQL files at all — a typo'd migrations directory would
    /// otherwise gate green forever having analyzed nothing.
    pub empty_scans: usize,
    /// One verdict per non-ignored analyzed table.
    pub verdicts: BTreeMap<String, TableVerdict>,
    /// Baseline entries with no matching analyzed table (renamed or dropped since acceptance).
    pub orphaned: Vec<String>,
    /// Entries whose layout changed non-append while the table now meets `fail_over` — stale,
    /// removed by the next baseline write.
    pub expired: Vec<String>,
}

/// Gate an analysis. An explicit `fail_over` wins over the baseline file's recorded one; with
/// neither present nothing can fail. Ignored tables are outside both gate and baseline.
/// `fail_on_degraded` additionally fails the gate when statements were skipped, tables are
/// incomplete, or a scanned path matched no SQL files — without it those are surfaced in the
/// outcome but stay green (a sqlparser user cannot always fix a parser gap; under pg-exact
/// skips should be zero, so strict is cheap).
pub fn evaluate(
    analysis: &Analysis,
    fail_over: Option<u64>,
    fail_on_degraded: bool,
    baseline: Option<&Baseline>,
) -> GateOutcome {
    let default_limit = fail_over.or_else(|| baseline.map(|b| b.fail_over));
    let mut verdicts = BTreeMap::new();
    let mut expired = Vec::new();
    for table in analysis.tables.iter().filter(|t| !t.ignored) {
        let entry = baseline.and_then(|b| b.tables.get(&table.name));
        let (verdict, entry_expired) = table_verdict(table, entry, default_limit);
        if entry_expired {
            expired.push(table.name.clone());
        }
        verdicts.insert(table.name.clone(), verdict);
    }
    let orphaned = baseline
        .map(|b| {
            b.tables
                .keys()
                .filter(|n| !verdicts.contains_key(*n))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let skipped_statements = analysis
        .notes
        .iter()
        .filter(|n| n.kind == crate::fold::NoteKind::SkippedStatement)
        .count();
    let incomplete_tables = analysis.tables.iter().filter(|t| !t.ignored && t.incomplete).count();
    let empty_scans = analysis
        .notes
        .iter()
        .filter(|n| n.kind == crate::fold::NoteKind::EmptyScan)
        .count();
    let degraded = skipped_statements > 0 || incomplete_tables > 0 || empty_scans > 0;
    let exceeded = verdicts.values().any(|v| v.failing()) || (fail_on_degraded && degraded);
    GateOutcome {
        exceeded,
        skipped_statements,
        incomplete_tables,
        empty_scans,
        verdicts,
        orphaned,
        expired,
    }
}

/// One table against its (possible) baseline entry. The second value reports a stale entry:
/// the layout changed non-append while the table now meets `default_limit`, so the entry no
/// longer describes anything and the next baseline write drops it.
fn table_verdict(
    table: &TableReport,
    entry: Option<&BaselineEntry>,
    default_limit: Option<u64>,
) -> (TableVerdict, bool) {
    let avoidable = table.avoidable_bytes_per_row;
    let over_limit = |limit: Option<u64>| limit.is_some_and(|l| avoidable > l);
    let Some(entry) = entry else {
        let verdict = if over_limit(default_limit) {
            TableVerdict::NewViolation { avoidable }
        } else {
            TableVerdict::Pass
        };
        return (verdict, false);
    };
    let allowed = entry.bytes;
    let verdict = match relation(&entry.layout, &table.layout_signature) {
        SignatureRelation::Match if avoidable > allowed => TableVerdict::Regression { avoidable, allowed },
        SignatureRelation::Grown if avoidable > allowed => TableVerdict::GrownSinceBaseline { avoidable, allowed },
        SignatureRelation::Match | SignatureRelation::Grown if avoidable < allowed => {
            TableVerdict::RatchetOpportunity { avoidable, allowed }
        }
        SignatureRelation::Match | SignatureRelation::Grown => TableVerdict::Pass,
        SignatureRelation::Different if over_limit(default_limit) => TableVerdict::ModifiedSinceBaseline { avoidable },
        SignatureRelation::Different => return (TableVerdict::Pass, true),
    };
    (verdict, false)
}

enum SignatureRelation {
    Match,
    /// The baselined signature is a comma-boundary prefix of the current one: columns were
    /// appended (`ADD COLUMN`), nothing else moved.
    Grown,
    Different,
}

fn relation(baselined: &str, current: &str) -> SignatureRelation {
    if baselined == current {
        SignatureRelation::Match
    } else if !baselined.is_empty()
        && current.len() > baselined.len()
        && current.starts_with(baselined)
        && current.as_bytes()[baselined.len()] == b','
    {
        SignatureRelation::Grown
    } else {
        SignatureRelation::Different
    }
}

/// Rewrite-from-scratch baselining: entries for every non-ignored table still over `fail_over`,
/// at current bytes and signature. Orphans and expired entries vanish by construction.
pub fn build_from(analysis: &Analysis, fail_over: u64, version: &str) -> Baseline {
    let tables = analysis
        .tables
        .iter()
        .filter(|t| !t.ignored && t.avoidable_bytes_per_row > fail_over)
        .map(|t| {
            (
                t.name.clone(),
                BaselineEntry {
                    bytes: t.avoidable_bytes_per_row,
                    layout: t.layout_signature.clone(),
                },
            )
        })
        .collect();
    Baseline {
        rowdiet: version.to_string(),
        fail_over,
        tables,
    }
}

/// Surgically refresh the named tables' entries to their current bytes and signature — the
/// explicit, reviewable act of accepting one table's growth without touching the rest of the
/// file. A named table now meeting the file's `fail_over` has its entry pruned instead
/// (baselines list only debt).
///
/// # Errors
///
/// When a name (fold key or display spelling) matches no non-ignored analyzed table. Names
/// processed before the failing one have already been applied — persist the baseline only on Ok.
pub fn accept_tables(baseline: &mut Baseline, analysis: &Analysis, names: &[String]) -> Result<(), String> {
    for name in names {
        let table = analysis
            .tables
            .iter()
            .find(|t| !t.ignored && (t.name == *name || t.display == *name))
            .ok_or_else(|| format!("cannot accept `{name}`: no such table in the analyzed DDL (or it is ignored)"))?;
        // Entries are stored under the canonical fold key regardless of which spelling the
        // caller used to name the table — an entry keyed by display would never match a gate.
        if table.avoidable_bytes_per_row > baseline.fail_over {
            baseline.tables.insert(
                table.name.clone(),
                BaselineEntry {
                    bytes: table.avoidable_bytes_per_row,
                    layout: table.layout_signature.clone(),
                },
            );
        } else {
            baseline.tables.remove(&table.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
