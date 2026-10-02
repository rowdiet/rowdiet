//! The public result model, and the fold→layout assembly that fills it.
//!
//! Reporting contract: for fixed-width-only tables everything is byte-exact —
//! the headline is the MAXALIGN-rounded footprint delta, and a reorder that does not cross an
//! 8-byte rung reports zero avoidable bytes. Tables with varlena columns follow the decision
//! policy (docs/design.md): the gate and the reorder recommendation rest only on
//! realization-independent facts — deterministic pads and dominance (never worse in any
//! storage-form/payload realization, verified by [`crate::dominance`]) — while genuinely
//! workload-dependent choices are reported as a frontier with the decision boundary and never
//! gate. Expected values stay as display fields under the layout module doc's stated model
//! assumptions and decide nothing.

use crate::dominance::{DiffBounds, Summary};
use crate::fold::{FoldedTable, Note, Origin};
use crate::layout::{self, ColumnKind, SearchScope, Tier, Walk};

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
    /// gated tables — 0.0 when every layout is tight (or nothing was analyzed). The number a
    /// zero-tolerance test asserts on. A clean maximum still says nothing about skipped
    /// statements, so pair it with a look at [`notes`](Self::notes) or use
    /// [`baseline::evaluate`](crate::baseline::evaluate) with `fail_on_degraded`.
    pub fn worst_avoidable(&self) -> f64 {
        self.gated_tables()
            .map(|table| table.avoidable_bytes_per_row)
            .fold(0.0, f64::max)
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
        self.notes.iter().any(|note| note.kind.is_degradation()) || self.gated_tables().any(|table| table.incomplete)
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
    /// The headline number gates compare: the sum of [`avoidable_deterministic`] and
    /// [`avoidable_dominance`] (for the exact tier, the MAXALIGN-rounded footprint delta).
    /// 0 = no dominating reorder was found, or the table is incomplete (unknown tier), where
    /// no waste can be claimed. Frontier findings never enter this number.
    ///
    /// [`avoidable_deterministic`]: Self::avoidable_deterministic
    /// [`avoidable_dominance`]: Self::avoidable_dominance
    pub avoidable_bytes_per_row: f64,
    /// Waste in exactly-known pads the recommended reorder removes, bytes/row: current minus
    /// suggested deterministic padding (exact tier: the footprint delta). Gates.
    pub avoidable_deterministic: u64,
    /// Further worst-case waste the recommended reorder removes beyond the deterministic part,
    /// bytes/row — nonzero only when the suggestion dominates the current order (never worse
    /// in any realization). Gates.
    pub avoidable_dominance: u64,
    /// Guaranteed-to-maximum saving of the recommended reorder over every realization,
    /// bytes/row; present exactly when the suggestion dominates the current order.
    pub dominance_saving: Option<SavingRange>,
    /// How much of the order space the suggestion search proved; anything short of complete is
    /// labeled in the rendered output.
    pub search_scope: layout::SearchScope,
    /// How far the dominance search went: [`DominanceScope::Exhaustive`] proves a clean table
    /// has no dominating reorder at all; [`DominanceScope::Budgeted`] means findings may have
    /// been missed, and the output says so instead of printing an unqualified clean verdict.
    pub dominance_search: DominanceScope,
    /// A workload-dependent alternative order for the reader to weigh: present when no
    /// candidate dominates but one is strictly better somewhere (or by worst case). Reported,
    /// never gated.
    pub frontier: Option<Frontier>,
    /// Type spellings that resolved by assumption, sorted and deduplicated — the table's
    /// numbers are only as good as those assumptions.
    pub assumed_types: Vec<String>,
    /// Varlena type spellings whose payload lengths the realization model does not narrow to
    /// what PostgreSQL stores, sorted and deduplicated. Findings over them hold; an exhaustive
    /// clean verdict does not (see [`DominanceScope::Superset`]).
    pub superset_types: Vec<String>,
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
    /// Padding before this column in the current order, bytes — when its value is certain;
    /// None when it depends on the payload lengths of preceding varlenas or, for a varlena,
    /// on its own storage form.
    pub pad_before: Option<u64>,
    /// Data start within the data area, bytes (tuple header not included); None once it is
    /// data-dependent (payload lengths of earlier varlenas, or this varlena's own pad).
    pub offset: Option<u64>,
}

