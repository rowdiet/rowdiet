//! The public result model, and the fold→layout assembly that fills it.
//!
//! Reporting contract: for fixed-width-only tables everything is byte-exact —
//! the headline is the MAXALIGN-rounded footprint delta, and a reorder that does not cross an
//! 8-byte rung reports zero avoidable bytes. Tables with varlena columns follow the decision
//! policy (docs/design.md): the gate and the reorder recommendation rest only on
//! realization-independent facts — deterministic pads and dominance (never worse in any
//! storage-form, payload, or NULL realization, verified by [`crate::dominance`]) — while
//! genuinely workload-dependent choices are reported as a frontier with the decision boundary
//! and never gate. A nullable column's NULL is one of those realization variables; NOT NULL
//! removes it. Fixed-width tables with nullable columns follow the same policy in row size,
//! which is byte-exact per NULL pattern. Expected values stay as display fields under the layout
//! module doc's stated model assumptions and decide nothing.

use crate::dominance::{DiffBounds, Measure, Nulls, Summary};
use crate::fold::{FoldedTable, Note, Origin};
use crate::layout::{self, Column, ColumnKind, SearchScope, Start, Tier, Walk};

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
    /// The relation's name parts, schema first, as SQL addresses it.
    pub relation: Vec<String>,
    /// Live column count (dropped attribute slots are in `dropped_columns`).
    pub natts: usize,
    /// Some column is nullable: rows holding a NULL carry a null bitmap in their header.
    pub any_nullable: bool,
    /// Columns (display names) whose NULL is a step no stored value takes, so a NULL there moves
    /// later offsets in a way the column's values cannot: every nullable fixed-width column, and
    /// a nullable varlena whose short payloads never advance a multiple of 8 bytes (numeric,
    /// arrays of wider elements). NOT NULL removes a column from here.
    pub null_variables: Vec<String>,
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

impl TableReport {
    /// A complete table whose dominance search ran out of budget: findings stand, but a clean
    /// verdict claims only what was searched.
    pub fn budgeted(&self) -> bool {
        !self.incomplete && self.dominance_search == DominanceScope::Budgeted
    }
}

/// One column, as placed in the table's current physical order.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ColumnReport {
    /// Column name (display spelling).
    pub name: String,
    /// The name PostgreSQL stores: case-folded unless the DDL quoted it.
    pub key: String,
    /// Declared type as written.
    pub type_display: String,
    /// Declared or implied NOT NULL.
    pub not_null: bool,
    /// False when the type resolved by assumption (listed in [`TableReport::assumed_types`]).
    pub known_type: bool,
    /// Storage class as the column's rows can hold it: the declared class, widened where a
    /// `STORAGE PLAIN` era lets a typmod-short varlena keep the aligned 4-byte header.
    pub kind: ColumnKind,
    /// Padding before this column in the current order in rows that store every column, bytes,
    /// when its value is certain there; None when it depends on the payload lengths of preceding
    /// varlenas or, for a varlena, on its own storage form.
    pub pad_before: Option<u64>,
    /// Data start within the data area in rows that store every column, bytes (tuple header not
    /// included); None once it is data-dependent (payload lengths of earlier varlenas, or this
    /// varlena's own pad).
    pub offset: Option<u64>,
    /// The statement that added the column: its CREATE TABLE or its ADD COLUMN.
    pub added_in: Origin,
    /// The column's TOAST strategy when DDL set one; None for the type's own.
    pub storage: Option<crate::extract::Storage>,
    /// Physical attribute number, 1-based, as PostgreSQL numbers it: a column added after a
    /// drop is numbered past the dropped slot.
    pub attnum: usize,
}

