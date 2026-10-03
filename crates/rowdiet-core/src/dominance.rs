//! Realization-independent comparison of two column orders over the same columns.
//!
//! A realization assigns every varlena column a storage form and a payload length, and every
//! nullable column a NULL or a value. Padding depends on the realization only through each
//! varlena's form and its payload length mod MAXALIGN, and through which nullable columns are
//! stored: the short form (payloads of 126 bytes or less) advances by 1 + payload with no
//! alignment, a TOAST pointer advances by a fixed 18 bytes (the same shape as a short payload
//! of 17), the in-line long form aligns to typalign and advances by 4 + payload, and a NULL
//! stores nothing. A short value is stored uncompressed, so its payload residues are the type's
//! own ([`Payload`](crate::layout::Payload): any for text, even for numeric, `4 + k * stride` for
//! arrays); a long value may be compressed and reaches every residue. A NULL varlena advances by
//! 0, the step of a short payload of residue 7, which is a step of its own wherever the type's
//! short payloads never take that residue (numeric, arrays of wider elements). Order A
//! **dominates** order B when A's total padding is less than or equal to B's in every
//! realization, and strictly less in at least one: a claim that needs no payload or NULL
//! knowledge, which is why the gate and the recommender may act on it (see docs/design.md).
//! [`Measure::RowSize`] makes the same claim about the MAXALIGN-rounded row size.
//!
//! Two exact engines compute the bounds of `pad(A) − pad(B)` over all realizations:
//!
//! - When both orders keep the varlenas in the same relative sequence, a joint walk over the
//!   pair of offset residues (64 states) visits each varlena once; per-column realizations are
//!   independent, so extremes compose state by state. A nullable fixed column the two orders
//!   place at different points carries its NULL bit in the state from the first order's
//!   placement to the second's, which keeps the walk exact; its cost doubles per bit in flight,
//!   and past [`IN_FLIGHT_LIMIT`] bits the pair goes to the enumeration instead.
//! - Otherwise, exhaustive enumeration of per-varlena (form, payload residue) assignments and
//!   per-column NULLs, within a state budget. Past the budget the comparison is reported as
//!   undecided rather than guessed.
//!
//! [`summary`] bounds one order alone, which decides dominance for free when one side pads
//! zero in every realization: nothing pads less, so such an order dominates every order that
//! pads somewhere, by exactly that order's padding.
//!
//! Every entry point takes a [`Start`]: the orders compared may be the appended block of a table
//! whose leading columns are committed. Both orders then start from the same offset residue, one
//! of those the prefix can end at, and the prefix's own padding is the same in both.

use crate::layout::{Column, ColumnKind, MAXALIGN, NULL_LIKE_RESIDUE, Start, pad};

/// Bounds of `pad(a) − pad(b)` in bytes over every realization; both ends are attained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffBounds {
    /// Smallest attainable difference.
    pub min: i64,
    /// Largest attainable difference.
    pub max: i64,
}

impl DiffBounds {
    /// `b` is never worse than `a` and strictly better in at least one realization.
    pub fn b_dominates(self) -> bool {
        self.min >= 0 && self.max > 0
    }

    /// `a` is never worse than `b` and strictly better in at least one realization.
    pub fn a_dominates(self) -> bool {
        self.max <= 0 && self.min < 0
    }

    /// Identical padding in every realization.
    pub fn equal(self) -> bool {
        self.min == 0 && self.max == 0
    }

    fn shifted(self, delta: i64) -> Self {
        Self {
            min: self.min + delta,
            max: self.max + delta,
        }
    }

    fn merge(&mut self, other: Self) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }
}

fn merge_into(slot: &mut Option<DiffBounds>, bounds: DiffBounds) {
    match slot {
        Some(existing) => existing.merge(bounds),
        None => *slot = Some(bounds),
    }
}

/// One storage-form band of a frontier: the named varlenas store the in-line long form, every
/// other varlena stays short, TOAST, or NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Column indices (into `columns`) stored long-form in this band.
    pub long_form: Vec<usize>,
    /// `pad(a) − pad(b)` bounds within the band, over every payload and NULL pattern.
    pub diff: DiffBounds,
}

/// Which NULL patterns a comparison ranges over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nulls {
    /// Every nullable column holds NULL or a value, independently per row.
    Vary,
    /// Every column is stored: the rows without NULLs.
    Stored,
}

