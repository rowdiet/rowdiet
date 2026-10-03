//! On-disk tuple layout math. Assumes 64-bit PostgreSQL (MAXALIGN = 8, `d` alignment = 8 bytes);
//! a 32-bit knob is out of scope for v1.
//!
//! [`walk`] lays out rows that store every column. A NULL stores no bytes and no pad
//! (`heap_fill_tuple` skips it), so a nullable column moves later offsets by its pad and length in
//! the rows that store it and by nothing in the rows that do not; [`Column`] carries that fact to
//! the realization model in [`crate::dominance`], which bounds padding over every NULL pattern.
//! Varlena payload bytes never count toward sizes (they are unknowable from DDL), but they do
//! move every later column's offset: from the first varlena on, an offset is known only as a set
//! of possible residues mod MAXALIGN, and each later pad is reported as a min/max/expected range
//! over that set.
//!
//! Expected values are display-only figures: the gate and the reorder recommendation act on
//! deterministic pads and dominance instead (see [`crate::dominance`] and the report layer).
//! Three stated modeling assumptions feed every expected value, and only bounds hold without them:
//!
//! 1. **Varlena pads are scored at the short-form/TOAST value of zero.** Postgres stores a
//!    varlena payload of 126 bytes or less with a 1-byte header and no alignment
//!    (`heap_compute_data_size` packs it, `att_align_datum` skips alignment), and a toasted
//!    value as an 18-byte unaligned pointer. Only the in-line long form (payloads of roughly
//!    127 bytes up to the TOAST threshold) aligns, so a varlena's own pad is zero in two of the
//!    three storage regimes; the long form contributes only to the pad's `max`.
//! 2. **Offset residues after a varlena are taken as uniformly likely.** Real payload-width
//!    distributions can be skewed mod 8 (fixed-length codes, TOAST pointers pin the residue),
//!    which moves the expectation of later fixed-column pads inside the reported min/max range.
//! 3. **No column holds a NULL.** NULL frequencies are workload facts the DDL does not carry.
//!
//! Pads placed while the offset is exactly known stay exact, so all-fixed tables keep
//! byte-exact numbers.

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
        /// The typmod proves every uncompressed value fits the 1-byte-header short form
        /// (varchar(n)/char(n), n ≤ 31). Stored unaligned unless the toaster compresses it in
        /// line, which [`ColumnKind::always_short`] rules out for the smallest typmods.
        proven_short: bool,
        /// What the type's encoding proves about its uncompressed payload lengths.
        #[cfg_attr(feature = "serde", serde(skip))]
        payload: Payload,
    },
}

/// The uncompressed payload lengths mod 8 a varlena type can store. A short value stores its
/// payload uncompressed, so this narrows the short form; compression and TOAST reach the other
/// residues and pointer shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payload {
    /// Uncompressed payloads are `4 + k * step` bytes mod 8 for every k: 1 for any length, 2 for
    /// numeric's even lengths, and for an array the 2-power part of its element stride (its data
    /// area starts MAXALIGNed after a 4-byte-aligned header).
    pub step: u8,
    /// The residues above are exactly what PostgreSQL stores (checked against its source). False
    /// for types nobody checked: the model then lets them store any length, which may be more
    /// than they can, so an absence claim over them is withheld.
    pub verified: bool,
    /// A value can exceed 20 payload bytes, so a wide row's toaster may compress it in line
    /// behind an aligned 4-byte header (lz4 has no minimum input; the toaster considers any
    /// attribute over 24 bytes, toast_helper.c).
    pub compressible: bool,
}

impl Payload {
    /// Any payload length, verified: text, bytea, jsonb, unlimited varchar.
    pub const ANY: Self = Self {
        step: 1,
        verified: true,
        compressible: true,
    };
    /// Any payload length assumed, not verified against the type's encoding.
    pub const UNVERIFIED: Self = Self {
        step: 1,
        verified: false,
        compressible: true,
    };
    /// numeric: a 2- or 4-byte header plus 2-byte digits, so every length is even.
    pub const EVEN: Self = Self {
        step: 2,
        verified: true,
        compressible: true,
    };

    /// An array's payload: its elements each take a multiple of their stride.
    pub fn array(stride: u64) -> Self {
        Self {
            step: gcd(stride, MAXALIGN) as u8,
            verified: true,
            compressible: true,
        }
    }

