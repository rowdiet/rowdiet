//! On-disk tuple layout math. Assumes 64-bit PostgreSQL (MAXALIGN = 8, `d` alignment = 8 bytes);
//! a 32-bit knob is out of scope for v1.
//!
//! All row-size numbers are computed for the canonical scenario: every column non-NULL and every
//! varlena stored in long form, with varlena payload bytes excluded (payload size is unknowable
//! from DDL and order-invariant; only headers, fixed data, and alignment padding are counted).

/// The 64-bit PostgreSQL MAXALIGN: tuple headers, data starts, and footprints all round to
/// 8-byte boundaries.
pub const MAXALIGN: u64 = 8;
const TUPLE_HEADER: u64 = 23;
const PAGE_SIZE: u64 = 8192;
const PAGE_HEADER: u64 = 24;
const LINE_POINTER: u64 = 4;
const VARLENA_LONG_HEADER: u64 = 4;
const VARLENA_SHORT_HEADER: u64 = 1;

/// pg_type.typalign storage alignment class — the boundary a value's first byte must sit on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "lowercase"))]
pub enum Align {
    /// `c`: byte-aligned — never causes padding.
    Char,
    /// `s`: 2-byte.
    Short,
    /// `i`: 4-byte.
    Int,
    /// `d`: 8-byte (= MAXALIGN on 64-bit).
    Double,
}

impl Align {
    /// The alignment boundary in bytes: 1, 2, 4, or 8.
    pub fn bytes(self) -> u64 {
        match self {
            Self::Char => 1,
            Self::Short => 2,
            Self::Int => 4,
            Self::Double => 8,
        }
    }
}

/// A column's storage class — the only fact about a column the layout math consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum ColumnKind {
    /// Fixed-width type: always `len` bytes on disk.
    Fixed {
        /// pg_type.typlen, bytes.
        len: u64,
        /// Storage alignment.
        align: Align,
    },
    /// Variable-length type, counted at header size only (payload bytes are unknowable from DDL).
    Varlena {
        /// Alignment of the long form; the short form never aligns.
        align: Align,
        /// The typmod proves every value fits the 1-byte-header short form (varchar(n)/char(n),
        /// n ≤ 31): stored unaligned, one byte counted.
        proven_short: bool,
    },
}

impl ColumnKind {
    /// True for [`ColumnKind::Fixed`] — a table of only fixed columns is what earns [`Tier::Exact`].
    pub fn is_fixed(&self) -> bool {
        matches!(self, Self::Fixed { .. })
    }

    /// The declared alignment for either form (for a varlena it governs the long form only).
    pub fn align(&self) -> Align {
        match self {
            Self::Fixed { align, .. } | Self::Varlena { align, .. } => *align,
        }
    }

    /// A fixed-width type whose size is not a multiple of its own alignment (timetz, macaddr):
    /// placing it anywhere but the end of its alignment group forces padding after it.
    pub fn irregular(&self) -> bool {
        match self {
            Self::Fixed { len, align } => len % align.bytes() != 0,
            Self::Varlena { .. } => false,
        }
    }
}

/// Bytes to insert so `offset` lands on a multiple of `align`; 0 when it already does.
pub fn pad(offset: u64, align: u64) -> u64 {
    (align - offset % align) % align
}

/// `n` rounded up to the next 8-byte boundary.
pub fn maxalign(n: u64) -> u64 {
    n + pad(n, MAXALIGN)
}

/// One column's placement in a [`Walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnWalk {
    /// Padding inserted immediately before this column, bytes.
    pub pad_before: u64,
    /// The column's data start, bytes from the beginning of the data area (t_hoff not included).
    pub offset: u64,
}

/// A column order laid out into offsets, under the module doc's canonical scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Placement per column, same order as the walked input.
    pub columns: Vec<ColumnWalk>,
    /// Total inter-column padding: the sum of every `pad_before`. Trailing MAXALIGN rounding is
    /// not part of it — that happens in [`footprint_at`].
    pub padding: u64,
    /// Offset just past the last column's data: the data-area size before footprint rounding.
    pub scenario_end: u64,
}

/// Place `kinds` in the given order and total the padding (canonical scenario: every column
/// non-NULL, varlenas at header size — 4 bytes aligned, or 1 byte unaligned when proven short).
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

/// Rows of this footprint per 8192-byte heap page, after the 24-byte page header and one 4-byte
/// line pointer per row (fillfactor 100, no special space).
pub fn rows_per_page(footprint: u64) -> u64 {
    (PAGE_SIZE - PAGE_HEADER) / (LINE_POINTER + footprint)
}

/// t_hoff for rows with no null bitmap: MAXALIGN(23) = 24.
pub fn bare_thoff() -> u64 {
    maxalign(TUPLE_HEADER)
}