/// What a comparison totals per realization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum Measure {
    /// Inter-column padding.
    Padding,
    /// Row size: padding plus the trailing MAXALIGN rounding. Both orders of one realization store
    /// the same header and data bytes, so a difference in this measure is the on-disk row size
    /// difference. Summaries in this measure count the bytes a row spends over the same row
    /// with no padding at all.
    RowSize,
}

/// Enumeration budget: the product of per-variable realization counts (up to 16 per varlena,
/// fewer for narrowed payloads or a pinned form, and 2 per nullable fixed column) must stay under
/// this for the exhaustive engine to run.
const ENUMERATION_BUDGET: u64 = 1 << 21;

/// Bands are enumerated per form combination; more long-capable varlenas than this and the
/// frontier is reported without band detail.
const BAND_LIMIT: usize = 6;

/// NULL bits the joint walk carries at once (64 × 2^bits residue states); past it the pair is
/// enumerated when that fits the budget, and undecided otherwise.
pub const IN_FLIGHT_LIMIT: usize = 12;

/// Estimated work for [`compare`] in realization-walk units: near-free for a pair sharing its
/// varlena sequence and its NULL placement, doubling per NULL bit the joint walk carries, the
/// enumeration state count for a pair that reorders varlenas, and `u64::MAX` when [`compare`]
/// would return None. Lets a caller ration a comparison budget before spending it.
pub fn comparison_cost(
    start: Start,
    columns: &[Column],
    a: &[usize],
    b: &[usize],
    nulls: Nulls,
    measure: Measure,
) -> u64 {
    Pair::new(start, columns, a, b, nulls, measure).cost(None)
}

/// Bounds of `measure(a) − measure(b)` over every realization the NULL scope allows, or None when
/// the pair is out of budget (varlena sequences differ or too many NULL bits are in flight, and
/// the enumeration space is too large).
pub fn compare(
    start: Start,
    columns: &[Column],
    a: &[usize],
    b: &[usize],
    nulls: Nulls,
    measure: Measure,
) -> Option<DiffBounds> {
    Pair::new(start, columns, a, b, nulls, measure).run(None)
}

/// The frontier bands for an incomparable pair: one entry per storage-form combination, in
/// ascending order of long-form column count, each over every payload and NULL pattern. None
/// past the band limit or budget.
pub fn bands(start: Start, columns: &[Column], a: &[usize], b: &[usize], measure: Measure) -> Option<Vec<Band>> {
    let long_capable: Vec<usize> = varlena_sequence(columns, a)
        .into_iter()
        .filter(|&c| long_capable(columns[c].kind))
        .collect();
    if long_capable.len() > BAND_LIMIT {
        return None;
    }
    let pair = Pair::new(start, columns, a, b, Nulls::Vary, measure);
    let mut out = Vec::with_capacity(1 << long_capable.len());
    for combo in 0u32..(1 << long_capable.len()) {
        let long_form: Vec<usize> = long_capable
            .iter()
            .enumerate()
            .filter(|(bit, _)| combo & (1 << bit) != 0)
            .map(|(_, &c)| c)
            .collect();
        let pin = FormPin { long: &long_form };
        let diff = pair.run(Some(&pin))?;
        out.push(Band { long_form, diff });
    }
    out.sort_by_key(|band| band.long_form.len());
    Some(out)
}

/// True when the column can store the aligned long form at all: every varlena but one proven
/// too short to compress.
fn long_capable(kind: ColumnKind) -> bool {
    !kind.is_fixed() && !kind.always_short()
}

/// The payload residue a TOAST pointer advances like: 18 bytes is a short payload of 17.
const TOAST_RESIDUE: u8 = 1 << 1;

/// A varlena's realizations as payload-residue bitmasks per form: `short` stores unaligned (a
/// 1-byte header, a TOAST pointer, or a NULL), `long` aligns to typalign behind a 4-byte header.
#[derive(Debug, Clone, Copy)]
struct Domain {
    short: u8,
    long: u8,
}

impl Domain {
    fn of(column: Column, nulls: Nulls) -> Self {
        // A NULL advances 0 bytes, which is what a short payload of residue 7 advances.
        let null = if column.nullable && nulls == Nulls::Vary {
            NULL_LIKE_RESIDUE
        } else {
            0
        };
        match column.kind {
            kind @ ColumnKind::Varlena { payload, .. } if kind.always_short() => Self {
                short: payload.residues() | null,
                long: 0,
            },
            ColumnKind::Varlena { payload, .. } => Self {
                short: payload.residues() | if payload.toastable { TOAST_RESIDUE } else { 0 } | null,
                long: 0xFF,
            },
            ColumnKind::Fixed { .. } => Self { short: 0, long: 0 },
        }
    }