/// Layout numbers for one column order — [`TableReport::current`] and
/// [`TableReport::suggested`] each hold one.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OrderStats {
    /// Certain inter-column padding, bytes per row: pads whose value the DDL fixes. For an
    /// all-fixed table this is the whole padding.
    pub padding: u64,
    /// Expected total padding, bytes per row: `padding` plus the expected values of the
    /// data-dependent pads (equals `padding` when there are none; fractional otherwise).
    /// Expected values follow the layout module doc's stated assumptions: varlena pads scored
    /// at the short/TOAST form, offset residues uniform.
    pub expected_padding: f64,
    /// Smallest possible total padding, bytes per row (equals `padding` when nothing is
    /// data-dependent).
    pub padding_min: u64,
    /// Largest possible total padding, bytes per row.
    pub padding_max: u64,
    /// Whole-row on-disk size, header included and MAXALIGN-rounded, bytes. None at the estimate
    /// tier (varlena payloads make it unknowable) and the unknown tier (columns not fully known).
    pub footprint: Option<u64>,
    /// Rows of that footprint per 8 kB heap page. None at the estimate and unknown tiers.
    pub rows_per_page: Option<u64>,
}

/// A dominance-proven saving: the reorder saves at least `min` and at most `max` bytes/row,
/// in every realization of storage forms and payload lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct SavingRange {
    /// Guaranteed saving, bytes/row (0 means some realization already packs the current order).
    pub min: u64,
    /// Largest attainable saving, bytes/row.
    pub max: u64,
}

/// A workload-dependent order choice the tool cannot make: the alternative wins some
/// realizations, the current order wins others (or the pair was undecidable). The bands carry
/// the decision boundary; the reader supplies the workload knowledge.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Frontier {
    /// The alternative order (display column names).
    pub order: Vec<String>,
    /// Current order's worst-case padding, bytes/row.
    pub current_worst: u64,
    /// Alternative order's worst-case padding, bytes/row.
    pub alternative_worst: u64,
    /// Current order's deterministic padding, bytes/row.
    pub current_deterministic: u64,
    /// Alternative order's deterministic padding, bytes/row.
    pub alternative_deterministic: u64,
    /// False when the pair was out of the dominance engine's budget: the worst cases above
    /// still hold, but no per-band winners could be computed.
    pub decided: bool,
    /// The decision boundary, one entry per storage-form combination (empty when undecided).
    pub bands: Vec<FrontierBand>,
}

/// One storage-form band of a frontier: who wins while exactly the named columns store the
/// in-line long form.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FrontierBand {
    /// Columns (display names) stored long-form in this band; every other varlena is short or
    /// TOAST.
    pub long_form: Vec<String>,
    /// Who wins the band.
    pub winner: BandWinner,
    /// Smallest `current - alternative` padding difference in the band, bytes/row.
    pub min_saving: i64,
    /// Largest `current - alternative` padding difference in the band, bytes/row.
    pub max_saving: i64,
}

/// Band verdicts: positive savings favor the alternative order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum BandWinner {
    /// The alternative order is never worse in the band and better somewhere in it.
    Alternative,
    /// The current order is never worse in the band and better somewhere in it.
    Current,
    /// Identical padding across the band.
    Tie,
    /// The winner flips with payload lengths (mod 8) inside the band.
    Mixed,
}

/// How far the dominance search went. Anything short of exhaustive must temper the clean
/// verdict: an unqualified "no dominating reorder exists" may only follow a completed sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum DominanceScope {
    /// Every member of the pointwise-complete order space was dominance-tested against the
    /// current order (a candidate failing a necessary condition counts as tested), or, at the
    /// exact tier, the search found the padding minimum: absence of a finding proves no
    /// dominating reorder exists.
    Exhaustive,
    /// The sweep was skipped or trimmed by its budgets (order space too large, or a comparison
    /// out of the enumeration budget): a clean verdict claims only what was searched.
    Budgeted,
    /// The sweep tested every candidate, but some column's type has unverified payload lengths
    /// ([`TableReport::superset_types`]): the model lets it store lengths PostgreSQL may never
    /// write, and a realization that only the model allows can hide a dominating reorder. A
    /// clean verdict says "found".
    Superset,
}

