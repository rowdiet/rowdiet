//! On-disk tuple layout math. Assumes 64-bit PostgreSQL (MAXALIGN = 8, `d` alignment = 8 bytes);
//! a 32-bit knob is out of scope for v1.
//!
//! All row numbers assume every column non-NULL. Varlena payload bytes never count toward sizes
//! (they are unknowable from DDL), but they do move every later column's offset: from the first
//! varlena on, an offset is known only as a set of possible residues mod MAXALIGN, and each
//! later pad is reported as a min/max/expected range over that set (residues taken as uniformly
//! likely, by assumption). Pads placed while the offset is exactly
//! known stay exact, so all-fixed tables keep byte-exact numbers.

/// The 64-bit PostgreSQL MAXALIGN: tuple headers, data starts, and footprints all round to
/// 8-byte boundaries.
pub const MAXALIGN: u64 = 8;
const TUPLE_HEADER: u64 = 23;
const PAGE_SIZE: u64 = 8192;
const PAGE_HEADER: u64 = 24;
const LINE_POINTER: u64 = 4;

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
    /// Variable-length type. Its payload length is unknowable from DDL, so every column placed
    /// after one sits at a data-dependent offset.
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

/// One pad's bounds and expectation. When the column sits at an exactly known offset (or every
/// possible offset pads the same), `min == max` and the pad is that one value; otherwise the
/// numbers range over the possible offset residues mod MAXALIGN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadRange {
    /// Smallest possible pad, bytes.
    pub min: u64,
    /// Largest possible pad, bytes.
    pub max: u64,
    /// Mean pad over the possible residues, in eighths of a byte — eighths keep it exact, since
    /// reachable residue sets have 1, 2, 4, or 8 members, all dividing 8.
    pub expected_eighths: u64,
}

impl PadRange {
    /// A pad of exactly `bytes`.
    pub fn certain(bytes: u64) -> Self {
        Self {
            min: bytes,
            max: bytes,
            expected_eighths: bytes * MAXALIGN,
        }
    }

    /// The pad when its value is certain (`min == max`), even if the absolute offset is not.
    pub fn exact(&self) -> Option<u64> {
        (self.min == self.max).then_some(self.min)
    }

    /// Mean pad in bytes (a multiple of 1/8, exactly representable in f64).
    pub fn expected(&self) -> f64 {
        self.expected_eighths as f64 / 8.0
    }
}

/// The offsets a column can start at, reduced mod MAXALIGN: a bitmask over residues 0..=7.
/// Starts as `{0}`; a fixed column narrows the set by its alignment then shifts it by its
/// length; any varlena replaces it with the full set — a proven-short typmod bounds only the
/// header form, the payload byte length still varies (multibyte encodings), so the set stays full.
/// Expected pads treat the members as uniformly likely. That assumption is made once, at the
/// widening: alignment maps preserve uniformity (reachable sets are cosets in Z/8, and each
/// surviving residue absorbs equally many predecessors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Residues(u8);

impl Residues {
    /// The walk's start: offset 0 exactly.
    const START: Self = Self(1);
    /// Every residue possible: the state after any varlena.
    const FULL: Self = Self(0xFF);

    fn contains(self, residue: u64) -> bool {
        self.0 & (1u8 << residue) != 0
    }

    fn len(self) -> u64 {
        u64::from(self.0.count_ones())
    }

    /// Pad statistics for aligning to `align` from any residue in the set.
    fn pad_to(self, align: u64) -> PadRange {
        let mut min = u64::MAX;
        let mut max = 0u64;
        let mut sum = 0u64;
        for residue in 0..MAXALIGN {
            if self.contains(residue) {
                let p = pad(residue, align);
                min = min.min(p);
                max = max.max(p);
                sum += p;
            }
        }
        // Coset sizes are 1, 2, 4, or 8, so the mean in eighths divides exactly.
        debug_assert_eq!(sum * MAXALIGN % self.len(), 0);
        PadRange {
            min,
            max,
            expected_eighths: sum * MAXALIGN / self.len(),
        }
    }

    /// The set after aligning to `align`: each residue moves to its next `align` boundary.
    /// Aligning to 8 collapses any set to `{0}`; aligning the full set to 4 leaves `{0, 4}`.
    fn aligned(self, align: u64) -> Self {
        let mut mask = 0u8;
        for residue in 0..MAXALIGN {
            if self.contains(residue) {
                mask |= 1u8 << ((residue + pad(residue, align)) % MAXALIGN);
            }
        }
        Self(mask)
    }

    /// The set after advancing by `len` bytes.
    fn shifted(self, len: u64) -> Self {
        Self(self.0.rotate_left((len % MAXALIGN) as u32))
    }
}

/// One column's placement in a [`Walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnWalk {
    /// Padding inserted immediately before this column.
    pub pad_before: PadRange,
    /// The column's data start, bytes from the beginning of the data area (t_hoff not
    /// included) — known only until the first varlena, whose payload moves every later offset.
    pub offset: Option<u64>,
}