    fn allows(self, form_long: bool, payload: u64) -> bool {
        let mask = if form_long { self.long } else { self.short };
        mask & (1 << payload) != 0
    }

    fn size(self) -> u64 {
        u64::from(self.short.count_ones() + self.long.count_ones())
    }
}

/// Storage forms pinned per column for band computation: listed columns are long, all other
/// long-capable columns short, TOAST, or NULL.
struct FormPin<'a> {
    long: &'a [usize],
}

impl FormPin<'_> {
    /// The realization domain for `column` with its form pinned.
    fn domain(&self, index: usize, column: Column, nulls: Nulls) -> Domain {
        let full = Domain::of(column, nulls);
        if long_capable(column.kind) && self.long.contains(&index) {
            Domain { short: 0, ..full }
        } else {
            Domain { long: 0, ..full }
        }
    }
}

/// Varlena column indices in walk order.
fn varlena_sequence(columns: &[Column], order: &[usize]) -> Vec<usize> {
    order
        .iter()
        .copied()
        .filter(|&i| matches!(columns[i].kind, ColumnKind::Varlena { .. }))
        .collect()
}

/// One varlena placement from a residue: (pad, residue after the value).
fn varlena_step(residue: u64, align: u64, long: bool, payload: u64) -> (u64, u64) {
    if long {
        let p = pad(residue, align);
        (p, (residue + p + 4 + payload) % MAXALIGN)
    } else {
        (0, (residue + 1 + payload) % MAXALIGN)
    }
}

/// A fixed run's effect from each start residue: (padding total, residue after).
type RunTable = [(u64, u64); MAXALIGN as usize];

fn run_table(segment: &[(u64, u64)]) -> RunTable {
    std::array::from_fn(|start| {
        let mut residue = start as u64;
        let mut total = 0;
        for &(align, len_mod) in segment {
            let p = pad(residue, align);
            total += p;
            residue = (residue + p + len_mod) % MAXALIGN;
        }
        (total, residue)
    })
}

/// The trailing MAXALIGN rounding a realization ending at `residue` adds to the row.
fn tail(measure: Measure, residue: u64) -> u64 {
    match measure {
        Measure::Padding => 0,
        Measure::RowSize => pad(residue, MAXALIGN),
    }
}

/// One comparison: the two orders, where they start, which NULL patterns count, and what is
/// totaled.
struct Pair<'a> {
    /// Offset residues both orders can start at (bitmask over 0..=7).
    start: u8,
    columns: &'a [Column],
    a: &'a [usize],
    b: &'a [usize],
    nulls: Nulls,
    measure: Measure,
}

impl<'a> Pair<'a> {
    fn new(
        start: Start,
        columns: &'a [Column],
        a: &'a [usize],
        b: &'a [usize],
        nulls: Nulls,
        measure: Measure,
    ) -> Self {
        debug_assert_eq!(a.len(), columns.len());
        debug_assert_eq!(b.len(), columns.len());
        Self {
            start: start.residues(nulls == Nulls::Vary),
            columns,
            a,
            b,
            nulls,
            measure,
        }
    }

    /// A NULL in this fixed column is a realization of its own in this comparison.
    fn varies(&self, column: usize) -> bool {
        self.nulls == Nulls::Vary && self.columns[column].nullable && self.columns[column].kind.is_fixed()
    }

    fn domain(&self, pin: Option<&FormPin<'_>>, column: usize) -> Domain {
        match pin {
            Some(p) => p.domain(column, self.columns[column], self.nulls),
            None => Domain::of(self.columns[column], self.nulls),
        }
    }

    fn run(&self, pin: Option<&FormPin<'_>>) -> Option<DiffBounds> {
        let va = varlena_sequence(self.columns, self.a);
        if va == varlena_sequence(self.columns, self.b) {
            let schedule = Schedule::build(self, &va);
            if schedule.slots <= IN_FLIGHT_LIMIT {
                return Some(self.joint_walk(&schedule, pin));
            }
        }
        if self.enumeration_states(&va, pin) <= ENUMERATION_BUDGET {
            return Some(self.enumerate(&va, pin));
        }
        None
    }

    fn cost(&self, pin: Option<&FormPin<'_>>) -> u64 {
        let va = varlena_sequence(self.columns, self.a);
        if va == varlena_sequence(self.columns, self.b) {
            let schedule = Schedule::build(self, &va);
            if schedule.slots <= IN_FLIGHT_LIMIT {
                return ((schedule.moves.len() as u64).max(1) * 64 * u64::from(self.start.count_ones()))
                    << schedule.slots;
            }
        }
        let states = self.enumeration_states(&va, pin);
        if states <= ENUMERATION_BUDGET { states } else { u64::MAX }
    }

