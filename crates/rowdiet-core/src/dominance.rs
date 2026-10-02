//! Realization-independent comparison of two column orders over the same columns.
//!
//! A realization assigns every varlena column a storage form and a payload length. Padding
//! depends on the realization only through each varlena's form and its payload length mod
//! MAXALIGN: the short form (payloads of 126 bytes or less) advances by 1 + payload with no
//! alignment, a TOAST pointer advances by a fixed 18 bytes (the same shape as a short payload
//! of 17), and the in-line long form aligns to typalign and advances by 4 + payload. A short
//! value is stored uncompressed, so its payload residues are the type's own
//! ([`Payload`](crate::layout::Payload): any for text, even for numeric, `4 + k * stride` for
//! arrays); a long value may be compressed and reaches every residue. Order A **dominates**
//! order B when A's total padding is less than or equal to B's in every realization, and
//! strictly less in at least one — a claim that needs no payload knowledge, which is why the
//! gate and the recommender may act on it (see docs/design.md).
//!
//! Two exact engines compute the bounds of `pad(A) − pad(B)` over all realizations:
//!
//! - When both orders keep the varlenas in the same relative sequence, a joint walk over the
//!   pair of offset residues (64 states) visits each varlena once; per-column realizations are
//!   independent, so extremes compose state by state. Exact at any column count, near-free.
//! - Otherwise, exhaustive enumeration of per-varlena (form, payload residue) assignments,
//!   within a state budget. Past the budget the comparison is reported as undecided rather
//!   than guessed.

use crate::layout::{ColumnKind, MAXALIGN, pad};

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

    fn merge(&mut self, other: Self) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }
}

/// One storage-form band of a frontier: the named varlenas store the in-line long form, every
/// other varlena stays short or TOAST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Column indices (into `kinds`) stored long-form in this band.
    pub long_form: Vec<usize>,
    /// `pad(a) − pad(b)` bounds within the band.
    pub diff: DiffBounds,
}

/// Enumeration budget: the product of per-varlena realization counts (16, or 8 when the form
/// is pinned) must stay under this for the exhaustive engine to run.
const ENUMERATION_BUDGET: u64 = 1 << 21;

/// Bands are enumerated per form combination; more long-capable varlenas than this and the
/// frontier is reported without band detail.
const BAND_LIMIT: usize = 6;

/// Estimated work for [`compare`] in realization-walk units: near-free for a pair sharing its
/// varlena sequence, the enumeration state count otherwise, and `u64::MAX` when [`compare`]
/// would return None. Lets a caller ration a comparison budget before spending it.
pub fn comparison_cost(kinds: &[ColumnKind], a: &[usize], b: &[usize]) -> u64 {
    let va = varlena_sequence(kinds, a);
    let vb = varlena_sequence(kinds, b);
    if va == vb {
        return (va.len() as u64).max(1) * 64;
    }
    let states = enumeration_states(kinds, &va, None);
    if states <= ENUMERATION_BUDGET { states } else { u64::MAX }
}

/// Bounds of `pad(a) − pad(b)` over every realization, or None when the pair is out of budget
/// (varlena sequences differ and the enumeration space is too large).
pub fn compare(kinds: &[ColumnKind], a: &[usize], b: &[usize]) -> Option<DiffBounds> {
    debug_assert_eq!(a.len(), kinds.len());
    debug_assert_eq!(b.len(), kinds.len());
    let va: Vec<usize> = varlena_sequence(kinds, a);
    let vb: Vec<usize> = varlena_sequence(kinds, b);
    if va == vb {
        return Some(joint_walk(kinds, a, b, &va, None));
    }
    if enumeration_states(kinds, &va, None) <= ENUMERATION_BUDGET {
        return Some(enumerate(kinds, a, b, &va, None));
    }
    None
}

