//! On-disk tuple layout math. Assumes 64-bit PostgreSQL (MAXALIGN = 8, `d` alignment = 8 bytes);
//! a 32-bit knob is out of scope for v1.
//!
//! All row numbers assume every column non-NULL. Varlena payload bytes never count toward sizes
//! (they are unknowable from DDL), but they do move every later column's offset: from the first
//! varlena on, an offset is known only as a set of possible residues mod MAXALIGN, and each
//! later pad is reported as a min/max/expected range over that set.
//!
//! Expected values are display-only figures: the gate and the reorder recommendation act on
//! deterministic pads and dominance instead (see [`crate::dominance`] and the report layer).
//! Two stated modeling assumptions feed every expected value, and only bounds hold without them:
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
            // Short/TOAST forms store unaligned (expected pad 0); only the long form sets the max.
            ColumnKind::Varlena {
                align,
                proven_short: false,
            } => PadRange {
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
/// alignment-descending with typmod-proven-short ones last; stable by original position. The
/// fixed prefix is refined to its exact deterministic minimum within [`refine_fixed_block`]'s
/// caps. This is one candidate pole; the decision policy in the report layer compares it (and
/// the [`search`] poles) against the current order by dominance.
pub fn suggested_order(kinds: &[ColumnKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..kinds.len()).collect();
    order.sort_by_key(|&i| sort_key(&kinds[i], i));
    refine_fixed_block(kinds, &mut order);
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
    /// Even the fixed-prefix search was out of caps; the order is the plain heuristic sort.
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

/// Whole-order state budget: Π(class count + 1) × 15 set states. Past it the exact search
/// degrades to the fixed-prefix search, keeping worst-case wall time at the pre-existing
/// fixed-block level instead of the multi-second corner a 12-class 24-column table reaches.
const WHOLE_ORDER_STATE_BUDGET: usize = 1 << 20;

/// Run the order search: the heuristic pole always, and the two exact lexicographic poles when
/// the whole-order state space fits [`WHOLE_ORDER_STATE_BUDGET`] and the table has at most 24
/// columns. A heuristic order that already achieves zero deterministic and zero worst-case
/// padding is the global minimum of both objectives, so the search is complete without running.
pub fn search(kinds: &[ColumnKind]) -> Search {
    let mut heuristic: Vec<usize> = (0..kinds.len()).collect();
    heuristic.sort_by_key(|&i| sort_key(&kinds[i], i));
    let heuristic_kinds: Vec<ColumnKind> = heuristic.iter().map(|&i| kinds[i]).collect();
    let hw = walk(&heuristic_kinds);
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
    if kinds.len() <= 24 && states <= WHOLE_ORDER_STATE_BUDGET {
        let certainty = lex_min_order(&classes, kinds.len(), LexMode::CertaintyFirst, states);
        let minimax = lex_min_order(&classes, kinds.len(), LexMode::WorstCaseFirst, states);
        // The heuristic pole keeps the fixed-prefix refinement so it stays a usable candidate.
        refine_fixed_block(kinds, &mut heuristic);
        return Search {
            heuristic,
            certainty_pole: Some(certainty),
            minimax_pole: Some(minimax),
            scope: SearchScope::Complete,
        };
    }
    let fixed_len = heuristic.iter().take_while(|&&i| kinds[i].is_fixed()).count();
    let fixed_refinable = fixed_len <= 24 && fixed_class_count(kinds) <= 12;
    refine_fixed_block(kinds, &mut heuristic);
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

fn fixed_class_count(kinds: &[ColumnKind]) -> usize {
    let mut keys: Vec<(u64, u64)> = kinds
        .iter()
        .filter_map(|k| match k {
            ColumnKind::Fixed { len, align } => Some((align.bytes(), len % MAXALIGN)),
            ColumnKind::Varlena { .. } => None,
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys.len()
}

/// Descending-alignment sorting leaves the fixed block zero-padding for most schemas, but with
/// two or more irregulars (timetz, macaddr, …) it can keep padding an interposed smaller column
/// would absorb. When the sorted fixed block still pads, find the exact minimum over the block:
/// deterministic padding depends only on (alignment, len mod MAXALIGN) classes and the running
/// offset residue, so a memoized search over class counts is exhaustive. The varlena tail stays
/// where the sort put it, which makes the refinement dominance-safe: with the tail sequence
/// preserved, reducing the prefix padding reduces the total in every realization (the report
/// layer relies on exactly this). Caps: 3 to 24 fixed columns, 12 fixed classes.
fn refine_fixed_block(kinds: &[ColumnKind], order: &mut [usize]) {
    let fixed_len = order.iter().take_while(|&&i| kinds[i].is_fixed()).count();
    if !(3..=24).contains(&fixed_len) {
        return;
    }
    let sorted_fixed: Vec<ColumnKind> = order[..fixed_len].iter().map(|&i| kinds[i]).collect();
    if walk(&sorted_fixed).padding == 0 {
        return;
    }
    let classes = padding_classes(kinds, &order[..fixed_len]);
    if classes.len() > 12 {
        return;
    }
    let states: usize = classes
        .iter()
        .map(|c| c.members.len() + 1)
        .product::<usize>()
        .saturating_mul(MAXALIGN as usize)
        .min(1 << 17);
    // An all-fixed block walks singleton states only, where deterministic and worst-case pads
    // coincide, so either lexicographic mode reproduces the plain padding minimum.
    let refined = run_dp(&classes, fixed_len, LexMode::CertaintyFirst, states);
    order[..fixed_len].copy_from_slice(&refined);
}

/// Exact lexicographic minimization over whole orders.
fn lex_min_order(classes: &[PaddingClass], total: usize, mode: LexMode, states: usize) -> Vec<usize> {
    run_dp(classes, total, mode, states)
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

/// Pack the lexicographic pair into one additive scalar. Each component is at most
/// 24 columns × 7 bytes = 168, far under the 512 radix.
const LEX_RADIX: u64 = 512;

fn run_dp(classes: &[PaddingClass], total: usize, mode: LexMode, states: usize) -> Vec<usize> {
    let mut dp = Dp {
        classes,
        mode,
        memo: std::collections::HashMap::with_capacity_and_hasher(states.min(1 << 17), PackedKeyHasherBuilder),
    };
    let mut counts: Vec<u8> = classes.iter().map(|c| c.members.len() as u8).collect();
    let mut remaining = total;
    let mut set = Residues::START;
    let mut queues: Vec<std::collections::VecDeque<usize>> =
        classes.iter().map(|c| c.members.iter().copied().collect()).collect();
    let mut refined = Vec::with_capacity(total);
    while remaining > 0 {
        let target = dp.min_lex(&mut counts, remaining, set);
        for class_index in 0..classes.len() {
            if counts[class_index] == 0 {
                continue;
            }
            let key = classes[class_index].key;
            let step = key.lex_cost(set, mode);
            counts[class_index] -= 1;
            let rest = dp.min_lex(&mut counts, remaining - 1, key.next_set(set));
            if step + rest == target {
                refined.push(queues[class_index].pop_front().expect("count tracked"));
                set = key.next_set(set);
                remaining -= 1;
                break;
            }
            counts[class_index] += 1;
        }
    }
    refined
}

/// Group `order` into its padding-equivalence classes, heuristic order preserved (first
/// appearance) so the search's tie-breaking keeps the familiar shape. Varlenas that never pad
/// in any storage form (typmod-proven short, or char-aligned) form one class; the others are
/// classed by alignment, which decides their worst-case long-form pad.
fn padding_classes(kinds: &[ColumnKind], order: &[usize]) -> Vec<PaddingClass> {
    let mut classes: Vec<PaddingClass> = Vec::new();
    for &index in order {
        let key = match kinds[index] {
            ColumnKind::Fixed { len, align } => ClassKey::Fixed {
                align: align.bytes(),
                len_mod: len % MAXALIGN,
            },
            ColumnKind::Varlena { align, proven_short } => {
                if proven_short || align == Align::Char {
                    ClassKey::PadlessVarlena
                } else {
                    ClassKey::Varlena { align: align.bytes() }
                }
            }
        };
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
    /// Proven-short or char-aligned varlena: pads 0 in every storage form.
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

struct Dp<'a> {
    classes: &'a [PaddingClass],
    mode: LexMode,
    memo: std::collections::HashMap<u128, u64, PackedKeyHasherBuilder>,
}

impl Dp<'_> {
    /// `remaining` is the sum of `counts`, carried so the all-placed base case is O(1). The
    /// state packs into one u128: up to 24 classes of 5 bits each plus the residue-set state
    /// (4 bits) — the memo never hashes heap data.
    fn min_lex(&mut self, counts: &mut [u8], remaining: usize, set: Residues) -> u64 {
        if remaining == 0 {
            return 0;
        }
        let key = counts
            .iter()
            .fold(u128::from(set_state_index(set)), |k, &c| (k << 5) | u128::from(c));
        if let Some(&cached) = self.memo.get(&key) {
            return cached;
        }
        let mut best = u64::MAX;
        for class_index in 0..self.classes.len() {
            if counts[class_index] == 0 {
                continue;
            }
            let class = self.classes[class_index].key;
            let step = class.lex_cost(set, self.mode);
            counts[class_index] -= 1;
            let total = step + self.min_lex(counts, remaining - 1, class.next_set(set));
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
        unreachable!("only u128 keys are hashed");
    }

    fn write_u128(&mut self, n: u128) {
        // Fibonacci multiplier per half, then fold: the table indexes by the low hash bits, and
        // a bare multiply leaves keys that differ only in high fields colliding into one bucket.
        let lo = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let hi = ((n >> 64) as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        let h = lo ^ hi.rotate_left(32);
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