    fn enumeration_states(&self, varlenas: &[usize], pin: Option<&FormPin<'_>>) -> u64 {
        let null_bits = (0..self.columns.len()).filter(|&c| self.varies(c)).count();
        if null_bits >= 60 {
            return u64::MAX;
        }
        varlenas
            .iter()
            .map(|&c| self.domain(pin, c).size())
            .try_fold(u64::from(self.start.count_ones()) << null_bits, u64::checked_mul)
            .unwrap_or(u64::MAX)
    }

    /// Exact bounds when both orders share the varlena sequence: a DP over the pair of offset
    /// residues plus the NULL bits in flight. Padding depends on offsets only mod MAXALIGN,
    /// per-column realizations are independent, and a state holds everything the rest of
    /// either walk depends on, so merging extremes per state loses nothing.
    fn joint_walk(&self, schedule: &Schedule, pin: Option<&FormPin<'_>>) -> DiffBounds {
        let size = 64usize << schedule.slots;
        let mut states: Vec<Option<DiffBounds>> = vec![None; size];
        let mut next: Vec<Option<DiffBounds>> = vec![None; size];
        let index = |bits: u64, ra: u64, rb: u64| ((bits << 6) | (ra << 3) | rb) as usize;
        for residue in (0..MAXALIGN).filter(|r| self.start & (1 << r) != 0) {
            states[index(0, residue, residue)] = Some(DiffBounds { min: 0, max: 0 });
        }
        for step in &schedule.moves {
            next.iter_mut().for_each(|slot| *slot = None);
            let live = states
                .iter()
                .enumerate()
                .filter_map(|(state, bounds)| bounds.map(|b| (state as u64, b)));
            match *step {
                Move::Varlena { column, align } => {
                    let domain = self.domain(pin, column);
                    for (state, bounds) in live {
                        let (bits, ra, rb) = (state >> 6, (state >> 3) & 7, state & 7);
                        for payload in 0..MAXALIGN {
                            for form_long in [false, true] {
                                if !domain.allows(form_long, payload) {
                                    continue;
                                }
                                let (pad_a, ra2) = varlena_step(ra, align, form_long, payload);
                                let (pad_b, rb2) = varlena_step(rb, align, form_long, payload);
                                merge_into(
                                    &mut next[index(bits, ra2, rb2)],
                                    bounds.shifted(pad_a as i64 - pad_b as i64),
                                );
                            }
                        }
                    }
                }
                Move::Run { a_side, ref table } => {
                    for (state, bounds) in live {
                        let (bits, ra, rb) = (state >> 6, (state >> 3) & 7, state & 7);
                        if a_side {
                            let (p, ra2) = table[ra as usize];
                            merge_into(&mut next[index(bits, ra2, rb)], bounds.shifted(p as i64));
                        } else {
                            let (p, rb2) = table[rb as usize];
                            merge_into(&mut next[index(bits, ra, rb2)], bounds.shifted(-(p as i64)));
                        }
                    }
                }
                Move::Nullable {
                    a_side,
                    len,
                    align,
                    presence,
                } => {
                    for (state, bounds) in live {
                        let (bits, ra, rb) = (state >> 6, (state >> 3) & 7, state & 7);
                        let bit = match presence {
                            Presence::Bind(slot) | Presence::Consume(slot) => 1u64 << slot,
                        };
                        let rest = bits & !bit;
                        let (store, skip) = match presence {
                            // Branch on the NULL and remember the choice in the slot's bit.
                            Presence::Bind(_) => (Some(bits | bit), Some(bits)),
                            // Replay the choice the first placement made, and free the slot.
                            Presence::Consume(_) if bits & bit != 0 => (Some(rest), None),
                            Presence::Consume(_) => (None, Some(rest)),
                        };
                        if let Some(bits) = store {
                            if a_side {
                                let p = pad(ra, align);
                                let ra2 = (ra + p + len) % MAXALIGN;
                                merge_into(&mut next[index(bits, ra2, rb)], bounds.shifted(p as i64));
                            } else {
                                let p = pad(rb, align);
                                let rb2 = (rb + p + len) % MAXALIGN;
                                merge_into(&mut next[index(bits, ra, rb2)], bounds.shifted(-(p as i64)));
                            }
                        }
                        if let Some(bits) = skip {
                            merge_into(&mut next[index(bits, ra, rb)], bounds);
                        }
                    }
                }
            }
            std::mem::swap(&mut states, &mut next);
        }
        let mut out: Option<DiffBounds> = None;
        for (state, bounds) in states.iter().enumerate() {
            let Some(bounds) = *bounds else { continue };
            debug_assert_eq!(state >> 6, 0, "every NULL bit is consumed by the end");
            let (ra, rb) = (((state >> 3) & 7) as u64, (state & 7) as u64);
            let delta = tail(self.measure, ra) as i64 - tail(self.measure, rb) as i64;
            merge_into(&mut out, bounds.shifted(delta));
        }
        out.expect("at least one realization exists")
    }