/// The frontier bands for an incomparable pair: one entry per storage-form combination, in
/// ascending order of long-form column count. None past the band limit or budget.
pub fn bands(kinds: &[ColumnKind], a: &[usize], b: &[usize]) -> Option<Vec<Band>> {
    let va: Vec<usize> = varlena_sequence(kinds, a);
    let vb: Vec<usize> = varlena_sequence(kinds, b);
    let long_capable: Vec<usize> = va.iter().copied().filter(|&c| long_capable(kinds[c])).collect();
    if long_capable.len() > BAND_LIMIT {
        return None;
    }
    let same_sequence = va == vb;
    let mut out = Vec::with_capacity(1 << long_capable.len());
    for combo in 0u32..(1 << long_capable.len()) {
        let long_form: Vec<usize> = long_capable
            .iter()
            .enumerate()
            .filter(|(bit, _)| combo & (1 << bit) != 0)
            .map(|(_, &c)| c)
            .collect();
        let pin = FormPin { long: &long_form };
        let diff = if same_sequence {
            joint_walk(kinds, a, b, &va, Some(&pin))
        } else if enumeration_states(kinds, &va, Some(&pin)) <= ENUMERATION_BUDGET {
            enumerate(kinds, a, b, &va, Some(&pin))
        } else {
            return None;
        };
        out.push(Band { long_form, diff });
    }
    out.sort_by_key(|band| band.long_form.len());
    Some(out)
}

/// True when the column can store the in-line long form at all; proven-short varlenas cannot.
fn long_capable(kind: ColumnKind) -> bool {
    matches!(
        kind,
        ColumnKind::Varlena {
            proven_short: false,
            ..
        }
    )
}

/// The payload residue a TOAST pointer advances like: 18 bytes is a short payload of 17.
const TOAST_RESIDUE: u8 = 1 << 1;

/// A varlena's realizations as payload-residue bitmasks per form: `short` stores unaligned (a
/// 1-byte header, or a TOAST pointer), `long` aligns to typalign behind a 4-byte header.
#[derive(Debug, Clone, Copy)]
struct Domain {
    short: u8,
    long: u8,
}