/// Layout numbers for one column order — [`TableReport::current`] and
/// [`TableReport::suggested`] each hold one.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OrderStats {
    /// Certain inter-column padding in rows that store every column, bytes per row: pads whose
    /// value the DDL fixes there. For an all-fixed NOT NULL table this is the whole padding.
    pub padding: u64,
    /// Expected total padding, bytes per row: `padding` plus the expected values of the
    /// data-dependent pads (equals `padding` when there are none; fractional otherwise).
    /// Expected values follow the layout module doc's stated assumptions: varlena pads scored
    /// at the short/TOAST form, offset residues uniform, no NULLs.
    pub expected_padding: f64,
    /// Smallest possible total padding over every storage form, payload length, and NULL
    /// pattern, bytes per row.
    pub padding_min: u64,
    /// Largest possible total padding over the same, bytes per row.
    pub padding_max: u64,
    /// Padding bounds in rows that store every column, when they differ from the bounds over
    /// every NULL pattern.
    pub without_nulls: Option<PaddingBounds>,
    /// Whole-row on-disk size of a row that stores every column, header included and
    /// MAXALIGN-rounded, bytes. None at the estimate tier (varlena payloads make it unknowable)
    /// and the unknown tier (columns not fully known).
    pub footprint: Option<u64>,
    /// Rows of that footprint per 8 kB heap page. None at the estimate and unknown tiers.
    pub rows_per_page: Option<u64>,
    /// Exact tier with nullable columns: the rows that hold at least one NULL, whose header
    /// carries the null bitmap.
    pub with_nulls: Option<NullRows>,
}

/// Padding bounds for one scenario, bytes per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PaddingBounds {
    /// Smallest total padding.
    pub min: u64,
    /// Largest total padding.
    pub max: u64,
}

/// The rows of an all-fixed table that hold at least one NULL: their header and footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NullRows {
    /// Header size of such a row: MAXALIGN(23 + one bitmap bit per attribute), which jumps from
    /// 24 to 32 at nine attributes and to 40 at 73.
    pub t_hoff: u64,
    /// Smallest footprint among them, bytes.
    pub footprint_min: u64,
    /// Largest footprint among them, bytes.
    pub footprint_max: u64,
}

/// A dominance-proven saving: the reorder saves at least `min` and at most `max` bytes/row,
/// in every realization of storage forms, payload lengths, and NULLs (row-size bytes at the
/// exact tier, padding bytes at the estimate tier).
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
    /// What the band and no-NULL savings count: padding at the estimate tier, row size at the
    /// exact tier.
    pub measure: Measure,
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
    /// The decision boundary, one entry per storage-form combination (empty when undecided),
    /// each over every payload and NULL pattern.
    pub bands: Vec<FrontierBand>,
    /// The comparison in rows that store every column, over every storage form and payload,
    /// when it differs from the comparison over every NULL pattern.
    pub without_nulls: Option<NullFreeComparison>,
    /// The SQL that settles this frontier on real rows, and how to read its answer.
    pub query: Option<crate::resolve::FrontierQuery>,
}

/// A frontier's verdict in rows without NULLs.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NullFreeComparison {
    /// Who wins those rows.
    pub winner: BandWinner,
    /// Smallest `current - alternative` difference there, bytes/row.
    pub min_saving: i64,
    /// Largest `current - alternative` difference there, bytes/row.
    pub max_saving: i64,
}

/// One storage-form band of a frontier: who wins while exactly the named columns store the
/// in-line long form.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FrontierBand {
    /// Columns (display names) stored long-form in this band; every other varlena is short,
    /// TOAST, or NULL.
    pub long_form: Vec<String>,
    /// Who wins the band.
    pub winner: BandWinner,
    /// Smallest `current - alternative` difference in the band, bytes/row.
    pub min_saving: i64,
    /// Largest `current - alternative` difference in the band, bytes/row.
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
    /// The winner flips inside the band, with payload lengths (mod 8) or with which columns
    /// hold NULL.
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

/// What the decision policy concluded for one table with realization variables.
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

