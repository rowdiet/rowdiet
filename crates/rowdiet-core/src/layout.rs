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
    maxalign(maxalign(TUPLE_HEADER) + scenario_end_all_fixed)
}

pub fn rows_per_page(footprint: u64) -> u64 {
    (PAGE_SIZE - PAGE_HEADER) / (LINE_POINTER + footprint)
}

pub fn no_null_thoff() -> u64 {
    maxalign(TUPLE_HEADER)
}

/// t_hoff for rows that DO contain a NULL: header + one bitmap bit per table column.
/// Order-invariant — display information only, never reorder advice.
pub fn null_thoff(natts: usize) -> u64 {
    maxalign(TUPLE_HEADER + (natts as u64).div_ceil(8))
}

/// Suggested column order: fixed before varlena; alignment descending; within a fixed alignment
/// group regular sizes before irregulars; varlenas alignment-descending with typmod-proven-short
/// ones last (they never align); stable by original position everywhere else.
pub fn suggested_order(kinds: &[ColumnKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..kinds.len()).collect();
    order.sort_by_key(|&i| sort_key(&kinds[i], i));
    order
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