/// t_hoff for rows that carry a null bitmap: header + one bitmap bit per table column.
/// Order-invariant, so it never changes reorder advice.
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
    let classes = fixed_classes(kinds, &order[..fixed_len]);
    if classes.len() > 12 {
        return;
    }
    // Upper bound of the reachable state space: every count combination × offset residue.
    // Pre-sizing spares the memo a dozen rehashes of an ever-growing table; the cap keeps the
    // up-front allocation modest when the bound explodes (3^12 × 8 at the class/column caps);
    // past the cap the map grows the rest of the way as before.
    let states: usize = classes
        .iter()
        .map(|c| c.members.len() + 1)
        .product::<usize>()
        .saturating_mul(MAXALIGN as usize)
        .min(1 << 17);
    let mut dp = Dp {
        classes: &classes,
        memo: std::collections::HashMap::with_capacity_and_hasher(states, PackedKeyHasherBuilder),
    };
    let mut counts: Vec<u8> = classes.iter().map(|c| c.members.len() as u8).collect();
    let mut remaining = fixed_len;
    let mut off = 0u64;
    let mut queues: Vec<std::collections::VecDeque<usize>> =
        classes.iter().map(|c| c.members.iter().copied().collect()).collect();
    let mut refined = Vec::with_capacity(fixed_len);
    while remaining > 0 {
        let target = dp.min_padding(&mut counts, remaining, off);
        for class_index in 0..classes.len() {
            if counts[class_index] == 0 {
                continue;
            }
            let (align, len_mod) = classes[class_index].key;
            let step = pad(off, align);
            counts[class_index] -= 1;
            let rest = dp.min_padding(&mut counts, remaining - 1, (off + step + len_mod) % MAXALIGN);
            if step + rest == target {
                refined.push(queues[class_index].pop_front().expect("count tracked"));
                off = (off + step + len_mod) % MAXALIGN;
                remaining -= 1;
                break;
            }
            counts[class_index] += 1;
        }
    }
    order[..fixed_len].copy_from_slice(&refined);
}

/// Group the fixed prefix of `order` into its padding-equivalence classes, heuristic order
/// preserved (first appearance) so the search's tie-breaking keeps the familiar shape.
fn fixed_classes(kinds: &[ColumnKind], fixed_order: &[usize]) -> Vec<FixedClass> {
    let mut classes: Vec<FixedClass> = Vec::new();
    for &index in fixed_order {
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
    classes
}

struct FixedClass {
    key: (u64, u64),
    members: Vec<usize>,
}

/// [`pad`] for the search's hot loop: alignments are powers of two (1/2/4/8), so the modulo
/// pair reduces to a mask — the div unit is measurable at the memo's node volume.
fn pad_pow2(offset: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    align.wrapping_sub(offset) & (align - 1)
}

struct Dp<'a> {
    classes: &'a [FixedClass],
    memo: std::collections::HashMap<u64, u64, PackedKeyHasherBuilder>,
}

impl Dp<'_> {
    /// `remaining` is the sum of `counts`, carried so the all-placed base case is O(1). The
    /// state fits one u64 — ≤ 12 classes (cap above) of ≤ 24 columns each (5 bits) plus the
    /// offset residue (3 bits) — so the memo never hashes heap data.
    fn min_padding(&mut self, counts: &mut [u8], remaining: usize, off: u64) -> u64 {
        if remaining == 0 {
            return 0;
        }
        let key = counts.iter().fold(off, |k, &c| (k << 5) | u64::from(c));
        if let Some(&cached) = self.memo.get(&key) {
            return cached;
        }
        let mut best = u64::MAX;
        for class_index in 0..self.classes.len() {
            if counts[class_index] == 0 {
                continue;
            }
            let (align, len_mod) = self.classes[class_index].key;
            let step = pad_pow2(off, align);
            counts[class_index] -= 1;
            let total = step + self.min_padding(counts, remaining - 1, (off + step + len_mod) & (MAXALIGN - 1));
            counts[class_index] += 1;
            best = best.min(total);
        }
        self.memo.insert(key, best);
        best
    }
}

/// Multiply-shift hasher for the already-packed DP key — SipHash overhead is measurable at the
/// memo's probe volume, and the key needs mixing only, not DoS resistance (it never hashes
/// attacker-controlled data; the state space is capped).
#[derive(Default)]
struct PackedKeyHasherBuilder;

impl std::hash::BuildHasher for PackedKeyHasherBuilder {
    type Hasher = PackedKeyHasher;

    fn build_hasher(&self) -> PackedKeyHasher {
        PackedKeyHasher(0)
    }
}

struct PackedKeyHasher(u64);

impl std::hash::Hasher for PackedKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("only u64 keys are hashed");
    }

    fn write_u64(&mut self, n: u64) {
        // Fibonacci multiplier, then fold the well-mixed high half down: the table indexes by
        // the low hash bits, and a bare multiply leaves keys that differ only in high fields
        // (the offset residue, early class counts) colliding into one bucket.
        let h = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

fn sort_key(kind: &ColumnKind, index: usize) -> (u8, u64, bool, usize) {
    let align_desc = |a: Align| MAXALIGN - a.bytes();
    match kind {
        ColumnKind::Fixed { align, .. } => (0, align_desc(*align), kind.irregular(), index),
        ColumnKind::Varlena {
            align,
            proven_short: false,
        } => (1, align_desc(*align), false, index),
        ColumnKind::Varlena {
            align,
            proven_short: true,
        } => (2, align_desc(*align), false, index),
    }
}

/// How solid the reported numbers are. `Exact`: only fixed-width columns — padding and footprint
/// are byte-exact and order-guaranteed. `Estimate`: at least one varlena — numbers describe the
/// long-form scenario; real rows are data-dependent (short-form/TOAST), so savings are bounds,
/// never guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum Tier {
    /// Only fixed-width columns: byte-exact, order-guaranteed.
    Exact,
    /// At least one varlena: long-form-scenario numbers — bounds, not guarantees.
    Estimate,
}

/// The tier `kinds` report at: [`Tier::Exact`] iff every column is fixed-width.
pub fn tier(kinds: &[ColumnKind]) -> Tier {
    if kinds.iter().all(ColumnKind::is_fixed) {
        Tier::Exact
    } else {
        Tier::Estimate
    }
}

#[cfg(test)]
mod tests;