    /// Exhaustive engine for pairs whose varlena sequences differ: enumerate every per-varlena
    /// (form, payload residue) assignment and every NULL pattern, and walk both orders concretely.
    fn enumerate(&self, varlenas: &[usize], pin: Option<&FormPin<'_>>) -> DiffBounds {
        let nullable: Vec<usize> = (0..self.columns.len()).filter(|&c| self.varies(c)).collect();
        let mut slot_of = vec![usize::MAX; self.columns.len()];
        for (slot, &c) in varlenas.iter().chain(&nullable).enumerate() {
            slot_of[c] = slot;
        }
        let plans = [
            WalkPlan::new(self, self.a, &slot_of),
            WalkPlan::new(self, self.b, &slot_of),
        ];
        let domains: Vec<Domain> = varlenas.iter().map(|&c| self.domain(pin, c)).collect();
        let mut assignment: Vec<Realization> = vec![Realization::Stored; varlenas.len() + nullable.len()];
        let mut out: Option<DiffBounds> = None;
        self.enumerate_rec(&plans, &domains, &mut assignment, 0, &mut out);
        out.expect("at least one realization exists")
    }

    fn enumerate_rec(
        &self,
        plans: &[WalkPlan; 2],
        domains: &[Domain],
        assignment: &mut [Realization],
        depth: usize,
        out: &mut Option<DiffBounds>,
    ) {
        if depth == assignment.len() {
            for start in (0..MAXALIGN).filter(|r| self.start & (1 << r) != 0) {
                let (pad_a, ra) = plans[0].pad(assignment, start);
                let (pad_b, rb) = plans[1].pad(assignment, start);
                let d = (pad_a + tail(self.measure, ra)) as i64 - (pad_b + tail(self.measure, rb)) as i64;
                merge_into(out, DiffBounds { min: d, max: d });
            }
            return;
        }
        let Some(&domain) = domains.get(depth) else {
            for realization in [Realization::Stored, Realization::Null] {
                assignment[depth] = realization;
                self.enumerate_rec(plans, domains, assignment, depth + 1, out);
            }
            return;
        };
        for payload in 0..MAXALIGN {
            for form_long in [false, true] {
                if !domain.allows(form_long, payload) {
                    continue;
                }
                assignment[depth] = Realization::Varlena { form_long, payload };
                self.enumerate_rec(plans, domains, assignment, depth + 1, out);
            }
        }
    }
}

/// One variable's value in a full realization under enumeration.
#[derive(Debug, Clone, Copy)]
enum Realization {
    /// A varlena's storage form and payload residue (a NULL varlena is the short payload 7).
    Varlena { form_long: bool, payload: u64 },
    /// A nullable fixed column holding a value.
    Stored,
    /// A nullable fixed column holding NULL.
    Null,
}

/// One order reduced to its variables (as slots into the assignment) and the fixed runs between.
struct WalkPlan {
    steps: Vec<PlanStep>,
}

enum PlanStep {
    Run(RunTable),
    Varlena { slot: usize, align: u64 },
    Nullable { slot: usize, len: u64, align: u64 },
}

impl WalkPlan {
    fn new(pair: &Pair<'_>, order: &[usize], slot_of: &[usize]) -> Self {
        let mut steps = Vec::new();
        let mut segment: Vec<(u64, u64)> = Vec::new();
        for &column in order {
            let step = match pair.columns[column].kind {
                ColumnKind::Fixed { len, align } if pair.varies(column) => PlanStep::Nullable {
                    slot: slot_of[column],
                    len,
                    align: align.bytes(),
                },
                ColumnKind::Fixed { len, align } => {
                    segment.push((align.bytes(), len % MAXALIGN));
                    continue;
                }
                ColumnKind::Varlena { align, .. } => PlanStep::Varlena {
                    slot: slot_of[column],
                    align: align.bytes(),
                },
            };
            if !segment.is_empty() {
                steps.push(PlanStep::Run(run_table(&segment)));
                segment.clear();
            }
            steps.push(step);
        }
        if !segment.is_empty() {
            steps.push(PlanStep::Run(run_table(&segment)));
        }
        Self { steps }
    }

