//! On-disk tuple layout math. Assumes 64-bit PostgreSQL (MAXALIGN = 8, `d` alignment = 8 bytes);
//! a 32-bit knob is out of scope for v1.
//!
//! All row-size numbers are computed for the canonical scenario: every column non-NULL and every
//! varlena stored in long form, with varlena payload bytes excluded (payload size is unknowable
//! from DDL and order-invariant; only headers, fixed data, and alignment padding are counted).

pub const MAXALIGN: u64 = 8;
const TUPLE_HEADER: u64 = 23;
const PAGE_SIZE: u64 = 8192;
const PAGE_HEADER: u64 = 24;
const LINE_POINTER: u64 = 4;
const VARLENA_LONG_HEADER: u64 = 4;
const VARLENA_SHORT_HEADER: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "lowercase"))]
pub enum Align {
    Char,
    Short,
    Int,
    Double,
}

impl Align {
    pub fn bytes(self) -> u64 {
        match self {
            Align::Char => 1,
            Align::Short => 2,
            Align::Int => 4,
            Align::Double => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum ColumnKind {
    Fixed { len: u64, align: Align },
    Varlena { align: Align, proven_short: bool },
}

impl ColumnKind {
    pub fn is_fixed(&self) -> bool {
        matches!(self, ColumnKind::Fixed { .. })
    }

    pub fn align(&self) -> Align {
        match self {
            ColumnKind::Fixed { align, .. } => *align,
            ColumnKind::Varlena { align, .. } => *align,
        }
    }

    /// A fixed-width type whose size is not a multiple of its own alignment (timetz, macaddr):
    /// placing it anywhere but the end of its alignment group forces padding after it.
    pub fn irregular(&self) -> bool {
        match self {
            ColumnKind::Fixed { len, align } => len % align.bytes() != 0,
            ColumnKind::Varlena { .. } => false,
        }
    }
}

pub fn pad(offset: u64, align: u64) -> u64 {
    (align - offset % align) % align
}

pub fn maxalign(n: u64) -> u64 {
    n + pad(n, MAXALIGN)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnWalk {
    pub pad_before: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    pub columns: Vec<ColumnWalk>,
    pub padding: u64,
    pub scenario_end: u64,
}

pub fn walk(kinds: &[ColumnKind]) -> Walk {
    let mut off = 0u64;
    let mut padding = 0u64;
    let mut columns = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let (p, size) = match kind {
            ColumnKind::Fixed { len, align } => (pad(off, align.bytes()), *len),
            // A short varlena (1-byte header) is stored with no alignment at all (tupmacs.h).
            ColumnKind::Varlena { proven_short: true, .. } => (0, VARLENA_SHORT_HEADER),
            ColumnKind::Varlena {
                align,
                proven_short: false,
            } => (pad(off, align.bytes()), VARLENA_LONG_HEADER),
        };
        off += p;
        columns.push(ColumnWalk {
            pad_before: p,
            offset: off,
        });
        off += size;
        padding += p;
    }
    Walk {
        columns,
        padding,
        scenario_end: off,
    }
}

/// Per-row on-disk footprint for a table of only fixed-width columns, no-NULL scenario:
/// MAXALIGN(t_hoff) + data, MAXALIGN-rounded as the page placement does (bufpage.c).
pub fn footprint(scenario_end_all_fixed: u64) -> u64 {
    footprint_at(maxalign(TUPLE_HEADER), scenario_end_all_fixed)
}

/// Footprint with an explicit data-start offset — used when dropped columns force a null
/// bitmap into every new row (`t_hoff = null_thoff(original natts)`).
pub fn footprint_at(t_hoff: u64, scenario_end_all_fixed: u64) -> u64 {
    maxalign(t_hoff + scenario_end_all_fixed)
}

pub fn rows_per_page(footprint: u64) -> u64 {
    (PAGE_SIZE - PAGE_HEADER) / (LINE_POINTER + footprint)
}

pub fn no_null_thoff() -> u64 {
    maxalign(TUPLE_HEADER)
}

/// t_hoff for rows that DO contain a NULL: header + one bitmap bit per table column.
/// Order-invariant — display information only, never reorder advice.
pub fn bare_thoff() -> u64 {
    maxalign(TUPLE_HEADER)
}

pub fn null_thoff(natts: usize) -> u64 {
    maxalign(TUPLE_HEADER + (natts as u64).div_ceil(8))
}

/// Suggested column order: fixed before varlena; alignment descending; within a fixed alignment
/// group regular sizes before irregulars; varlenas alignment-descending with typmod-proven-short
/// ones last (they never align); stable by original position everywhere else.
pub fn suggested_order(kinds: &[ColumnKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..kinds.len()).collect();
    order.sort_by_key(|&i| sort_key(&kinds[i], i));
    refine_fixed_block(kinds, &mut order);
    order
}

/// Descending-alignment sorting is provably zero-padding only while every size is a multiple of
/// its own alignment; with two or more irregulars (timetz, macaddr, …) it can leave padding an
/// interposed smaller column would absorb. When the sorted fixed block still pads, find the
/// exact minimum: padding depends only on (alignment, len mod MAXALIGN) classes and the running
/// offset mod MAXALIGN, so a small memoized search over class counts is exhaustive. Ties prefer
/// the heuristic's own class order, so regular schemas keep their familiar shape.
fn refine_fixed_block(kinds: &[ColumnKind], order: &mut [usize]) {
    let fixed_len = order.iter().take_while(|&&i| kinds[i].is_fixed()).count();
    if !(3..=24).contains(&fixed_len) {
        return;
    }
    let sorted_fixed: Vec<ColumnKind> = order[..fixed_len].iter().map(|&i| kinds[i]).collect();
    if walk(&sorted_fixed).padding == 0 {
        return;
    }
    let mut classes: Vec<FixedClass> = Vec::new();
    for &index in order[..fixed_len].iter() {
        let ColumnKind::Fixed { len, align } = kinds[index] else {
            unreachable!("fixed prefix")
        };
        let key = (align.bytes(), len % MAXALIGN);
        match classes.iter_mut().find(|c| c.key == key) {
            Some(class) => class.members.push(index),
            None => classes.push(FixedClass {
                key,
                members: vec![index],
            }),
        }
    }
    if classes.len() > 12 {
        return;
    }
    let mut dp = Dp {
        classes: &classes,
        memo: std::collections::HashMap::new(),
    };
    let full: Vec<u8> = classes.iter().map(|c| c.members.len() as u8).collect();
    let mut counts = full;
    let mut off = 0u64;
    let mut queues: Vec<std::collections::VecDeque<usize>> =
        classes.iter().map(|c| c.members.iter().copied().collect()).collect();
    let mut refined = Vec::with_capacity(fixed_len);
    while counts.iter().any(|&c| c > 0) {
        let target = dp.min_padding(&counts, off);
        for class_index in 0..classes.len() {
            if counts[class_index] == 0 {
                continue;
            }
            let (align, len_mod) = classes[class_index].key;
            let step = pad(off, align);
            counts[class_index] -= 1;
            let rest = dp.min_padding(&counts, (off + step + len_mod) % MAXALIGN);
            if step + rest == target {
                refined.push(queues[class_index].pop_front().expect("count tracked"));
                off = (off + step + len_mod) % MAXALIGN;
                break;
            }
            counts[class_index] += 1;
        }
    }
    order[..fixed_len].copy_from_slice(&refined);
}

struct FixedClass {
    key: (u64, u64),
    members: Vec<usize>,
}

struct Dp<'a> {
    classes: &'a [FixedClass],
    memo: std::collections::HashMap<(Vec<u8>, u64), u64>,
}

impl Dp<'_> {
    fn min_padding(&mut self, counts: &[u8], off: u64) -> u64 {
        if counts.iter().all(|&c| c == 0) {
            return 0;
        }
        let key = (counts.to_vec(), off);
        if let Some(&cached) = self.memo.get(&key) {
            return cached;
        }
        let mut best = u64::MAX;
        let mut next = counts.to_vec();
        for class_index in 0..self.classes.len() {
            if next[class_index] == 0 {
                continue;
            }
            let (align, len_mod) = self.classes[class_index].key;
            let step = pad(off, align);
            next[class_index] -= 1;
            let total = step + self.min_padding(&next, (off + step + len_mod) % MAXALIGN);
            next[class_index] += 1;
            best = best.min(total);
        }
        self.memo.insert(key, best);
        best
    }
}

fn sort_key(kind: &ColumnKind, index: usize) -> (u8, u8, u8, usize) {
    let align_desc = |a: Align| 8 - a.bytes() as u8;
    match kind {
        ColumnKind::Fixed { align, .. } => (0, align_desc(*align), kind.irregular() as u8, index),
        ColumnKind::Varlena {
            align,
            proven_short: false,
        } => (1, align_desc(*align), 0, index),
        ColumnKind::Varlena {
            align,
            proven_short: true,
        } => (2, align_desc(*align), 0, index),
    }
}

/// How solid the reported numbers are. `Exact`: only fixed-width columns — padding and footprint
/// are byte-exact and order-guaranteed. `Estimate`: at least one varlena — numbers describe the
/// long-form scenario; real rows are data-dependent (short-form/TOAST), so savings are bounds,
/// never guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum Tier {
    Exact,
    Estimate,
}

pub fn tier(kinds: &[ColumnKind]) -> Tier {
    if kinds.iter().all(ColumnKind::is_fixed) {
        Tier::Exact
    } else {
        Tier::Estimate
    }
}

#[cfg(test)]
mod tests;