/// The decision policy (docs/design.md): sweep the dominance-complete order space within
/// budgets, recommend the best dominating order (smallest worst case), and surface the best
/// non-dominating search pole as a frontier instead of a recommendation. `measure` is padding at
/// the estimate tier and row size at the exact tier, where rows are byte-exact per NULL pattern.
/// Scalar-objective poles alone were measured to miss 11-19% of dominating reorders on 4-5
/// column varlena schemas, which is why the sweep exists. A sweep that is skipped or trimmed by
/// its budgets still tests the search poles and the safe repacks, so a budget never hides a
/// dominating order those candidates hold. A candidate that costs nothing in any realization
/// dominates without a pair comparison, so no budget hides it either.
fn decide(
    start: Start,
    columns: &[Column],
    search: &layout::Search,
    current_walk: &Walk,
    measure: Measure,
) -> Decision {
    let n = columns.len();
    let identity: Vec<usize> = (0..n).collect();
    let current = crate::dominance::summary(start, columns, &identity, Nulls::Vary, measure);
    // Nothing costs less than zero in any realization, so nothing dominates a zero-cost order.
    if current.max == 0 {
        return Decision {
            order: identity,
            avoidable_deterministic: 0,
            avoidable_dominance: 0,
            dominance_saving: None,
            dominance_search: DominanceScope::Exhaustive,
            frontier_order: None,
        };
    }
    let mut dominating: Vec<(Vec<usize>, DiffBounds)> = Vec::new();
    let sequence_cap = SWEEP_SEQUENCE_CAP.min(SWEEP_ELEMENT_CAP / n.max(1));
    let exhaustive = layout::order_space(columns, sequence_cap)
        .is_some_and(|orders| sweep(start, columns, orders, measure, current, &mut dominating));
    if !exhaustive {
        for candidate in fallback_candidates(start, columns, search) {
            if candidate == identity || dominating.iter().any(|(order, _)| *order == candidate) {
                continue;
            }
            let cand = crate::dominance::summary(start, columns, &candidate, Nulls::Vary, measure);
            let diff = if cand.max == 0 {
                Some(free_proof(current))
            } else {
                crate::dominance::compare(start, columns, &identity, &candidate, Nulls::Vary, measure)
            };
            if let Some(diff) = diff
                && diff.b_dominates()
            {
                dominating.push((candidate, diff));
            }
        }
        sequence_sweep(start, columns, measure, current, &mut dominating);
    }
    let dominance_search = if exhaustive {
        DominanceScope::Exhaustive
    } else {
        DominanceScope::Budgeted
    };
    let best = dominating
        .into_iter()
        .map(|(order, diff)| {
            let summary = crate::dominance::summary(start, columns, &order, Nulls::Vary, measure);
            let walk = layout::walk_from(start, &kinds_in(columns, &order));
            (order, diff, walk, summary)
        })
        .min_by_key(|(_, _, w, summary)| (summary.max, w.padding, summary.short_mean_eighths))
        .map(|(order, diff, _, _)| (order, diff));
    if let Some((order, diff)) = best {
        let max_saving = diff.max as u64;
        let det = match measure {
            // The gate never claims more than the engine's proven maximum saving: certain pads
            // the reorder converts into smaller data-dependent ones are capped at what is
            // attainable. Certain means certain over every NULL pattern too.
            Measure::Padding => layout::certain_padding(start, columns, &identity)
                .saturating_sub(layout::certain_padding(start, columns, &order))
                .min(max_saving),
            // Row sizes are exact per NULL pattern: what every row saves is the floor.
            Measure::RowSize => diff.min as u64,
        };
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
        let cand = crate::dominance::summary(start, columns, candidate, Nulls::Vary, measure);
        let cand_walk = layout::walk_from(start, &kinds_in(columns, candidate));
        let better_summary = cand.max < current.max || cand_walk.padding < current_walk.padding;
        // Every pole was tested above or by the exhaustive sweep, so none of them dominates.
        let wins_somewhere = crate::dominance::compare(start, columns, &identity, candidate, Nulls::Vary, measure)
            .is_some_and(|diff| {
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

/// The saving of a candidate that costs nothing in any realization: exactly the current order's
/// cost, realization by realization, so the bounds are the current summary's.
fn free_proof(current: Summary) -> DiffBounds {
    DiffBounds {
        min: current.min as i64,
        max: current.max as i64,
    }
}

fn kinds_in(columns: &[Column], order: &[usize]) -> Vec<ColumnKind> {
    order.iter().map(|&i| columns[i].kind).collect()
}

/// Test every member of the pointwise-complete order space against the current order and return
/// whether every member was decided. The class-sequence members go first, in the order the
/// collapsed sweep tested them, so a trimmed sweep finds at least what that sweep found; the
/// rest follow by ascending worst case.
fn sweep(
    start: Start,
    columns: &[Column],
    orders: Vec<Vec<usize>>,
    measure: Measure,
    current: Summary,
    dominating: &mut Vec<(Vec<usize>, DiffBounds)>,
) -> bool {
    let identity: Vec<usize> = (0..columns.len()).collect();
    let passes_prunes = |candidate: &[usize]| -> Option<(Summary, u64)> {
        // Dominance implies pointwise <=, so it implies <= on the max, the min, and the mean
        // over any sub-distribution of realizations; the summary computes all three over the
        // same realization model the comparison uses, so the prunes stay sound where a type
        // narrows its payload residues or a column holds NULL.
        let cand = crate::dominance::summary(start, columns, candidate, Nulls::Vary, measure);
        let pruned =
            cand.max > current.max || cand.min > current.min || cand.short_mean_eighths > current.short_mean_eighths;
        (!pruned).then(|| (cand, layout::walk_from(start, &kinds_in(columns, candidate)).padding))
    };
    let class_sequences = layout::class_sequence_space(columns, orders.len()).unwrap_or_default();
    let mut rest: Vec<((u64, u64, u64), Vec<usize>)> = Vec::new();
    for candidate in orders {
        if layout::keeps_class_order(columns, &candidate) {
            continue;
        }
        if let Some((cand, certain)) = passes_prunes(&candidate) {
            rest.push(((cand.max, certain, cand.short_mean_eighths), candidate));
        }
    }
    rest.sort();
    let ranked = class_sequences
        .into_iter()
        .filter_map(|candidate| passes_prunes(&candidate).map(|(cand, _)| (candidate, cand.max)))
        .chain(rest.into_iter().map(|((worst, _, _), candidate)| (candidate, worst)));
    let mut budget = SWEEP_WORK_BUDGET;
    let mut complete = true;
    for (candidate, worst) in ranked {
        if candidate == identity {
            continue;
        }
        if worst == 0 {
            dominating.push((candidate, free_proof(current)));
            continue;
        }
        let cost = crate::dominance::comparison_cost(start, columns, &identity, &candidate, Nulls::Vary, measure);
        if cost > budget {
            complete = false;
            continue;
        }
        budget -= cost;
        match crate::dominance::compare(start, columns, &identity, &candidate, Nulls::Vary, measure) {
            Some(diff) if diff.b_dominates() => dominating.push((candidate, diff)),
            Some(_) => {}
            None => complete = false,
        }
    }
    complete
}

/// Orders kept to the written varlena sequence when the whole space is too large to sweep.
const SEQUENCE_SWEEP_CAP: usize = 40_320;

/// After a skipped or trimmed sweep, test the orders that keep the written varlena sequence
/// ([`layout::sequence_space`]): the joint walk decides each exactly at the cost of a step per
/// column, so a few hundred comparisons, ranked by worst case after the summary prunes, cover
/// tables whose whole order space is out of reach. Measured on 3,000 realistic tables of 4 to 8
/// columns, 229 of 378 that the fallback candidates left clean have a dominating order here.
fn sequence_sweep(
    start: Start,
    columns: &[Column],
    measure: Measure,
    current: Summary,
    dominating: &mut Vec<(Vec<usize>, DiffBounds)>,
) {
    let n = columns.len();
    let identity: Vec<usize> = (0..n).collect();
    let Some(orders) = layout::sequence_space(columns, SEQUENCE_SWEEP_CAP.min(SWEEP_ELEMENT_CAP / n.max(1))) else {
        return;
    };
    let mut ranked: Vec<((u64, u64, u64), Vec<usize>)> = Vec::new();
    for candidate in orders {
        if candidate == identity || dominating.iter().any(|(order, _)| *order == candidate) {
            continue;
        }
        let cand = crate::dominance::summary(start, columns, &candidate, Nulls::Vary, measure);
        if cand.max > current.max || cand.min > current.min || cand.short_mean_eighths > current.short_mean_eighths {
            continue;
        }
        let certain = layout::walk_from(start, &kinds_in(columns, &candidate)).padding;
        ranked.push(((cand.max, certain, cand.short_mean_eighths), candidate));
    }
    ranked.sort();
    let mut budget = SWEEP_WORK_BUDGET;
    for ((worst, _, _), candidate) in ranked {
        if worst == 0 {
            dominating.push((candidate, free_proof(current)));
            continue;
        }
        let cost = crate::dominance::comparison_cost(start, columns, &identity, &candidate, Nulls::Vary, measure);
        if cost > budget {
            continue;
        }
        budget -= cost;
        if let Some(diff) = crate::dominance::compare(start, columns, &identity, &candidate, Nulls::Vary, measure)
            && diff.b_dominates()
        {
            dominating.push((candidate, diff));
        }
    }
}

/// Candidates for a sweep that was skipped or trimmed: the search poles, the current order with
/// its leading fixed run repacked, and every fixed column first with the varlenas in written
/// order. The last two keep the varlena sequence, so the dominance engine decides them exactly
/// at any width while few enough NULL bits are in flight.
fn fallback_candidates(start: Start, columns: &[Column], search: &layout::Search) -> Vec<Vec<usize>> {
    let mut fixed_first: Vec<usize> = search
        .heuristic
        .iter()
        .copied()
        .filter(|&i| columns[i].kind.is_fixed())
        .collect();
    fixed_first.extend((0..columns.len()).filter(|&i| !columns[i].kind.is_fixed()));
    let mut candidates: Vec<Vec<usize>> = Vec::new();
    for candidate in [
        search.minimax_pole.clone(),
        search.certainty_pole.clone(),
        Some(search.heuristic.clone()),
        Some(current_prefix_repack(start, columns)),
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
/// suffix untouched: dominance-safe by construction when the run is NOT NULL, verified like any
/// candidate otherwise, and free at any table size.
fn current_prefix_repack(start: Start, columns: &[Column]) -> Vec<usize> {
    let kinds: Vec<ColumnKind> = columns.iter().map(|c| c.kind).collect();
    let mut order: Vec<usize> = (0..columns.len()).collect();
    layout::refine_leading_fixed_from(start, &kinds, &mut order);
    order
}

fn band_winner(diff: DiffBounds) -> BandWinner {
    if diff.b_dominates() {
        BandWinner::Alternative
    } else if diff.a_dominates() {
        BandWinner::Current
    } else if diff.equal() {
        BandWinner::Tie
    } else {
        BandWinner::Mixed
    }
}

fn frontier_report(
    start: Start,
    columns: &[Column],
    names: &[String],
    current_walk: &Walk,
    alternative: &[usize],
    measure: Measure,
) -> Frontier {
    let identity: Vec<usize> = (0..columns.len()).collect();
    let alt_walk = layout::walk_from(start, &kinds_in(columns, alternative));
    let worst = |order: &[usize]| crate::dominance::summary(start, columns, order, Nulls::Vary, Measure::Padding).max;
    let bands = crate::dominance::bands(start, columns, &identity, alternative, measure);
    let decided = bands.is_some();
    let bands = bands
        .unwrap_or_default()
        .into_iter()
        .map(|band| FrontierBand {
            long_form: band.long_form.iter().map(|&i| names[i].clone()).collect(),
            winner: band_winner(band.diff),
            min_saving: band.diff.min,
            max_saving: band.diff.max,
        })
        .collect();
    let without_nulls = if columns.iter().any(Column::null_varies) {
        let overall = crate::dominance::compare(start, columns, &identity, alternative, Nulls::Vary, measure);
        let stored = crate::dominance::compare(start, columns, &identity, alternative, Nulls::Stored, measure);
        match (overall, stored) {
            (Some(overall), Some(stored)) if overall != stored => Some(NullFreeComparison {
                winner: band_winner(stored),
                min_saving: stored.min,
                max_saving: stored.max,
            }),
            _ => None,
        }
    } else {
        None
    };
    Frontier {
        order: alternative.iter().map(|&i| names[i].clone()).collect(),
        measure,
        current_worst: worst(&identity),
        alternative_worst: worst(alternative),
        current_deterministic: current_walk.padding,
        alternative_deterministic: alt_walk.padding,
        decided,
        bands,
        without_nulls,
        query: None,
    }
}

pub(crate) fn build(table: FoldedTable) -> TableReport {
    // The signature names the declared layout; the engine sees what rows can actually hold.
    let kinds: Vec<ColumnKind> = table.columns.iter().map(stored_kind).collect();
    let columns: Vec<Column> = table
        .columns
        .iter()
        .zip(&kinds)
        .map(|(c, &kind)| Column {
            kind,
            nullable: !c.not_null,
        })
        .collect();
    let null_varies = columns.iter().any(Column::null_varies);
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
    // starts data at MAXALIGN(23) = 24. A row holding a NULL always carries the bitmap.
    let null_hoff = layout::null_thoff(kinds.len() + table.dropped_count);
    let t_hoff = if table.dropped_count > 0 {
        null_hoff
    } else {
        layout::bare_thoff()
    };
    let current_walk = layout::walk(&kinds);
    let search = layout::search(&kinds);
    let identity: Vec<usize> = (0..kinds.len()).collect();
    let current = stats(tier, &columns, &identity, &current_walk, t_hoff, null_hoff);
    let decision = match tier {
        // Exact tier without NULLs: one realization, so the certainty pole is the exact padding
        // minimum when the search completed, and the footprint delta is the whole story
        // (computed below). A capped search offers its heuristic and claims no exhaustiveness.
        Tier::Exact if !null_varies => Decision {
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
        // Exact per NULL pattern: decided, searched, and reported in row size, which varies by
        // pattern.
        Tier::Exact => decide(Start::TABLE, &columns, &search, &current_walk, Measure::RowSize),
        Tier::Estimate => decide(Start::TABLE, &columns, &search, &current_walk, Measure::Padding),
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
    let suggested_walk = layout::walk(&kinds_in(&columns, &decision.order));
    let suggested = stats(tier, &columns, &decision.order, &suggested_walk, t_hoff, null_hoff);
    // Exact-tier avoidable without NULLs is the footprint delta; a reorder that does not cross an
    // 8-byte rung reports zero even when raw padding drops.
    let (avoidable_deterministic, avoidable_dominance) = match tier {
        Tier::Exact if !null_varies => (
            current
                .footprint
                .unwrap_or(0)
                .saturating_sub(suggested.footprint.unwrap_or(0)),
            0,
        ),
        Tier::Exact | Tier::Estimate | Tier::Unknown => {
            (decision.avoidable_deterministic, decision.avoidable_dominance)
        }
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
    let measure = if tier == Tier::Exact {
        Measure::RowSize
    } else {
        Measure::Padding
    };
    let frontier = decision.frontier_order.as_ref().map(|alternative| {
        let names: Vec<String> = table.columns.iter().map(|c| c.display.clone()).collect();
        let mut frontier = frontier_report(Start::TABLE, &columns, &names, &current_walk, alternative, measure);
        let query_columns: Vec<crate::resolve::QueryColumn> = table
            .columns
            .iter()
            .zip(&kinds)
            .map(|(c, &kind)| crate::resolve::QueryColumn::new(&c.key, kind, &c.type_display))
            .collect();
        let written: Vec<usize> = (0..kinds.len()).collect();
        frontier.query = Some(crate::resolve::query(
            &table.relation,
            &query_columns,
            &written,
            alternative,
            measure,
        ));
        frontier
    });
    let suggested_order = final_order.iter().map(|&i| table.columns[i].display.clone()).collect();
    let null_variables = table
        .columns
        .iter()
        .zip(&columns)
        .filter(|(_, c)| c.null_varies())
        .map(|(f, _)| f.display.clone())
        .collect();
    let column_reports = table
        .columns
        .iter()
        .zip(&current_walk.columns)
        .map(|(c, w)| ColumnReport {
            name: c.display.clone(),
            key: c.key.clone(),
            type_display: c.type_display.clone(),
            not_null: c.not_null,
            known_type: c.known_type,
            kind: stored_kind(c),
            storage: c.storage,
            pad_before: w.pad_before.exact(),
            offset: w.offset,
            added_in: c.origin.clone(),
            attnum: c.attnum,
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
    let mut slots: Vec<Option<ColumnKind>> = vec![None; kinds.len() + table.dropped_count];
    for column in &table.columns {
        if let Some(slot) = column.attnum.checked_sub(1).and_then(|i| slots.get_mut(i)) {
            *slot = Some(column.kind);
        }
    }
    let layout_signature = layout_signature(&slots);
    TableReport {
        name: table.key,
        display: table.display,
        origin: table.origin,
        altered_in: table.altered_in,
        ignored: table.ignored,
        incomplete: table.incomplete,
        tier,
        relation: table.relation.clone(),
        natts: kinds.len(),
        any_nullable,
        null_variables,
        columns: column_reports,
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

/// The column's storage class as its rows can hold it. PLAIN does not make the 1-byte header on
/// the way in (`VARLENA_ATT_IS_PACKABLE`), so a value a COPY or an UPDATE writes keeps the aligned
/// 4-byte header at any length: a typmod no longer proves the column short. Only a column whose
/// every row was written under PLAIN is sure to hold no TOAST pointer.
fn stored_kind(column: &crate::fold::FoldedColumn) -> ColumnKind {
    match column.kind {
        ColumnKind::Varlena { align, payload, .. } if column.plain_rows => ColumnKind::Varlena {
            align,
            proven_short: false,
            payload: layout::Payload {
                toastable: column.toasted_rows,
                ..payload
            },
        },
        kind => kind,
    }
}

/// Canonical signature of the physical attribute slots: `f{len}{align}` per fixed column,
/// `v{align}` per varlena (`p` appended when typmod-proven short), `-` per dropped slot,
/// comma-joined, e.g. `f8d,f4i,-,vi,vip`. Stored verbatim in baseline entries: self-describing in
/// diffs, and free of hash-stability concerns across releases. `ADD COLUMN` appends a slot and
/// `DROP COLUMN` turns one into `-` without moving any other, which is what lets the baseline
/// gate tell the committed slots from the appended ones.
pub fn layout_signature(slots: &[Option<ColumnKind>]) -> String {
    let parts: Vec<String> = slots
        .iter()
        .map(|slot| match slot {
            Some(ColumnKind::Fixed { len, align }) => format!("f{len}{}", align_letter(*align)),
            Some(ColumnKind::Varlena {
                align, proven_short, ..
            }) => {
                let p = if *proven_short { "p" } else { "" };
                format!("v{}{p}", align_letter(*align))
            }
            None => "-".to_string(),
        })
        .collect();
    parts.join(",")
}

/// The appended block of a table whose leading slots are committed: applied in production,
/// where only a rewrite could reorder them. The block's order is still free while the migration
/// that appends it is unapplied, so it is judged by dominance among orders of the block, starting
/// from the offset residues the committed columns can end at; those are never reordered.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BlockFinding {
    /// The statements that appended the block's columns, in order, each once.
    pub origins: Vec<Origin>,
    /// Committed attribute slots ahead of the block, dropped ones included.
    pub committed_slots: usize,
    /// Live committed columns ahead of the block.
    pub prefix_columns: usize,
    /// The block's columns in written order (display names).
    pub columns: Vec<String>,
    /// The block order to write instead; the written order when nothing dominates it.
    pub suggested_order: Vec<String>,
    /// What the suggested block order saves: deterministic plus dominance-proven, as for a table
    /// (row-size bytes at the exact tier, padding bytes at the estimate tier).
    pub avoidable_bytes_per_row: f64,
    /// Exactly-known waste the block reorder removes, bytes/row.
    pub avoidable_deterministic: u64,
    /// Further worst-case waste the dominating block order removes, bytes/row.
    pub avoidable_dominance: u64,
    /// Guaranteed-to-maximum saving of the suggested block order over every realization.
    pub dominance_saving: Option<SavingRange>,
    /// How far the search over block orders went.
    pub dominance_search: DominanceScope,
    /// How much of the block's order space the pole search proved.
    pub search_scope: layout::SearchScope,
    /// A workload-dependent alternative block order, reported and never gated.
    pub frontier: Option<Frontier>,
}

/// Judge the columns of `table` past its first `committed_slots` attribute slots as an appended
/// block, or None when there is no block (nothing was appended) or the table could not be
/// modeled.
pub fn block_finding(table: &TableReport, committed_slots: usize) -> Option<BlockFinding> {
    let prefix_columns = table.columns.iter().take_while(|c| c.attnum <= committed_slots).count();
    if table.incomplete || prefix_columns >= table.columns.len() {
        return None;
    }
    let columns: Vec<Column> = table
        .columns
        .iter()
        .map(|c| Column {
            kind: c.kind,
            nullable: !c.not_null,
        })
        .collect();
    let (prefix, block) = columns.split_at(prefix_columns);
    let names: Vec<String> = table.columns[prefix_columns..].iter().map(|c| c.name.clone()).collect();
    let block_kinds: Vec<ColumnKind> = block.iter().map(|c| c.kind).collect();
    let start = Start::after(prefix);
    let walk = layout::walk_from(start, &block_kinds);
    let search = layout::search_from(start, &block_kinds);
    let measure = if table.tier == Tier::Exact {
        Measure::RowSize
    } else {
        Measure::Padding
    };
    let decision = decide(start, block, &search, &walk, measure);
    let avoidable = decision.avoidable_deterministic + decision.avoidable_dominance;
    let order = if avoidable == 0 {
        (0..block.len()).collect()
    } else {
        decision.order
    };
    let frontier = decision.frontier_order.as_ref().map(|alternative| {
        let mut frontier = frontier_report(start, block, &names, &walk, alternative, measure);
        let query_columns: Vec<crate::resolve::QueryColumn> = table
            .columns
            .iter()
            .map(|c| crate::resolve::QueryColumn::new(&c.key, c.kind, &c.type_display))
            .collect();
        let current: Vec<usize> = (0..table.columns.len()).collect();
        let whole: Vec<usize> = (0..prefix_columns)
            .chain(alternative.iter().map(|&i| prefix_columns + i))
            .collect();
        frontier.query = Some(crate::resolve::query(
            &table.relation,
            &query_columns,
            &current,
            &whole,
            measure,
        ));
        frontier
    });
    let mut origins: Vec<Origin> = Vec::new();
    for column in &table.columns[prefix_columns..] {
        if !origins.contains(&column.added_in) {
            origins.push(column.added_in.clone());
        }
    }
    Some(BlockFinding {
        origins,
        committed_slots,
        prefix_columns,
        suggested_order: order.iter().map(|&i| names[i].clone()).collect(),
        columns: names,
        avoidable_bytes_per_row: avoidable as f64,
        avoidable_deterministic: decision.avoidable_deterministic,
        avoidable_dominance: decision.avoidable_dominance,
        dominance_saving: (avoidable > 0).then_some(decision.dominance_saving).flatten(),
        dominance_search: match decision.dominance_search {
            DominanceScope::Exhaustive
                if block
                    .iter()
                    .any(|c| matches!(c.kind, ColumnKind::Varlena { payload, .. } if !payload.verified)) =>
            {
                DominanceScope::Superset
            }
            scope => scope,
        },
        search_scope: search.scope,
        frontier,
    })
}

fn align_letter(align: layout::Align) -> char {
    match align {
        layout::Align::Char => 'c',
        layout::Align::Short => 's',
        layout::Align::Int => 'i',
        layout::Align::Double => 'd',
    }
}

fn stats(tier: Tier, columns: &[Column], order: &[usize], walk: &Walk, t_hoff: u64, null_hoff: u64) -> OrderStats {
    // The end is known exactly iff the table has no varlena, which is exactly the exact tier;
    // the estimate tier has no footprint to claim, and the unknown tier claims nothing.
    let footprint = match (tier, walk.end) {
        (Tier::Exact, Some(end)) => Some(layout::footprint_at(t_hoff, end)),
        _ => None,
    };
    let with_nulls = match tier {
        Tier::Exact => {
            let ordered: Vec<Column> = order.iter().map(|&i| columns[i]).collect();
            layout::null_row_ends(&ordered).map(|(lo, hi)| NullRows {
                t_hoff: null_hoff,
                footprint_min: layout::footprint_at(null_hoff, lo),
                footprint_max: layout::footprint_at(null_hoff, hi),
            })
        }
        Tier::Estimate | Tier::Unknown => None,
    };
    // Bounds over the realization model, which knows the payload residues a type stores and
    // which columns may hold NULL.
    let bounds = crate::dominance::summary(Start::TABLE, columns, order, Nulls::Vary, Measure::Padding);
    let stored = crate::dominance::summary(Start::TABLE, columns, order, Nulls::Stored, Measure::Padding);
    let without_nulls = ((stored.min, stored.max) != (bounds.min, bounds.max)).then_some(PaddingBounds {
        min: stored.min,
        max: stored.max,
    });
    OrderStats {
        padding: walk.padding,
        expected_padding: walk.expected_padding(),
        padding_min: bounds.min,
        padding_max: bounds.max,
        without_nulls,
        footprint,
        rows_per_page: footprint.map(layout::rows_per_page),
        with_nulls,
    }
}