/// What the decision policy concluded for one estimate-tier table.
struct Decision {
    order: Vec<usize>,
    avoidable_deterministic: u64,
    avoidable_dominance: u64,
    dominance_saving: Option<SavingRange>,
    dominance_search: DominanceScope,
    frontier_order: Option<Vec<usize>>,
}

/// The dominance sweep covers the pointwise-complete order space up to this many candidates.
const SWEEP_SEQUENCE_CAP: usize = 5040;
/// Column slots the materialized candidate orders may hold in total (32 MB of indices).
const SWEEP_ELEMENT_CAP: usize = 1 << 22;
/// Total realization-walk work the sweep may spend on candidate comparisons.
const SWEEP_WORK_BUDGET: u64 = 1 << 22;

/// The estimate-tier decision policy (docs/design.md): sweep the dominance-complete order
/// space within budgets, recommend the best dominating order (smallest worst case), and
/// surface the best non-dominating search pole as a frontier instead of a recommendation.
/// Scalar-objective poles alone were measured to miss 11-19% of dominating reorders on 4-5
/// column varlena schemas, which is why the sweep exists. A sweep that is skipped or trimmed by
/// its budgets still tests the search poles and the safe repacks, so a budget never hides a
/// dominating order those candidates hold.
fn decide(kinds: &[ColumnKind], search: &layout::Search, current_walk: &Walk) -> Decision {
    let n = kinds.len();
    let identity: Vec<usize> = (0..n).collect();
    let mut dominating: Vec<(Vec<usize>, DiffBounds)> = Vec::new();
    let sequence_cap = SWEEP_SEQUENCE_CAP.min(SWEEP_ELEMENT_CAP / n.max(1));
    let exhaustive =
        layout::order_space(kinds, sequence_cap).is_some_and(|orders| sweep(kinds, orders, &mut dominating));
    if !exhaustive {
        for candidate in fallback_candidates(kinds, search) {
            if candidate == identity || dominating.iter().any(|(order, _)| *order == candidate) {
                continue;
            }
            if let Some(diff) = crate::dominance::compare(kinds, &identity, &candidate)
                && diff.b_dominates()
            {
                dominating.push((candidate, diff));
            }
        }
    }
    let dominance_search = if exhaustive {
        DominanceScope::Exhaustive
    } else {
        DominanceScope::Budgeted
    };
    let best = dominating
        .into_iter()
        .map(|(order, diff)| {
            let ordered: Vec<ColumnKind> = order.iter().map(|&i| kinds[i]).collect();
            let summary = crate::dominance::summary(kinds, &order);
            (order, diff, layout::walk(&ordered), summary)
        })
        .min_by_key(|(_, _, w, summary)| (summary.max, w.padding, summary.short_mean_eighths))
        .map(|(order, diff, w, _)| (order, diff, w));
    if let Some((order, diff, cand_walk)) = best {
        // The gate never claims more than the engine's proven maximum saving: certain pads the
        // reorder converts into smaller data-dependent ones are capped at what is attainable.
        let max_saving = diff.max as u64;
        let det = current_walk.padding.saturating_sub(cand_walk.padding).min(max_saving);
        return Decision {
            order,
            avoidable_deterministic: det,
            avoidable_dominance: max_saving - det,
            dominance_saving: Some(SavingRange {
                min: diff.min as u64,
                max: max_saving,
            }),
            dominance_search,
            frontier_order: None,
        };
    }
    // No dominating order: surface the most useful pole as a frontier. A pole qualifies when
    // it wins somewhere and improves a realization-free summary (worst case, or certain
    // padding, the certainty-trade case). Documented as deviation five in docs/design.md.
    let mut frontier_pick: Option<Vec<usize>> = None;
    for candidate in [
        search.minimax_pole.as_ref(),
        search.certainty_pole.as_ref(),
        Some(&search.heuristic),
    ]
    .into_iter()
    .flatten()
    {
        if *candidate == identity || frontier_pick.is_some() {
            continue;
        }
        let ordered: Vec<ColumnKind> = candidate.iter().map(|&i| kinds[i]).collect();
        let cand_walk = layout::walk(&ordered);
        let better_summary =
            cand_walk.padding_max() < current_walk.padding_max() || cand_walk.padding < current_walk.padding;
        // Every pole was tested above or by the exhaustive sweep, so none of them dominates.
        let wins_somewhere = crate::dominance::compare(kinds, &identity, candidate).is_some_and(|diff| {
            debug_assert!(!diff.b_dominates(), "a dominating pole must be recommended");
            diff.max > 0
        });
        if better_summary && wins_somewhere {
            frontier_pick = Some(candidate.clone());
        }
    }
    Decision {
        order: identity,
        avoidable_deterministic: 0,
        avoidable_dominance: 0,
        dominance_saving: None,
        dominance_search,
        frontier_order: frontier_pick,
    }
}