    /// Padding of the order under one full realization from a start residue, residues only:
    /// (padding, end residue).
    fn pad(&self, assignment: &[Realization], start: u64) -> (u64, u64) {
        let mut total = 0;
        let mut residue = start;
        for step in &self.steps {
            match *step {
                PlanStep::Run(ref table) => {
                    let (p, next) = table[residue as usize];
                    total += p;
                    residue = next;
                }
                PlanStep::Varlena { slot, align } => {
                    let Realization::Varlena { form_long, payload } = assignment[slot] else {
                        unreachable!("varlena slots hold varlena realizations")
                    };
                    let (p, next) = varlena_step(residue, align, form_long, payload);
                    total += p;
                    residue = next;
                }
                PlanStep::Nullable { slot, len, align } => {
                    if matches!(assignment[slot], Realization::Stored) {
                        let p = pad(residue, align);
                        total += p;
                        residue = (residue + p + len) % MAXALIGN;
                    }
                }
            }
        }
        (total, residue)
    }
}

/// How a nullable fixed column's move treats its NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// First of the two orders to place it: branch on its NULL and keep the bit in `slot`.
    Bind(u32),
    /// Second order to place it: read the bit from `slot` and free it.
    Consume(u32),
}

enum Move {
    /// A varlena both orders place at the same point of their varlena sequence.
    Varlena { column: usize, align: u64 },
    /// A run of fixed columns stored in every row, placed by one order; `a_side` says which.
    Run { a_side: bool, table: RunTable },
    /// A nullable fixed column one order places.
    Nullable {
        a_side: bool,
        len: u64,
        align: u64,
        presence: Presence,
    },
}

/// The joint walk's move list: both orders advance between shared varlenas, and a nullable
/// column's NULL bit stays in flight from its first placement to its second.
struct Schedule {
    moves: Vec<Move>,
    /// Bit slots the walk needs: the most NULL bits in flight at once.
    slots: usize,
}

impl Schedule {
    fn build(pair: &Pair<'_>, varlenas: &[usize]) -> Self {
        let segments_a = fixed_segments(pair.columns, pair.a);
        let segments_b = fixed_segments(pair.columns, pair.b);
        debug_assert_eq!(segments_a.len(), varlenas.len() + 1);
        debug_assert_eq!(segments_b.len(), varlenas.len() + 1);
        let mut moves = Vec::with_capacity(pair.columns.len());
        let mut slot_of: Vec<Option<u32>> = vec![None; pair.columns.len()];
        let mut used: u64 = 0;
        let mut slots = 0usize;
        // Stored columns accumulate into one run per side until a nullable move or a varlena.
        let mut runs: [Vec<(u64, u64)>; 2] = [Vec::new(), Vec::new()];
        let flush = |moves: &mut Vec<Move>, runs: &mut [Vec<(u64, u64)>; 2]| {
            for (side, run) in runs.iter_mut().enumerate() {
                if !run.is_empty() {
                    moves.push(Move::Run {
                        a_side: side == 0,
                        table: run_table(run),
                    });
                    run.clear();
                }
            }
        };
        for (k, (seg_a, seg_b)) in segments_a.iter().zip(&segments_b).enumerate() {
            if k > 0 {
                let column = varlenas[k - 1];
                moves.push(Move::Varlena {
                    column,
                    align: pair.columns[column].kind.align().bytes(),
                });
            }
            let (mut i, mut j) = (0usize, 0usize);
            while i < seg_a.len() || j < seg_b.len() {
                // Advance a side whose next column opens no new NULL bit; else the first order.
                let opens = |column: usize| pair.varies(column) && slot_of[column].is_none();
                let a_side = if i < seg_a.len() && !opens(seg_a[i]) {
                    true
                } else if j < seg_b.len() && !opens(seg_b[j]) {
                    false
                } else {
                    i < seg_a.len()
                };
                let column = if a_side { seg_a[i] } else { seg_b[j] };
                if a_side {
                    i += 1;
                } else {
                    j += 1;
                }
                let ColumnKind::Fixed { len, align } = pair.columns[column].kind else {
                    unreachable!("fixed segments hold fixed columns")
                };
                if !pair.varies(column) {
                    runs[usize::from(!a_side)].push((align.bytes(), len % MAXALIGN));
                    continue;
                }
                flush(&mut moves, &mut runs);
                let presence = if let Some(slot) = slot_of[column].take() {
                    used &= !(1 << slot);
                    Presence::Consume(slot)
                } else {
                    let slot = (!used).trailing_zeros();
                    used |= 1 << slot;
                    slots = slots.max(slot as usize + 1);
                    slot_of[column] = Some(slot);
                    Presence::Bind(slot)
                };
                moves.push(Move::Nullable {
                    a_side,
                    len: len % MAXALIGN,
                    align: align.bytes(),
                    presence,
                });
                if slots > IN_FLIGHT_LIMIT {
                    return Self { moves, slots };
                }
            }
            flush(&mut moves, &mut runs);
        }
        Self { moves, slots }
    }
}