    /// The storable residues as a bitmask over 0..=7.
    pub fn residues(self) -> u8 {
        (0..MAXALIGN as u8)
            .filter(|r| (r + MAXALIGN as u8 - 4).is_multiple_of(self.step))
            .fold(0, |mask, r| mask | (1 << r))
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
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

    /// A varlena stored unaligned in every form: proven short and too small to compress.
    pub fn always_short(&self) -> bool {
        matches!(self, Self::Varlena { proven_short: true, payload, .. } if !payload.compressible)
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

/// The payload residue whose short form advances a multiple of 8 bytes (a 1-byte header plus 7):
/// the one non-NULL step that leaves the offset where a NULL leaves it.
pub(crate) const NULL_LIKE_RESIDUE: u8 = 1 << 7;

/// One column as the realization model sees it: its storage class and whether its rows may hold
/// NULL. A NULL stores nothing, so a nullable column's NULL is a realization of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    /// Storage class.
    pub kind: ColumnKind,
    /// Rows may store NULL here: such a row stores neither the value nor its pad.
    pub nullable: bool,
}

impl Column {
    /// A `NOT NULL` column: stored in every row.
    pub fn not_null(kind: ColumnKind) -> Self {
        Self { kind, nullable: false }
    }

    /// A nullable column.
    pub fn nullable(kind: ColumnKind) -> Self {
        Self { kind, nullable: true }
    }

    /// True when a NULL here is a step no stored value takes, so it moves later offsets in a way
    /// the column's values cannot: every nullable fixed column, and a nullable varlena whose short
    /// payloads never advance a multiple of 8 (numeric, arrays of wider elements). A nullable
    /// text's NULL advances like its 7-byte payloads.
    pub fn null_varies(&self) -> bool {
        self.nullable
            && match self.kind {
                ColumnKind::Fixed { .. } => true,
                ColumnKind::Varlena { payload, .. } => payload.residues() & NULL_LIKE_RESIDUE == 0,
            }
    }
}

impl From<ColumnKind> for Column {
    fn from(kind: ColumnKind) -> Self {
        Self::not_null(kind)
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

/// One pad's bounds and expectation. When the pad's value is certain, `min == max` and the pad
/// is that one value; otherwise the bounds range over the possible offset residues mod MAXALIGN
/// and, for a varlena, over its storage forms. `min` and `max` are jointly achievable across a
/// whole walk: any residue stays reachable after any earlier extreme, so the per-column extremes
/// compose (pinned by a test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadRange {
    /// Smallest possible pad, bytes.
    pub min: u64,
    /// Largest possible pad, bytes.
    pub max: u64,
    /// Expected pad under the module doc's stated assumptions (short/TOAST varlena forms,
    /// uniform residues), in eighths of a byte — eighths keep it exact, since reachable residue
    /// sets have 1, 2, 4, or 8 members, all dividing 8.
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
        debug_assert!(self.0 != 0, "empty residue set");
        u64::from(self.0.count_ones())
    }

    /// Pad statistics for aligning to `align` from any residue in the set. Singleton sets (the
    /// whole walk of an all-fixed table, and most search states) skip the residue loop.
    fn pad_to(self, align: u64) -> PadRange {
        if self.0.count_ones() == 1 {
            return PadRange::certain(pad_pow2(u64::from(self.0.trailing_zeros()), align));
        }
        let mut min = u64::MAX;
        let mut max = 0u64;
        let mut sum = 0u64;
        for residue in 0..MAXALIGN {
            if self.contains(residue) {
                let p = pad_pow2(residue, align);
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
        if self.0.count_ones() == 1 {
            let residue = u64::from(self.0.trailing_zeros());
            return Self(1u8 << ((residue + pad_pow2(residue, align)) % MAXALIGN));
        }
        let mut mask = 0u8;
        for residue in 0..MAXALIGN {
            if self.contains(residue) {
                mask |= 1u8 << ((residue + pad_pow2(residue, align)) % MAXALIGN);
            }
        }
        Self(mask)
    }

    /// The set after advancing by `len` bytes.
    fn shifted(self, len: u64) -> Self {
        Self(self.0.rotate_left((len % MAXALIGN) as u32))
    }

    /// The largest pad for aligning to `align` from any residue in the set, and whether every
    /// residue pads the same; valid for every set, cosets or not.
    fn pad_spread(self, align: u64) -> (u64, bool) {
        let mut pads = (0..MAXALIGN).filter(|&r| self.contains(r)).map(|r| pad_pow2(r, align));
        let first = pads.next().expect("non-empty residue set");
        pads.fold((first, true), |(max, same), p| (max.max(p), same && p == first))
    }
}

/// One column's placement in a [`Walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnWalk {
    /// Padding inserted immediately before this column.
    pub pad_before: PadRange,
    /// The column's data start, bytes from the beginning of the data area (t_hoff not
    /// included), claimed only while it is certain: the first varlena's payload moves every
    /// later offset, and a varlena whose own pad depends on its storage form has none either.
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

/// Where an order starts: the offset residues a committed prefix can end at, in rows that store
/// every column and over every realization. A whole table starts at offset 0; an appended block
/// starts wherever the columns before it can leave the offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    stored: Residues,
    any: Residues,
}

impl Start {
    /// A whole table: offset 0 exactly.
    pub const TABLE: Self = Self {
        stored: Residues::START,
        any: Residues::START,
    };

    /// The residues `prefix` can end at, storage forms, payload lengths, and NULLs included.
    pub fn after(prefix: &[Column]) -> Self {
        let mut start = Self::TABLE;
        for column in prefix {
            match column.kind {
                ColumnKind::Fixed { len, align } => {
                    let stored = start.any.aligned(align.bytes()).shifted(len);
                    start = Self {
                        stored: start.stored.aligned(align.bytes()).shifted(len),
                        any: if column.nullable {
                            Residues(start.any.0 | stored.0)
                        } else {
                            stored
                        },
                    };
                }
                ColumnKind::Varlena { .. } => {
                    start = Self {
                        stored: Residues::FULL,
                        any: Residues::FULL,
                    }
                }
            }
        }
        start
    }

    /// The reachable residues as a bitmask over 0..=7: over every realization when `nulls`, else
    /// in rows that store every column.
    pub fn residues(self, nulls: bool) -> u8 {
        if nulls { self.any.0 } else { self.stored.0 }
    }
}

/// Place `kinds` in the given order and total the padding (every column non-NULL; pads placed
/// after the first varlena are min/max/expected ranges over the possible offset residues).
pub fn walk(kinds: &[ColumnKind]) -> Walk {
    walk_at(Start::TABLE, Some(0), kinds)
}

/// [`walk`] for an order that starts after a committed prefix, in rows that store every column.
/// The absolute start is unknown, so no offset or end is claimed.
pub fn walk_from(start: Start, kinds: &[ColumnKind]) -> Walk {
    walk_at(start, None, kinds)
}

fn walk_at(start: Start, offset: Option<u64>, kinds: &[ColumnKind]) -> Walk {
    let mut residues = start.stored;
    let mut end = offset;
    let mut padding = 0u64;
    let mut uncertain_min = 0u64;
    let mut uncertain_max = 0u64;
    let mut uncertain_expected_eighths = 0u64;
    let mut columns = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let pad_before = match kind {
            ColumnKind::Fixed { align, .. } => residues.pad_to(align.bytes()),
            // A short varlena (1-byte header) is stored with no alignment at all (tupmacs.h).
            ColumnKind::Varlena { .. } if kind.always_short() => PadRange::certain(0),
            // Short/TOAST forms store unaligned (expected pad 0); only the long form sets the max.
            ColumnKind::Varlena { align, .. } => PadRange {
                min: 0,
                max: residues.pad_to(align.bytes()).max,
                expected_eighths: 0,
            },
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
            // An offset is claimed only while both the running end and this pad are certain.
            offset: end.and_then(|e| pad_before.exact().map(|p| e + p)),
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

/// Padding certain in every row that stores its column, over every NULL pattern: the pads whose
/// value no payload length, storage form, or earlier NULL can change. Equals [`Walk::padding`]
/// when no fixed column is nullable; a nullable fixed column widens the residues later columns
/// can start at, so a pad behind one stays certain only if an alignment restores it.
pub fn certain_padding(start: Start, columns: &[Column], order: &[usize]) -> u64 {
    let mut any = start.any;
    let mut total = 0;
    for &index in order {
        let column = columns[index];
        match column.kind {
            ColumnKind::Fixed { len, align } => {
                let (max, same) = any.pad_spread(align.bytes());
                if same {
                    total += max;
                }
                let stored = any.aligned(align.bytes()).shifted(len);
                any = if column.nullable {
                    Residues(any.0 | stored.0)
                } else {
                    stored
                };
            }
            // A varlena's short form pads 0, so its own pad is never certain and nonzero.
            ColumnKind::Varlena { .. } => any = Residues::FULL,
        }
    }
    total
}

/// Exact smallest and largest data end over the rows of an all-fixed order that hold at least
/// one NULL, or None when no column is nullable or a varlena makes the end unknowable. Feeds the
/// footprint of NULL-carrying rows, which also pay the null bitmap in their header.
pub fn null_row_ends(columns: &[Column]) -> Option<(u64, u64)> {
    if !columns.iter().any(|c| c.nullable) || !columns.iter().all(|c| c.kind.is_fixed()) {
        return None;
    }
    // State: (offset residue, some NULL seen) -> (smallest end, largest end).
    let mut states: [Option<(u64, u64)>; 16] = [None; 16];
    states[0] = Some((0, 0));
    for column in columns {
        let ColumnKind::Fixed { len, align } = column.kind else {
            unreachable!("checked all-fixed above")
        };
        let mut next: [Option<(u64, u64)>; 16] = [None; 16];
        let mut merge = |slot: usize, lo: u64, hi: u64| match &mut next[slot] {
            Some((a, b)) => {
                *a = (*a).min(lo);
                *b = (*b).max(hi);
            }
            none @ None => *none = Some((lo, hi)),
        };
        for (state, bounds) in states.iter().enumerate() {
            let Some((lo, hi)) = *bounds else { continue };
            let (residue, seen) = ((state % 8) as u64, state / 8);
            let p = pad_pow2(residue, align.bytes());
            merge(
                seen * 8 + ((residue + p + len) % MAXALIGN) as usize,
                lo + p + len,
                hi + p + len,
            );
            if column.nullable {
                merge(8 + residue as usize, lo, hi);
            }
        }
        states = next;
    }
    states[8..]
        .iter()
        .flatten()
        .copied()
        .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
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

/// Suggested column order under the fixed-first heuristic: fixed before varlena; alignment
/// descending; within a fixed alignment group regular sizes before irregulars; varlenas
/// alignment-descending with always-short ones last; stable by original position. The
/// fixed prefix is refined to its exact deterministic minimum within [`refine_fixed_block`]'s
/// caps. This is one candidate pole; the decision policy in the report layer compares it (and
/// the [`search`] poles) against the current order by dominance.
pub fn suggested_order(kinds: &[ColumnKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..kinds.len()).collect();
    order.sort_by_key(|&i| sort_key(&kinds[i], i));
    refine_fixed_block(kinds, &mut order, Start::TABLE);
    order
}

/// How much of the order space the search proved. Anything short of [`Complete`](Self::Complete)
/// must be labeled in the output: a capped search claims nothing beyond what it walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum SearchScope {
    /// The whole-order search ran to completion (or the heuristic already proves the global
    /// minimum): the emitted poles are exact lexicographic minima.
    Complete,
    /// The whole-order search was over budget; the fixed prefix was still searched exactly and
    /// varlena placement follows the heuristic.
    FixedPrefix,
    /// Even the fixed-prefix search was out of caps; the fixed block takes the better of the
    /// heuristic sort and a greedy residue packing.
    SortOnly,
}

/// The candidate orders the decision policy evaluates, plus how much was proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// Fixed-first heuristic, fixed prefix refined within caps. Always present.
    pub heuristic: Vec<usize>,
    /// Lexicographic minimum of (deterministic padding, worst-case padding): the certainty
    /// pole, free to place fixed columns behind varlenas. None when the search was capped.
    pub certainty_pole: Option<Vec<usize>>,
    /// Lexicographic minimum of (worst-case padding, deterministic padding): the minimax pole.
    /// None when the search was capped.
    pub minimax_pole: Option<Vec<usize>>,
    /// What the emitted poles prove.
    pub scope: SearchScope,
}

/// Whole-order state budget: Π(class count + 1) × 15 set states, which is also the exact size
/// of the dense memo the search allocates (8 bytes per state). Past it the exact search
/// degrades to the fixed-prefix search. The budget is the only cost cap: a column-count cap
/// would be redundant with it and was measured to cost a real finding at exactly 25 columns.
const WHOLE_ORDER_STATE_BUDGET: usize = 1 << 20;

/// Run the order search: the heuristic pole always, and the two exact lexicographic poles when
/// the whole-order state space fits [`WHOLE_ORDER_STATE_BUDGET`]. A heuristic order that already
/// achieves zero deterministic and zero worst-case padding is the global minimum of both
/// objectives, so the search is complete without running.
pub fn search(kinds: &[ColumnKind]) -> Search {
    search_from(Start::TABLE, kinds)
}

/// [`search`] for an order that starts after a committed prefix, over the rows that store every
/// column.
pub fn search_from(start: Start, kinds: &[ColumnKind]) -> Search {
    let mut heuristic: Vec<usize> = (0..kinds.len()).collect();
    heuristic.sort_by_key(|&i| sort_key(&kinds[i], i));
    let heuristic_kinds: Vec<ColumnKind> = heuristic.iter().map(|&i| kinds[i]).collect();
    let hw = walk_from(start, &heuristic_kinds);
    if hw.padding == 0 && hw.padding_max() == 0 {
        return Search {
            certainty_pole: Some(heuristic.clone()),
            minimax_pole: Some(heuristic.clone()),
            heuristic,
            scope: SearchScope::Complete,
        };
    }
    let classes = padding_classes(kinds, &heuristic);
    let states: usize = classes
        .iter()
        .map(|c| c.members.len() + 1)
        .try_fold(SET_STATES, usize::checked_mul)
        .unwrap_or(usize::MAX);
    if states <= WHOLE_ORDER_STATE_BUDGET {
        let certainty = run_dp(&classes, kinds.len(), LexMode::CertaintyFirst, start.stored);
        let minimax = run_dp(&classes, kinds.len(), LexMode::WorstCaseFirst, start.stored);
        // The heuristic pole keeps the fixed-prefix refinement so it stays a usable candidate.
        refine_fixed_block(kinds, &mut heuristic, start);
        return Search {
            heuristic,
            certainty_pole: Some(certainty),
            minimax_pole: Some(minimax),
            scope: SearchScope::Complete,
        };
    }
    let fixed_len = heuristic.iter().take_while(|&&i| kinds[i].is_fixed()).count();
    let fixed_refinable = fixed_block_fits(kinds, &heuristic[..fixed_len], start);
    refine_fixed_block(kinds, &mut heuristic, start);
    let scope = if fixed_refinable && fixed_len == kinds.len() {
        // All-fixed: the fixed prefix is the whole order, so the block search is complete.
        SearchScope::Complete
    } else if fixed_refinable {
        SearchScope::FixedPrefix
    } else {
        SearchScope::SortOnly
    };
    Search {
        certainty_pole: (scope == SearchScope::Complete).then(|| heuristic.clone()),
        minimax_pole: (scope == SearchScope::Complete).then(|| heuristic.clone()),
        heuristic,
        scope,
    }
}

/// Memo bound of the fixed-block search: Π(class count + 1) × the residue states it can reach (8
/// singletons from an exact start, 15 cosets after a prefix that ends anywhere), at most 8M
/// states (64 MB of memo). The bound caps cost at any column count.
const FIXED_BLOCK_STATE_BUDGET: usize = 1 << 23;

/// Whether [`refine_fixed_block`] searches `block` (a fixed run) exactly.
fn fixed_block_fits(kinds: &[ColumnKind], block: &[usize], start: Start) -> bool {
    padding_classes(kinds, block)
        .iter()
        .map(|c| c.members.len() + 1)
        .try_fold(live_states(true, start.stored), usize::checked_mul)
        .is_some_and(|states| states <= FIXED_BLOCK_STATE_BUDGET)
}

/// The residue-set states a search can reach: singletons only for a fixed block from an exact
/// start, every coset otherwise.
fn live_states(fixed_only: bool, start: Residues) -> usize {
    if fixed_only && start.0.count_ones() == 1 {
        SINGLETON_STATES
    } else {
        SET_STATES
    }
}

/// A dominance-complete candidate space over `columns`, or None when it exceeds `cap`.
///
/// `NOT NULL` fixed columns of one padding class are pointwise interchangeable: they carry no
/// realization variable, and their pads depend only on (alignment, len mod 8) and the offset
/// residue, so swapping two of them changes no realization's padding or row size and one
/// representative arrangement (original relative order) stands for all. Columns that carry a
/// realization variable get no such collapse: a realization assigns each varlena its own payload
/// and each nullable column its own NULL, so swapping two same-class ones permutes that
/// assignment and changes padding pointwise (measured: in (t1, m1, t2, m2) the order
/// (m1, t2, t1, m2) dominates while its class-sequence twin (m1, t1, t2, m2) can be 4 B/row
/// worse). Every varlena and every nullable fixed column is therefore its own singleton class
/// here, which makes the space pointwise-complete: if any reorder dominates a given order, some
/// member of this space attains identical padding in every realization.
pub fn order_space(columns: &[Column], cap: usize) -> Option<Vec<Vec<usize>>> {
    let mut classes: Vec<PaddingClass> = Vec::new();
    let mut collapsed: Vec<(ClassKey, usize)> = Vec::new();
    for (index, &column) in columns.iter().enumerate() {
        let key = class_key(column.kind);
        // One class per varlena and per nullable fixed column: the realization variable each
        // carries forbids the collapse.
        let shared = column.kind.is_fixed() && !column.nullable;
        match collapsed.iter().find(|(k, _)| shared && *k == key) {
            Some(&(_, class)) => classes[class].members.push(index),
            None => {
                if shared {
                    collapsed.push((key, classes.len()));
                }
                classes.push(PaddingClass {
                    key,
                    members: vec![index],
                });
            }
        }
    }
    generate_space(&classes, columns.len(), cap)
}

/// The members of [`order_space`] that keep [`keeps_class_order`]: same-class varlenas and
/// same-class nullable fixed columns in written order, generated over padding classes. This is
/// the candidate space an earlier, collapsed sweep tested, in the order it tested them. None
/// when it exceeds `cap`.
pub fn class_sequence_space(columns: &[Column], cap: usize) -> Option<Vec<Vec<usize>>> {
    let mut classes: Vec<(SequenceKey, PaddingClass)> = Vec::new();
    for (index, &column) in columns.iter().enumerate() {
        let key = sequence_key(column);
        match classes.iter_mut().find(|(k, _)| *k == key) {
            Some((_, class)) => class.members.push(index),
            None => classes.push((
                key,
                PaddingClass {
                    key: key.class,
                    members: vec![index],
                },
            )),
        }
    }
    let classes: Vec<PaddingClass> = classes.into_iter().map(|(_, class)| class).collect();
    generate_space(&classes, columns.len(), cap)
}

/// True when `order` keeps same-class varlenas and same-class nullable fixed columns in written
/// order: the members [`class_sequence_space`] generates.
pub fn keeps_class_order(columns: &[Column], order: &[usize]) -> bool {
    let mut last_seen: Vec<(SequenceKey, usize)> = Vec::new();
    for &column in order {
        let key = sequence_key(columns[column]);
        if !key.variable {
            continue;
        }
        match last_seen.iter_mut().find(|(k, _)| *k == key) {
            Some((_, last)) if *last > column => return false,
            Some((_, last)) => *last = column,
            None => last_seen.push((key, column)),
        }
    }
    true
}

/// A column's class in [`class_sequence_space`]: its padding class, split by whether it carries
/// a realization variable (a varlena, or a nullable fixed column).
#[derive(Clone, Copy, PartialEq, Eq)]
struct SequenceKey {
    class: ClassKey,
    variable: bool,
}

fn sequence_key(column: Column) -> SequenceKey {
    SequenceKey {
        class: class_key(column.kind),
        variable: !column.kind.is_fixed() || column.nullable,
    }
}

/// Every interleaving of `classes` that keeps each class's members in order, or None past `cap`.
fn generate_space(classes: &[PaddingClass], total: usize, cap: usize) -> Option<Vec<Vec<usize>>> {
    let mut sequences: usize = 1;
    let mut remaining = total;
    for class in classes {
        sequences = sequences.checked_mul(binomial(remaining, class.members.len(), cap)?)?;
        if sequences > cap {
            return None;
        }
        remaining -= class.members.len();
    }
    let mut queues: Vec<std::collections::VecDeque<usize>> =
        classes.iter().map(|c| c.members.iter().copied().collect()).collect();
    let mut counts: Vec<usize> = classes.iter().map(|c| c.members.len()).collect();
    let mut out = Vec::with_capacity(sequences);
    let mut current = Vec::with_capacity(total);
    generate_orders(&mut queues, &mut counts, total, &mut current, &mut out);
    Some(out)
}

/// C(n, k), or None past `cap` (the caller cannot use a larger space anyway).
fn binomial(n: usize, k: usize, cap: usize) -> Option<usize> {
    let mut result: usize = 1;
    for i in 0..k.min(n - k) {
        result = result.checked_mul(n - i)? / (i + 1);
        if result > cap.saturating_mul(1 << 10) {
            return None;
        }
    }
    Some(result)
}

fn generate_orders(
    queues: &mut [std::collections::VecDeque<usize>],
    counts: &mut [usize],
    remaining: usize,
    current: &mut Vec<usize>,
    out: &mut Vec<Vec<usize>>,
) {
    if remaining == 0 {
        out.push(current.clone());
        return;
    }
    for class_index in 0..counts.len() {
        if counts[class_index] == 0 {
            continue;
        }
        counts[class_index] -= 1;
        let column = queues[class_index].pop_front().expect("count tracked");
        current.push(column);
        generate_orders(queues, counts, remaining - 1, current, out);
        current.pop();
        queues[class_index].push_front(column);
        counts[class_index] += 1;
    }
}

/// Repack the leading fixed run of `order` to its deterministic minimum (past the search budget,
/// to the better of the sort and a greedy packing when that pads less), leaving everything from
/// the first varlena on
/// untouched. With the suffix preserved, any prefix improvement dominates the original order
/// (the never-negative-recovery induction), which makes this the decision policy's always-safe
/// repair candidate at any width.
pub fn refine_leading_fixed(kinds: &[ColumnKind], order: &mut [usize]) {
    refine_fixed_block(kinds, order, Start::TABLE);
}

/// [`refine_leading_fixed`] for an order that starts after a committed prefix.
pub fn refine_leading_fixed_from(start: Start, kinds: &[ColumnKind], order: &mut [usize]) {
    refine_fixed_block(kinds, order, start);
}

/// Descending-alignment sorting leaves the fixed block zero-padding for most schemas, but with
/// two or more irregulars (timetz, macaddr, …) it can keep padding an interposed smaller column
/// would absorb. When the fixed block pads, find the exact minimum over the block: deterministic
/// padding depends only on (alignment, len mod MAXALIGN) classes and the running offset residue,
/// so a memoized search over class counts is exhaustive. Past [`FIXED_BLOCK_STATE_BUDGET`] the
/// block takes the better of the heuristic sort and [`greedy_pack`], when that pads less. The
/// varlena tail stays where it
/// was, which makes the refinement dominance-safe: with the tail sequence preserved, reducing the
/// prefix padding reduces the total in every realization (the report layer relies on exactly
/// this).
fn refine_fixed_block(kinds: &[ColumnKind], order: &mut [usize], start: Start) {
    let fixed_len = order.iter().take_while(|&&i| kinds[i].is_fixed()).count();
    if fixed_len < 3 {
        return;
    }
    let block_padding = |block: &[usize]| {
        let w = walk_from(start, &block.iter().map(|&i| kinds[i]).collect::<Vec<_>>());
        (w.padding, w.padding_max())
    };
    let current = block_padding(&order[..fixed_len]);
    if current == (0, 0) {
        return;
    }
    if fixed_block_fits(kinds, &order[..fixed_len], start) {
        let classes = padding_classes(kinds, &order[..fixed_len]);
        // From an exact start an all-fixed block walks singleton states only, where deterministic
        // and worst-case pads coincide, so either lexicographic mode reproduces the plain padding
        // minimum.
        let refined = run_dp(&classes, fixed_len, LexMode::CertaintyFirst, start.stored);
        order[..fixed_len].copy_from_slice(&refined);
        return;
    }
    let mut sorted = order[..fixed_len].to_vec();
    sorted.sort_by_key(|&i| sort_key(&kinds[i], i));
    let greedy = greedy_pack(kinds, &sorted, start.stored);
    let best = [sorted, greedy]
        .into_iter()
        .min_by_key(|candidate| block_padding(candidate))
        .expect("two candidates");
    if block_padding(&best) < current {
        order[..fixed_len].copy_from_slice(&best);
    }
}

/// A fixed block packed column by column: each step takes the class that pads least from the
/// current offset, the earliest in `sorted` on ties. It interleaves irregulars with the columns
/// that absorb them (timetz with int4, macaddr with int2), which the sort keeps apart.
fn greedy_pack(kinds: &[ColumnKind], sorted: &[usize], start: Residues) -> Vec<usize> {
    let mut queues: Vec<(ClassKey, std::collections::VecDeque<usize>)> = padding_classes(kinds, sorted)
        .into_iter()
        .map(|class| (class.key, class.members.into_iter().collect()))
        .collect();
    // A block behind a prefix starts where the prefix ends when that is one residue.
    let mut residue = if start.0.count_ones() == 1 {
        u64::from(start.0.trailing_zeros())
    } else {
        0
    };
    let mut packed = Vec::with_capacity(sorted.len());
    while let Some((key, queue)) = queues
        .iter_mut()
        .filter(|(_, queue)| !queue.is_empty())
        .min_by_key(|(key, _)| match key {
            ClassKey::Fixed { align, .. } => pad_pow2(residue, *align),
            ClassKey::Varlena { .. } | ClassKey::PadlessVarlena => u64::MAX,
        })
    {
        let ClassKey::Fixed { align, len_mod } = *key else {
            unreachable!("a fixed block holds fixed columns")
        };
        packed.push(queue.pop_front().expect("non-empty"));
        residue = (residue + pad_pow2(residue, align) + len_mod) % MAXALIGN;
    }
    packed
}

/// The two lexicographic objectives the policy needs. Both components are additive per
/// (class, residue-set state), and adding a common prefix cost preserves lexicographic order,
/// so Bellman optimality holds for the packed scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexMode {
    /// (deterministic padding, worst-case padding): the certainty pole.
    CertaintyFirst,
    /// (worst-case padding, deterministic padding): the minimax pole.
    WorstCaseFirst,
}

/// Pack the lexicographic pair into one additive scalar. Each component sums at most 7 bytes
/// per column, so the radix stays above it for any table under 600 million columns.
const LEX_RADIX: u64 = 1 << 32;

fn run_dp(classes: &[PaddingClass], total: usize, mode: LexMode, start: Residues) -> Vec<usize> {
    debug_assert_eq!(total, classes.iter().map(|c| c.members.len()).sum::<usize>());
    // Mixed-radix strides over the class counts: every (counts, set state) combination maps to
    // one dense memo slot, so the memo is a flat Vec of exactly the state-space bound (8 bytes
    // per state) with no hashing and no rehash growth.
    let live_states = live_states(classes.iter().all(|c| matches!(c.key, ClassKey::Fixed { .. })), start);
    let mut strides = Vec::with_capacity(classes.len());
    let mut bound = live_states;
    for class in classes {
        strides.push(bound);
        bound *= class.members.len() + 1;
    }
    let steps: Vec<[(u64, usize); SET_STATES]> = classes
        .iter()
        .map(|class| {
            std::array::from_fn(|state| {
                let set = SET_BY_INDEX[state];
                (
                    class.key.lex_cost(set, mode),
                    set_state_index(class.key.next_set(set)) as usize,
                )
            })
        })
        .collect();
    // memo[slot] is the cheapest packed cost of placing the counts the slot encodes from its set
    // state. Placing a column lowers the slot, so one ascending pass fills the memo without
    // recursion.
    let mut memo = vec![0u64; bound];
    let mut counts = vec![0usize; classes.len()];
    for base in (live_states..bound).step_by(live_states) {
        for (count, class) in counts.iter_mut().zip(classes) {
            if *count < class.members.len() {
                *count += 1;
                break;
            }
            *count = 0;
        }
        for state in 0..live_states {
            let mut best = u64::MAX;
            for (class_index, &count) in counts.iter().enumerate() {
                if count > 0 {
                    let (cost, next) = steps[class_index][state];
                    best = best.min(cost + memo[base - strides[class_index] + next]);
                }
            }
            memo[base + state] = best;
        }
    }
    let mut counts: Vec<usize> = classes.iter().map(|c| c.members.len()).collect();
    let mut base = bound - live_states;
    let mut state = set_state_index(start) as usize;
    let mut queues: Vec<std::collections::VecDeque<usize>> =
        classes.iter().map(|c| c.members.iter().copied().collect()).collect();
    let mut refined = Vec::with_capacity(total);
    // Exactly one column per round: some class attains the optimum the memo recorded.
    for _ in 0..total {
        let target = memo[base + state];
        let class_index = (0..classes.len())
            .find(|&c| {
                let (cost, next) = steps[c][state];
                counts[c] > 0 && cost + memo[base - strides[c] + next] == target
            })
            .expect("the recorded optimum is attained by some class");
        refined.push(queues[class_index].pop_front().expect("count tracked"));
        counts[class_index] -= 1;
        base -= strides[class_index];
        state = steps[class_index][state].1;
    }
    refined
}

/// Group `order` into its padding-equivalence classes, heuristic order preserved (first
/// appearance) so the search's tie-breaking keeps the familiar shape. Varlenas that never pad
/// in any storage form (always short, or char-aligned) form one class; the others are
/// classed by alignment, which decides their worst-case long-form pad.
fn padding_classes(kinds: &[ColumnKind], order: &[usize]) -> Vec<PaddingClass> {
    let mut classes: Vec<PaddingClass> = Vec::new();
    for &index in order {
        let key = class_key(kinds[index]);
        match classes.iter_mut().find(|c| c.key == key) {
            Some(class) => class.members.push(index),
            None => classes.push(PaddingClass {
                key,
                members: vec![index],
            }),
        }
    }
    classes
}

/// A column's padding class: fixed columns by (alignment, len mod 8); varlenas that never pad in
/// any storage form (always short, or char-aligned) share one class; the others class by
/// alignment, which decides their worst-case long-form pad.
fn class_key(kind: ColumnKind) -> ClassKey {
    match kind {
        ColumnKind::Fixed { len, align } => ClassKey::Fixed {
            align: align.bytes(),
            len_mod: len % MAXALIGN,
        },
        ColumnKind::Varlena { align, .. } if kind.always_short() || align == Align::Char => ClassKey::PadlessVarlena,
        ColumnKind::Varlena { align, .. } => ClassKey::Varlena { align: align.bytes() },
    }
}

struct PaddingClass {
    key: ClassKey,
    members: Vec<usize>,
}

/// What the walk sees of a column class: its cost from a residue-set state and the state it
/// leaves behind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClassKey {
    Fixed {
        align: u64,
        len_mod: u64,
    },
    /// Long-form-capable varlena: pads up to `align - 1` in the long form, 0 otherwise.
    Varlena {
        align: u64,
    },
    /// Always-short or char-aligned varlena: pads 0 in every storage form.
    PadlessVarlena,
}

impl ClassKey {
    /// (deterministic, worst-case) pad in bytes from `set`, packed per `mode`.
    fn lex_cost(self, set: Residues, mode: LexMode) -> u64 {
        let (det, max) = match self {
            Self::Fixed { align, .. } => {
                let range = set.pad_to(align);
                (range.exact().unwrap_or(0), range.max)
            }
            // The varlena's own pad is certain only when even the long form pads zero.
            Self::Varlena { align } => (0, set.pad_to(align).max),
            Self::PadlessVarlena => (0, 0),
        };
        match mode {
            LexMode::CertaintyFirst => det * LEX_RADIX + max,
            LexMode::WorstCaseFirst => max * LEX_RADIX + det,
        }
    }

    fn next_set(self, set: Residues) -> Residues {
        match self {
            Self::Fixed { align, len_mod } => set.aligned(align).shifted(len_mod),
            Self::Varlena { .. } | Self::PadlessVarlena => Residues::FULL,
        }
    }
}

/// Count of reachable residue-set states: the 15 cosets in Z/8 (8 singletons, 4 pairs, the even
/// and odd sets, and the full set).
const SET_STATES: usize = 15;

/// The singletons come first in [`set_state_index`] order.
const SINGLETON_STATES: usize = 8;

/// Each set state by its [`set_state_index`].
const SET_BY_INDEX: [Residues; SET_STATES] = [
    Residues(0b0000_0001),
    Residues(0b0000_0010),
    Residues(0b0000_0100),
    Residues(0b0000_1000),
    Residues(0b0001_0000),
    Residues(0b0010_0000),
    Residues(0b0100_0000),
    Residues(0b1000_0000),
    Residues(0b0001_0001),
    Residues(0b0010_0010),
    Residues(0b0100_0100),
    Residues(0b1000_1000),
    Residues(0b0101_0101),
    Residues(0b1010_1010),
    Residues(0xFF),
];

/// Dense index of a reachable set state, for the DP key: cosets only, 0..15.
fn set_state_index(set: Residues) -> u64 {
    let mask = set.0;
    let tz = u64::from(mask.trailing_zeros());
    match mask.count_ones() {
        1 => tz,
        2 => {
            debug_assert!(tz < 4 && mask == (1 << tz) | (1 << (tz + 4)), "not a coset of 4Z");
            8 + tz
        }
        4 => {
            debug_assert!(tz < 2 && mask == 0b0101_0101 << tz, "not a coset of 2Z");
            12 + tz
        }
        _ => {
            debug_assert_eq!(mask, 0xFF, "not a coset");
            14
        }
    }
}

/// [`pad`] for the walk's and the search's hot loops: alignments are powers of two (1/2/4/8),
/// so the modulo pair reduces to a mask — the div unit is measurable at the memo's node volume.
fn pad_pow2(offset: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    align.wrapping_sub(offset) & (align - 1)
}

fn sort_key(kind: &ColumnKind, index: usize) -> (u8, u64, bool, usize) {
    let align_desc = |a: Align| MAXALIGN - a.bytes();
    match kind {
        ColumnKind::Fixed { align, .. } => (0, align_desc(*align), kind.irregular(), index),
        ColumnKind::Varlena { align, .. } if kind.always_short() => (2, align_desc(*align), false, index),
        ColumnKind::Varlena { align, .. } => (1, align_desc(*align), false, index),
    }
}

/// How solid the reported numbers are. `Exact`: only fixed-width columns — padding and footprint
/// are byte-exact and order-guaranteed. `Estimate`: at least one varlena — the min/max range
/// bounds every storage form, expected values are display-only model figures, and gating rests
/// on deterministic and dominance-proven padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(rename_all = "snake_case"))]
pub enum Tier {
    /// Only fixed-width columns: byte-exact, order-guaranteed.
    Exact,
    /// At least one varlena: bounds hold for every storage form, expected values are
    /// display-only figures under the module doc's stated assumptions, and the gate uses
    /// deterministic and dominance-proven padding only.
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
