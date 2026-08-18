//! Realization-independent comparison of two column orders over the same columns.
//!
//! A realization assigns every varlena column a storage form and a payload length. Padding
//! depends on the realization only through each varlena's form and its payload length mod
//! MAXALIGN: the short form (payloads of 126 bytes or less) advances by 1 + payload with no
//! alignment, a TOAST pointer advances by a fixed 18 bytes (the same shape as a short payload
//! of 17), and the in-line long form aligns to typalign and advances by 4 + payload. Order A
//! **dominates** order B when A's total padding is less than or equal to B's in every
//! realization, and strictly less in at least one — a claim that needs no payload knowledge,
//! which is why the gate and the recommender may act on it (see docs/design.md).
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

/// Storage forms pinned per column for band computation: listed columns are long, all other
/// long-capable columns short/TOAST.
struct FormPin<'a> {
    long: &'a [usize],
}

impl FormPin<'_> {
    /// The form domain for `column`: (short allowed, long allowed).
    fn domain(&self, column: usize, kind: ColumnKind) -> (bool, bool) {
        if !long_capable(kind) {
            return (true, false);
        }
        if self.long.contains(&column) {
            (false, true)
        } else {
            (true, false)
        }
    }
}

fn form_domain(pin: Option<&FormPin<'_>>, column: usize, kind: ColumnKind) -> (bool, bool) {
    match pin {
        Some(p) => p.domain(column, kind),
        None => (true, long_capable(kind)),
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
        .map(|&c| {
            let (short, long) = form_domain(pin, c, kinds[c]);
            (u64::from(short) + u64::from(long)) * MAXALIGN
        })
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
    let segments_a = fixed_segments(kinds, a);
    let segments_b = fixed_segments(kinds, b);
    debug_assert_eq!(segments_a.len(), varlenas.len() + 1);
    debug_assert_eq!(segments_b.len(), varlenas.len() + 1);
    let mut states: [Option<DiffBounds>; 64] = [None; 64];
    let (pad_a0, ra0) = fixed_run(0, &segments_a[0]);
    let (pad_b0, rb0) = fixed_run(0, &segments_b[0]);
    let d0 = pad_a0 as i64 - pad_b0 as i64;
    states[(ra0 * 8 + rb0) as usize] = Some(DiffBounds { min: d0, max: d0 });
    for (k, &column) in varlenas.iter().enumerate() {
        let ColumnKind::Varlena { align, .. } = kinds[column] else {
            unreachable!("varlena sequence holds varlenas")
        };
        let (short, long) = form_domain(pin, column, kinds[column]);
        let mut next: [Option<DiffBounds>; 64] = [None; 64];
        for (state, bounds) in states.iter().enumerate() {
            let Some(bounds) = *bounds else { continue };
            let (ra, rb) = ((state as u64) / 8, (state as u64) % 8);
            for payload in 0..MAXALIGN {
                for form_long in [false, true] {
                    if (form_long && !long) || (!form_long && !short) {
                        continue;
                    }
                    let (pad_a, ra2) = varlena_step(ra, align.bytes(), form_long, payload);
                    let (pad_b, rb2) = varlena_step(rb, align.bytes(), form_long, payload);
                    let (fa, ra3) = fixed_run(ra2, &segments_a[k + 1]);
                    let (fb, rb3) = fixed_run(rb2, &segments_b[k + 1]);
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
    let mut assignment: Vec<(bool, u64)> = vec![(false, 0); varlenas.len()];
    let mut out: Option<DiffBounds> = None;
    enumerate_rec(kinds, a, b, varlenas, pin, &slot_of, &mut assignment, 0, &mut out);
    out.expect("at least one realization exists")
}

#[allow(clippy::too_many_arguments)]
fn enumerate_rec(
    kinds: &[ColumnKind],
    a: &[usize],
    b: &[usize],
    varlenas: &[usize],
    pin: Option<&FormPin<'_>>,
    slot_of: &[usize],
    assignment: &mut Vec<(bool, u64)>,
    depth: usize,
    out: &mut Option<DiffBounds>,
) {
    if depth == varlenas.len() {
        let d = concrete_pad(kinds, a, slot_of, assignment) as i64 - concrete_pad(kinds, b, slot_of, assignment) as i64;
        let bounds = DiffBounds { min: d, max: d };
        match out {
            Some(existing) => existing.merge(bounds),
            slot @ None => *slot = Some(bounds),
        }
        return;
    }
    let column = varlenas[depth];
    let (short, long) = form_domain(pin, column, kinds[column]);
    for payload in 0..MAXALIGN {
        for form_long in [false, true] {
            if (form_long && !long) || (!form_long && !short) {
                continue;
            }
            assignment[depth] = (form_long, payload);
            enumerate_rec(kinds, a, b, varlenas, pin, slot_of, assignment, depth + 1, out);
        }
    }
}

/// Padding of one order under one full realization, residues only.
fn concrete_pad(kinds: &[ColumnKind], order: &[usize], slot_of: &[usize], assignment: &[(bool, u64)]) -> u64 {
    let mut residue = 0u64;
    let mut total = 0u64;
    for &i in order {
        match kinds[i] {
            ColumnKind::Fixed { len, align } => {
                let p = pad(residue, align.bytes());
                total += p;
                residue = (residue + p + len) % MAXALIGN;
            }
            ColumnKind::Varlena { align, .. } => {
                let (form_long, payload) = assignment[slot_of[i]];
                let (p, next) = varlena_step(residue, align.bytes(), form_long, payload);
                total += p;
                residue = next;
            }
        }
    }
    total
}

#[cfg(test)]
mod tests;