/// The fixed-column runs between varlenas: `varlena count + 1` segments of column indices, in
/// walk order.
fn fixed_segments(columns: &[Column], order: &[usize]) -> Vec<Vec<usize>> {
    let mut segments: Vec<Vec<usize>> = vec![Vec::new()];
    for &i in order {
        match columns[i].kind {
            ColumnKind::Fixed { .. } => segments.last_mut().expect("segments start non-empty").push(i),
            ColumnKind::Varlena { .. } => segments.push(Vec::new()),
        }
    }
    segments
}

/// One order's cost over the realization model: exact bounds over every realization, and the
/// mean over the sub-distribution where every column is stored and every varlena stores a short
/// uncompressed payload uniform over its type's residues. Dominance implies <= on all three,
/// which makes them sound prunes. In [`Measure::RowSize`] the cost is the bytes a row spends over
/// the same row with no padding at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Smallest attainable cost, bytes.
    pub min: u64,
    /// Largest attainable cost, bytes.
    pub max: u64,
    /// The mean, eighths of a byte (uniform residues over cosets keep it exact).
    pub short_mean_eighths: u64,
}

/// [`Summary`] of `order` from `start` over the realizations the NULL scope allows. Padding walks
/// the eight offset residues for the bounds and one uniform coset of residues for the mean. Row
/// size also tracks the residue the same rows would end at unpadded, and is defined for all-fixed
/// tables only, where the mean is the cost of the row that stores every column.
pub fn summary(start: Start, columns: &[Column], order: &[usize], nulls: Nulls, measure: Measure) -> Summary {
    match measure {
        Measure::Padding => padding_summary(start, columns, order, nulls),
        Measure::RowSize => row_size_summary(start, columns, order, nulls),
    }
}