/// A column order laid out into offsets, under the module doc's no-NULL assumption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Placement per column, same order as the walked input.
    pub columns: Vec<ColumnWalk>,
    /// Total certain padding: the sum of every pad whose value is known, bytes — all of it, for
    /// an all-fixed table. Trailing MAXALIGN rounding is not part of it — that happens in
    /// [`footprint_at`].
    pub padding: u64,
    /// Sum of the smallest possible values of the data-dependent pads, bytes.
    pub uncertain_min: u64,
    /// Sum of the largest possible values of the data-dependent pads, bytes.
    pub uncertain_max: u64,
    /// Sum of the expected values of the data-dependent pads, eighths of a byte.
    pub uncertain_expected_eighths: u64,
    /// Offset just past the last column's data, when exactly known: None once the table has a
    /// varlena (its payload length moves the end).
    pub end: Option<u64>,
}

impl Walk {
    /// Expected total padding in eighths of a byte: certain pads plus the expected values of
    /// the data-dependent ones. Exact integer arithmetic — compare orders on this.
    pub fn expected_padding_eighths(&self) -> u64 {
        self.padding * MAXALIGN + self.uncertain_expected_eighths
    }

    /// Expected total padding, bytes (a multiple of 1/8, exactly representable in f64).
    pub fn expected_padding(&self) -> f64 {
        self.expected_padding_eighths() as f64 / 8.0
    }

    /// Smallest possible total padding, bytes.
    pub fn padding_min(&self) -> u64 {
        self.padding + self.uncertain_min
    }

    /// Largest possible total padding, bytes.
    pub fn padding_max(&self) -> u64 {
        self.padding + self.uncertain_max
    }
}

/// Place `kinds` in the given order and total the padding (every column non-NULL; pads placed
/// after the first varlena are min/max/expected ranges over the possible offset residues).
pub fn walk(kinds: &[ColumnKind]) -> Walk {
    let mut residues = Residues::START;
    let mut end = Some(0u64);
    let mut padding = 0u64;
    let mut uncertain_min = 0u64;
    let mut uncertain_max = 0u64;
    let mut uncertain_expected_eighths = 0u64;
    let mut columns = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let pad_before = match kind {
            ColumnKind::Fixed { align, .. } => residues.pad_to(align.bytes()),
            // A short varlena (1-byte header) is stored with no alignment at all (tupmacs.h).
            ColumnKind::Varlena { proven_short: true, .. } => PadRange::certain(0),
            ColumnKind::Varlena {
                align,
                proven_short: false,
            } => residues.pad_to(align.bytes()),
        };
        match pad_before.exact() {
            Some(p) => padding += p,
            None => {
                uncertain_min += pad_before.min;
                uncertain_max += pad_before.max;
                uncertain_expected_eighths += pad_before.expected_eighths;
            }
        }
        columns.push(ColumnWalk {
            pad_before,
            // While `end` is known the residue set is a singleton, so `min` is the pad.
            offset: end.map(|e| e + pad_before.min),
        });
        match kind {
            ColumnKind::Fixed { len, align } => {
                residues = residues.aligned(align.bytes()).shifted(*len);
                end = end.map(|e| e + pad_before.min + len);
            }
            ColumnKind::Varlena { .. } => {
                residues = Residues::FULL;
                end = None;
            }
        }
    }
    Walk {
        columns,
        padding,
        uncertain_min,
        uncertain_max,
        uncertain_expected_eighths,
        end,
    }
}

/// Per-row on-disk footprint for a table of only fixed-width columns, no-NULL scenario:
/// MAXALIGN(t_hoff) + data, MAXALIGN-rounded as the page placement does (bufpage.c).
pub fn footprint(data_end_all_fixed: u64) -> u64 {
    footprint_at(maxalign(TUPLE_HEADER), data_end_all_fixed)
}

/// Footprint with an explicit data-start offset — used when dropped columns force a null
/// bitmap into every new row (`t_hoff = null_thoff(original natts)`).
pub fn footprint_at(t_hoff: u64, data_end_all_fixed: u64) -> u64 {
    maxalign(t_hoff + data_end_all_fixed)
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
/// are byte-exact and order-guaranteed. `Estimate`: at least one varlena — columns after it sit
/// at data-dependent offsets, so padding is an expected value with a min/max range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum Tier {
    /// Only fixed-width columns: byte-exact, order-guaranteed.
    Exact,
    /// At least one varlena: columns after it sit at data-dependent offsets — padding is
    /// reported as expected values with bounds.
    Estimate,
    /// The table's columns are not fully known (an unexpanded LIKE/INHERITS/typed table): no
    /// footprint is claimed. Assigned when the table is incomplete, never inferred from `kinds`
    /// — an empty but complete table is still [`Exact`](Self::Exact).
    Unknown,
}

/// The tier's stable tag — the same word the serde representation carries (`exact`,
/// `estimate`), so logs and JSON name tiers identically. Renderers add their own explanatory
/// wording on top.
impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self {
            Self::Exact => "exact",
            Self::Estimate => "estimate",
            Self::Unknown => "unknown",
        };
        f.write_str(tag)
    }
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