/// Test every member of the pointwise-complete order space against the current order and return
/// whether every member was decided. The class-sequence members go first, in the order the
/// collapsed sweep tested them, so a trimmed sweep finds at least what that sweep found; the
/// rest follow by ascending worst case.
fn sweep(kinds: &[ColumnKind], orders: Vec<Vec<usize>>, dominating: &mut Vec<(Vec<usize>, DiffBounds)>) -> bool {
    let identity: Vec<usize> = (0..kinds.len()).collect();
    let current = crate::dominance::summary(kinds, &identity);
    let passes_prunes = |candidate: &[usize]| -> Option<(Summary, u64)> {
        // Dominance implies pointwise <=, so it implies <= on the max, the min, and the mean
        // over any sub-distribution of realizations; the summary computes all three over the
        // same realization model the comparison uses, so the prunes stay sound where a type
        // narrows its payload residues.
        let cand = crate::dominance::summary(kinds, candidate);
        let pruned =
            cand.max > current.max || cand.min > current.min || cand.short_mean_eighths > current.short_mean_eighths;
        let ordered: Vec<ColumnKind> = candidate.iter().map(|&i| kinds[i]).collect();
        (!pruned).then(|| (cand, layout::walk(&ordered).padding))
    };
    let class_sequences = layout::class_sequence_space(kinds, orders.len()).unwrap_or_default();
    let mut rest: Vec<((u64, u64, u64), Vec<usize>)> = Vec::new();
    for candidate in orders {
        if keeps_class_order(kinds, &candidate) {
            continue;
        }
        if let Some((cand, certain)) = passes_prunes(&candidate) {
            rest.push(((cand.max, certain, cand.short_mean_eighths), candidate));
        }
    }
    rest.sort();
    let ranked = class_sequences
        .into_iter()
        .filter(|candidate| passes_prunes(candidate).is_some())
        .chain(rest.into_iter().map(|(_, candidate)| candidate));
    let mut budget = SWEEP_WORK_BUDGET;
    let mut complete = true;
    for candidate in ranked {
        if candidate == identity {
            continue;
        }
        let cost = crate::dominance::comparison_cost(kinds, &identity, &candidate);
        if cost > budget {
            complete = false;
            continue;
        }
        budget -= cost;
        match crate::dominance::compare(kinds, &identity, &candidate) {
            Some(diff) if diff.b_dominates() => dominating.push((candidate, diff)),
            Some(_) => {}
            None => complete = false,
        }
    }
    complete
}

/// True when varlenas of one padding class keep their written relative order: the class-sequence
/// representatives an earlier, collapsed candidate space held.
fn keeps_class_order(kinds: &[ColumnKind], order: &[usize]) -> bool {
    let class = |kind: ColumnKind| match kind {
        ColumnKind::Varlena {
            align, proven_short, ..
        } if !proven_short && align != layout::Align::Char => Some(align.bytes()),
        ColumnKind::Varlena { .. } => Some(0),
        ColumnKind::Fixed { .. } => None,
    };
    let mut last_seen: Vec<(u64, usize)> = Vec::new();
    for &column in order {
        let Some(key) = class(kinds[column]) else { continue };
        match last_seen.iter_mut().find(|(k, _)| *k == key) {
            Some((_, last)) if *last > column => return false,
            Some((_, last)) => *last = column,
            None => last_seen.push((key, column)),
        }
    }
    true
}