impl Domain {
    fn of(kind: ColumnKind) -> Self {
        match kind {
            ColumnKind::Varlena {
                proven_short: true,
                payload,
                ..
            } => Self {
                short: payload.residues(),
                long: 0,
            },
            ColumnKind::Varlena { payload, .. } => Self {
                short: payload.residues() | TOAST_RESIDUE,
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
/// long-capable columns short/TOAST.
struct FormPin<'a> {
    long: &'a [usize],
}

impl FormPin<'_> {
    /// The realization domain for `column` with its form pinned.
    fn domain(&self, column: usize, kind: ColumnKind) -> Domain {
        let full = Domain::of(kind);
        if long_capable(kind) && self.long.contains(&column) {
            Domain { short: 0, ..full }
        } else {
            Domain { long: 0, ..full }
        }
    }
}

fn form_domain(pin: Option<&FormPin<'_>>, column: usize, kind: ColumnKind) -> Domain {
    match pin {
        Some(p) => p.domain(column, kind),
        None => Domain::of(kind),
    }
}

/// Varlena column indices in walk order.
fn varlena_sequence(kinds: &[ColumnKind], order: &[usize]) -> Vec<usize> {
    order
        .iter()
        .copied()
        .filter(|&i| matches!(kinds[i], ColumnKind::Varlena { .. }))
        .collect()
}

fn enumeration_states(kinds: &[ColumnKind], varlenas: &[usize], pin: Option<&FormPin<'_>>) -> u64 {
    varlenas
        .iter()
        .map(|&c| form_domain(pin, c, kinds[c]).size())
        .try_fold(1u64, u64::checked_mul)
        .unwrap_or(u64::MAX)
}

/// Exact bounds when both orders share the varlena sequence: a DP over the pair of offset
/// residues. Padding depends on offsets only mod MAXALIGN, per-column realizations are
/// independent, and every residue pair reached carries its attainable diff extremes, so
/// min/max compose exactly.
fn joint_walk(
    kinds: &[ColumnKind],
    a: &[usize],
    b: &[usize],
    varlenas: &[usize],
    pin: Option<&FormPin<'_>>,
) -> DiffBounds {
    let runs_a = run_tables(kinds, a);
    let runs_b = run_tables(kinds, b);
    debug_assert_eq!(runs_a.len(), varlenas.len() + 1);
    debug_assert_eq!(runs_b.len(), varlenas.len() + 1);
    let mut states: [Option<DiffBounds>; 64] = [None; 64];
    let (pad_a0, ra0) = runs_a[0][0];
    let (pad_b0, rb0) = runs_b[0][0];
    let d0 = pad_a0 as i64 - pad_b0 as i64;
    states[(ra0 * 8 + rb0) as usize] = Some(DiffBounds { min: d0, max: d0 });
    for (k, &column) in varlenas.iter().enumerate() {
        let ColumnKind::Varlena { align, .. } = kinds[column] else {
            unreachable!("varlena sequence holds varlenas")
        };
        let domain = form_domain(pin, column, kinds[column]);
        let mut next: [Option<DiffBounds>; 64] = [None; 64];
        for (state, bounds) in states.iter().enumerate() {
            let Some(bounds) = *bounds else { continue };
            let (ra, rb) = ((state as u64) / 8, (state as u64) % 8);
            for payload in 0..MAXALIGN {
                for form_long in [false, true] {
                    if !domain.allows(form_long, payload) {
                        continue;
                    }
                    let (pad_a, ra2) = varlena_step(ra, align.bytes(), form_long, payload);
                    let (pad_b, rb2) = varlena_step(rb, align.bytes(), form_long, payload);
                    let (fa, ra3) = runs_a[k + 1][ra2 as usize];
                    let (fb, rb3) = runs_b[k + 1][rb2 as usize];
                    let delta = (pad_a + fa) as i64 - (pad_b + fb) as i64;
                    let moved = DiffBounds {
                        min: bounds.min + delta,
                        max: bounds.max + delta,
                    };
                    match &mut next[(ra3 * 8 + rb3) as usize] {
                        Some(existing) => existing.merge(moved),
                        slot @ None => *slot = Some(moved),
                    }
                }
            }
        }
        states = next;
    }
    let mut out: Option<DiffBounds> = None;
    for bounds in states.into_iter().flatten() {
        match &mut out {
            Some(existing) => existing.merge(bounds),
            slot @ None => *slot = Some(bounds),
        }
    }
    out.expect("at least one realization exists")
}

/// One order's padding over the realization model: exact bounds over every realization, and the
/// mean over the sub-distribution where every varlena stores a short uncompressed payload uniform
/// over its type's residues. Dominance implies <= on all three, which makes them sound prunes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Smallest attainable total padding, bytes.
    pub min: u64,
    /// Largest attainable total padding, bytes.
    pub max: u64,
    /// The short-form mean, eighths of a byte (uniform residues over cosets keep it exact).
    pub short_mean_eighths: u64,
}

/// [`Summary`] of `order`: a walk over the eight offset residues for the bounds, and over one
/// uniform coset of residues for the mean.
pub fn summary(kinds: &[ColumnKind], order: &[usize]) -> Summary {
    let mut states: [Option<(u64, u64)>; MAXALIGN as usize] = [None; MAXALIGN as usize];
    states[0] = Some((0, 0));
    let mut set: u8 = 1;
    let mut mean_eighths = 0u64;
    let merge = |slot: &mut Option<(u64, u64)>, lo: u64, hi: u64| {
        *slot = Some(match *slot {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    };
    for &column in order {
        let kind = kinds[column];
        let mut next: [Option<(u64, u64)>; MAXALIGN as usize] = [None; MAXALIGN as usize];
        let mut next_set = 0u8;
        match kind {
            ColumnKind::Fixed { len, align } => {
                for (residue, bounds) in states.iter().enumerate() {
                    let Some((lo, hi)) = *bounds else { continue };
                    let p = pad(residue as u64, align.bytes());
                    merge(
                        &mut next[((residue as u64 + p + len) % MAXALIGN) as usize],
                        lo + p,
                        hi + p,
                    );
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
                let domain = Domain::of(kind);
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

/// One varlena placement from a residue: (pad, residue after the value).
fn varlena_step(residue: u64, align: u64, long: bool, payload: u64) -> (u64, u64) {
    if long {
        let p = pad(residue, align);
        (p, (residue + p + 4 + payload) % MAXALIGN)
    } else {
        (0, (residue + 1 + payload) % MAXALIGN)
    }
}

/// The fixed-column runs between varlenas: `varlena count + 1` segments in walk order.
fn fixed_segments(kinds: &[ColumnKind], order: &[usize]) -> Vec<Vec<(u64, u64)>> {
    let mut segments: Vec<Vec<(u64, u64)>> = vec![Vec::new()];
    for &i in order {
        match kinds[i] {
            ColumnKind::Fixed { len, align } => {
                segments
                    .last_mut()
                    .expect("segments start non-empty")
                    .push((align.bytes(), len % MAXALIGN));
            }
            ColumnKind::Varlena { .. } => segments.push(Vec::new()),
        }
    }
    segments
}

/// A fixed run's effect from each start residue: (padding total, residue after).
type RunTable = [(u64, u64); MAXALIGN as usize];

/// One [`RunTable`] per fixed run of `order`, so a walk costs a step per varlena at any width.
fn run_tables(kinds: &[ColumnKind], order: &[usize]) -> Vec<RunTable> {
    fixed_segments(kinds, order)
        .iter()
        .map(|segment| std::array::from_fn(|residue| fixed_run(residue as u64, segment)))
        .collect()
}

/// Walk a fixed run from a residue: (padding total, residue after).
fn fixed_run(mut residue: u64, segment: &[(u64, u64)]) -> (u64, u64) {
    let mut total = 0;
    for &(align, len_mod) in segment {
        let p = pad(residue, align);
        total += p;
        residue = (residue + p + len_mod) % MAXALIGN;
    }
    (total, residue)
}

/// Exhaustive engine for pairs whose varlena sequences differ: enumerate every per-column
/// (form, payload residue) assignment and walk both orders concretely.
fn enumerate(
    kinds: &[ColumnKind],
    a: &[usize],
    b: &[usize],
    varlenas: &[usize],
    pin: Option<&FormPin<'_>>,
) -> DiffBounds {
    let mut slot_of = vec![usize::MAX; kinds.len()];
    for (slot, &c) in varlenas.iter().enumerate() {
        slot_of[c] = slot;
    }
    let plans = [WalkPlan::new(kinds, a, &slot_of), WalkPlan::new(kinds, b, &slot_of)];
    let mut assignment: Vec<(bool, u64)> = vec![(false, 0); varlenas.len()];
    let mut out: Option<DiffBounds> = None;
    enumerate_rec(kinds, &plans, varlenas, pin, &mut assignment, 0, &mut out);
    out.expect("at least one realization exists")
}

/// One order reduced to its varlenas (as slots into the assignment) and the fixed runs between.
struct WalkPlan {
    varlenas: Vec<(usize, u64)>,
    runs: Vec<RunTable>,
}

impl WalkPlan {
    fn new(kinds: &[ColumnKind], order: &[usize], slot_of: &[usize]) -> Self {
        let varlenas = varlena_sequence(kinds, order)
            .into_iter()
            .map(|c| (slot_of[c], kinds[c].align().bytes()))
            .collect();
        Self {
            varlenas,
            runs: run_tables(kinds, order),
        }
    }

    /// Padding of the order under one full realization, residues only.
    fn pad(&self, assignment: &[(bool, u64)]) -> u64 {
        let (mut total, mut residue) = self.runs[0][0];
        for (k, &(slot, align)) in self.varlenas.iter().enumerate() {
            let (form_long, payload) = assignment[slot];
            let (p, next) = varlena_step(residue, align, form_long, payload);
            let (fixed, after) = self.runs[k + 1][next as usize];
            total += p + fixed;
            residue = after;
        }
        total
    }
}

fn enumerate_rec(
    kinds: &[ColumnKind],
    plans: &[WalkPlan; 2],
    varlenas: &[usize],
    pin: Option<&FormPin<'_>>,
    assignment: &mut Vec<(bool, u64)>,
    depth: usize,
    out: &mut Option<DiffBounds>,
) {
    if depth == varlenas.len() {
        let d = plans[0].pad(assignment) as i64 - plans[1].pad(assignment) as i64;
        let bounds = DiffBounds { min: d, max: d };
        match out {
            Some(existing) => existing.merge(bounds),
            slot @ None => *slot = Some(bounds),
        }
        return;
    }
    let column = varlenas[depth];
    let domain = form_domain(pin, column, kinds[column]);
    for payload in 0..MAXALIGN {
        for form_long in [false, true] {
            if !domain.allows(form_long, payload) {
                continue;
            }
            assignment[depth] = (form_long, payload);
            enumerate_rec(kinds, plans, varlenas, pin, assignment, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests;