fn padding_summary(start: Start, columns: &[Column], order: &[usize], nulls: Nulls) -> Summary {
    let mut states: [Option<(u64, u64)>; MAXALIGN as usize] = [None; MAXALIGN as usize];
    let reachable = start.residues(nulls == Nulls::Vary);
    for (residue, state) in states.iter_mut().enumerate() {
        if reachable & (1 << residue) != 0 {
            *state = Some((0, 0));
        }
    }
    let mut set: u8 = start.residues(false);
    let mut mean_eighths = 0u64;
    let merge = |slot: &mut Option<(u64, u64)>, lo: u64, hi: u64| {
        *slot = Some(match *slot {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    };
    for &index in order {
        let column = columns[index];
        let mut next: [Option<(u64, u64)>; MAXALIGN as usize] = [None; MAXALIGN as usize];
        let mut next_set = 0u8;
        match column.kind {
            ColumnKind::Fixed { len, align } => {
                let may_be_null = column.nullable && nulls == Nulls::Vary;
                for (residue, bounds) in states.iter().enumerate() {
                    let Some((lo, hi)) = *bounds else { continue };
                    let p = pad(residue as u64, align.bytes());
                    merge(
                        &mut next[((residue as u64 + p + len) % MAXALIGN) as usize],
                        lo + p,
                        hi + p,
                    );
                    if may_be_null {
                        merge(&mut next[residue], lo, hi);
                    }
                }
                let mut sum = 0u64;
                for residue in (0..MAXALIGN).filter(|r| set & (1 << r) != 0) {
                    let p = pad(residue, align.bytes());
                    sum += p;
                    next_set |= 1 << ((residue + p + len) % MAXALIGN);
                }
                let members = u64::from(set.count_ones());
                debug_assert_eq!(sum * MAXALIGN % members, 0, "a coset keeps the mean in eighths");
                mean_eighths += sum * MAXALIGN / members;
            }
            ColumnKind::Varlena { align, payload, .. } => {
                let domain = Domain::of(column, nulls);
                for (residue, bounds) in states.iter().enumerate() {
                    let Some((lo, hi)) = *bounds else { continue };
                    for p in 0..MAXALIGN {
                        for form_long in [false, true] {
                            if domain.allows(form_long, p) {
                                let (own, after) = varlena_step(residue as u64, align.bytes(), form_long, p);
                                merge(&mut next[after as usize], lo + own, hi + own);
                            }
                        }
                    }
                }
                let uncompressed = payload.residues();
                for residue in (0..MAXALIGN).filter(|r| set & (1 << r) != 0) {
                    for p in (0..MAXALIGN).filter(|p| uncompressed & (1 << p) != 0) {
                        next_set |= 1 << ((residue + 1 + p) % MAXALIGN);
                    }
                }
            }
        }
        states = next;
        set = next_set;
    }
    let reached = states.iter().flatten();
    Summary {
        min: reached.clone().map(|&(lo, _)| lo).min().unwrap_or(0),
        max: reached.map(|&(_, hi)| hi).max().unwrap_or(0),
        short_mean_eighths: mean_eighths,
    }
}

fn row_size_summary(start: Start, columns: &[Column], order: &[usize], nulls: Nulls) -> Summary {
    debug_assert!(
        columns.iter().all(|c| c.kind.is_fixed()),
        "row size is summarized for all-fixed tables"
    );
    // State: (offset residue, unpadded end residue) -> (smallest, largest) padding so far. Both
    // start where the committed prefix ends, which the two rows share.
    let mut states: [Option<(u64, u64)>; 64] = [None; 64];
    let reachable = start.residues(nulls == Nulls::Vary);
    for residue in (0..MAXALIGN).filter(|r| reachable & (1 << r) != 0) {
        states[(residue * 9) as usize] = Some((0, 0));
    }
    let merge = |slot: &mut Option<(u64, u64)>, lo: u64, hi: u64| {
        *slot = Some(match *slot {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    };
    for &index in order {
        let column = columns[index];
        let ColumnKind::Fixed { len, align } = column.kind else {
            continue;
        };
        let may_be_null = column.nullable && nulls == Nulls::Vary;
        let mut next: [Option<(u64, u64)>; 64] = [None; 64];
        for (state, bounds) in states.iter().enumerate() {
            let Some((lo, hi)) = *bounds else { continue };
            let (residue, data) = ((state / 8) as u64, (state % 8) as u64);
            let p = pad(residue, align.bytes());
            let after = ((residue + p + len) % MAXALIGN) * 8 + (data + len) % MAXALIGN;
            merge(&mut next[after as usize], lo + p, hi + p);
            if may_be_null {
                merge(&mut next[state], lo, hi);
            }
        }
        states = next;
    }
    let mut min = u64::MAX;
    let mut max = 0u64;
    for (state, bounds) in states.iter().enumerate() {
        let Some((lo, hi)) = *bounds else { continue };
        let (residue, data) = ((state / 8) as u64, (state % 8) as u64);
        // Both ends add the row's own rounding and drop the unpadded row's.
        let extra = pad(residue, MAXALIGN) as i64 - pad(data, MAXALIGN) as i64;
        min = min.min((lo as i64 + extra) as u64);
        max = max.max((hi as i64 + extra) as u64);
    }
    // The mean: the row that stores every column, uniform over the starts such rows reach.
    let starts: Vec<u64> = (0..MAXALIGN)
        .filter(|r| start.residues(false) & (1 << r) != 0)
        .collect();
    let stored_sum: u64 = starts.iter().map(|&r| stored_cost(columns, order, r)).sum();
    Summary {
        min,
        max,
        short_mean_eighths: stored_sum * MAXALIGN / starts.len() as u64,
    }
}

/// Row-size cost of the row that stores every column of an all-fixed `order` from `start`.
fn stored_cost(columns: &[Column], order: &[usize], start: u64) -> u64 {
    let (mut residue, mut data, mut padding) = (start, start, 0);
    for &index in order {
        if let ColumnKind::Fixed { len, align } = columns[index].kind {
            let p = pad(residue, align.bytes());
            padding += p;
            residue = (residue + p + len) % MAXALIGN;
            data = (data + len) % MAXALIGN;
        }
    }
    padding + pad(residue, MAXALIGN) - pad(data, MAXALIGN)
}

#[cfg(test)]
mod tests;