/// Candidates for a sweep that was skipped or trimmed: the search poles, the current order with
/// its leading fixed run repacked, and every fixed column first with the varlenas in written
/// order. The last two keep the varlena sequence, so the dominance engine decides them exactly
/// at any width.
fn fallback_candidates(kinds: &[ColumnKind], search: &layout::Search) -> Vec<Vec<usize>> {
    let mut fixed_first: Vec<usize> = search
        .heuristic
        .iter()
        .copied()
        .filter(|&i| kinds[i].is_fixed())
        .collect();
    fixed_first.extend((0..kinds.len()).filter(|&i| !kinds[i].is_fixed()));
    let mut candidates: Vec<Vec<usize>> = Vec::new();
    for candidate in [
        search.minimax_pole.clone(),
        search.certainty_pole.clone(),
        Some(search.heuristic.clone()),
        Some(current_prefix_repack(kinds)),
        Some(fixed_first),
    ]
    .into_iter()
    .flatten()
    {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// The current order with its leading fixed run repacked to the deterministic minimum and the
/// suffix untouched: dominance-safe by construction, and free at any table size.
fn current_prefix_repack(kinds: &[ColumnKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..kinds.len()).collect();
    layout::refine_leading_fixed(kinds, &mut order);
    order
}

fn frontier_report(
    kinds: &[ColumnKind],
    columns: &[crate::fold::FoldedColumn],
    current_walk: &Walk,
    alternative: &[usize],
) -> Frontier {
    let identity: Vec<usize> = (0..kinds.len()).collect();
    let ordered: Vec<ColumnKind> = alternative.iter().map(|&i| kinds[i]).collect();
    let alt_walk = layout::walk(&ordered);
    let bands = crate::dominance::bands(kinds, &identity, alternative);
    let decided = bands.is_some();
    let bands = bands
        .unwrap_or_default()
        .into_iter()
        .map(|band| {
            let winner = if band.diff.b_dominates() {
                BandWinner::Alternative
            } else if band.diff.a_dominates() {
                BandWinner::Current
            } else if band.diff.equal() {
                BandWinner::Tie
            } else {
                BandWinner::Mixed
            };
            FrontierBand {
                long_form: band.long_form.iter().map(|&i| columns[i].display.clone()).collect(),
                winner,
                min_saving: band.diff.min,
                max_saving: band.diff.max,
            }
        })
        .collect();
    Frontier {
        order: alternative.iter().map(|&i| columns[i].display.clone()).collect(),
        current_worst: current_walk.padding_max(),
        alternative_worst: alt_walk.padding_max(),
        current_deterministic: current_walk.padding,
        alternative_deterministic: alt_walk.padding,
        decided,
        bands,
    }
}

pub(crate) fn build(table: FoldedTable) -> TableReport {
    let kinds: Vec<ColumnKind> = table.columns.iter().map(|c| c.kind).collect();
    // An incomplete table's columns are not the whole table, so no footprint can be claimed —
    // scoring the partial (or empty) column list as if it were complete is how a table nobody
    // could model would otherwise report a confident `exact`/`pass`.
    let tier = if table.incomplete {
        Tier::Unknown
    } else {
        layout::tier(&kinds)
    };
    // Dropped attributes are stored as NULL in every subsequent row, so the bitmap (sized by
    // the ORIGINAL natts) is unconditionally present; the all-non-NULL scenario otherwise
    // starts data at MAXALIGN(23) = 24.
    let t_hoff = if table.dropped_count > 0 {
        layout::null_thoff(kinds.len() + table.dropped_count)
    } else {
        layout::bare_thoff()
    };
    let current_walk = layout::walk(&kinds);
    let search = layout::search(&kinds);
    let identity: Vec<usize> = (0..kinds.len()).collect();
    let current = stats(tier, &kinds, &identity, &current_walk, t_hoff);
    let decision = match tier {
        // Exact tier: everything is deterministic, so the certainty pole is the exact padding
        // minimum when the search completed, and the footprint delta is the whole story
        // (computed below). A capped search offers its heuristic and claims no exhaustiveness.
        Tier::Exact => Decision {
            order: search
                .certainty_pole
                .clone()
                .unwrap_or_else(|| search.heuristic.clone()),
            avoidable_deterministic: 0,
            avoidable_dominance: 0,
            dominance_saving: None,
            dominance_search: if search.scope == SearchScope::Complete {
                DominanceScope::Exhaustive
            } else {
                DominanceScope::Budgeted
            },
            frontier_order: None,
        },
        Tier::Estimate => decide(&kinds, &search, &current_walk),
        // Columns unknown: no avoidable waste can be claimed, nothing was searched, and the
        // incomplete verdict carries the "not analyzed" signal.
        Tier::Unknown => Decision {
            order: identity.clone(),
            avoidable_deterministic: 0,
            avoidable_dominance: 0,
            dominance_saving: None,
            dominance_search: DominanceScope::Budgeted,
            frontier_order: None,
        },
    };
    let ordered_kinds: Vec<ColumnKind> = decision.order.iter().map(|&i| kinds[i]).collect();
    let suggested_walk = layout::walk(&ordered_kinds);
    let suggested = stats(tier, &kinds, &decision.order, &suggested_walk, t_hoff);
    // Exact-tier avoidable is the footprint delta; a reorder that does not cross an 8-byte
    // rung reports zero even when raw padding drops.
    let (avoidable_deterministic, avoidable_dominance) = match tier {
        Tier::Exact => (
            current
                .footprint
                .unwrap_or(0)
                .saturating_sub(suggested.footprint.unwrap_or(0)),
            0,
        ),
        Tier::Estimate | Tier::Unknown => (decision.avoidable_deterministic, decision.avoidable_dominance),
    };
    let avoidable_units = avoidable_deterministic + avoidable_dominance;
    let avoidable = avoidable_units as f64;
    // With nothing avoidable the suggestion IS the current order; the stats must say the same
    // thing, or the JSON contradicts itself (suggested.padding 0 beside the original order).
    let (final_order, suggested): (Vec<usize>, OrderStats) = if avoidable_units == 0 {
        (identity, current.clone())
    } else {
        (decision.order, suggested)
    };
    let frontier = decision
        .frontier_order
        .as_ref()
        .map(|alternative| frontier_report(&kinds, &table.columns, &current_walk, alternative));
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
            pad_before: w.pad_before.exact(),
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
    let mut superset_types: Vec<String> = table
        .columns
        .iter()
        .filter(|c| matches!(c.kind, ColumnKind::Varlena { payload, .. } if !payload.verified))
        .map(|c| c.type_display.clone())
        .collect();
    superset_types.sort();
    superset_types.dedup();
    let dominance_search = match decision.dominance_search {
        DominanceScope::Exhaustive if !superset_types.is_empty() => DominanceScope::Superset,
        scope => scope,
    };
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
        avoidable_deterministic,
        avoidable_dominance,
        dominance_saving: if avoidable_units == 0 {
            None
        } else {
            decision.dominance_saving
        },
        search_scope: search.scope,
        dominance_search,
        frontier,
        assumed_types,
        superset_types,
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
            ColumnKind::Varlena {
                align, proven_short, ..
            } => {
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

fn stats(tier: Tier, kinds: &[ColumnKind], order: &[usize], walk: &Walk, t_hoff: u64) -> OrderStats {
    // The end is known exactly iff the table has no varlena, which is exactly the exact tier;
    // the estimate tier has no footprint to claim, and the unknown tier claims nothing.
    let footprint = match (tier, walk.end) {
        (Tier::Exact, Some(end)) => Some(layout::footprint_at(t_hoff, end)),
        _ => None,
    };
    // Bounds over the realization model, which knows the payload residues a type stores.
    let bounds = crate::dominance::summary(kinds, order);
    OrderStats {
        padding: walk.padding,
        expected_padding: walk.expected_padding(),
        padding_min: bounds.min,
        padding_max: bounds.max,
        footprint,
        rows_per_page: footprint.map(layout::rows_per_page),
    }
}
