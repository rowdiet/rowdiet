use super::*;
use crate::layout::Align;

fn fixed(len: u64, align: Align) -> ColumnKind {
    ColumnKind::Fixed { len, align }
}

fn varlena(align: Align) -> ColumnKind {
    ColumnKind::Varlena {
        align,
        proven_short: false,
    }
}

fn short() -> ColumnKind {
    ColumnKind::Varlena {
        align: Align::Int,
        proven_short: true,
    }
}

#[test]
fn aligned_slot_plus_char_tail_dominates_everything() {
    // (bigint, text, boolean): the text sits on a 4-boundary in every storage form and the
    // boolean never aligns, so total padding is zero in every realization.
    let kinds = [varlena(Align::Int), fixed(1, Align::Char), fixed(8, Align::Double)];
    let current = [0usize, 1, 2];
    let suggested = [2usize, 0, 1];
    let diff = compare(&kinds, &current, &suggested).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    assert_eq!(diff.min, 0, "some realization pads the current order zero too");
    assert_eq!(diff.max, 7, "worst case: the bigint pads 7 behind the text");
}

#[test]
fn stranded_int_behind_text_is_dominated_by_fixed_first() {
    let kinds = [varlena(Align::Int), fixed(4, Align::Int)];
    let diff = compare(&kinds, &[0, 1], &[1, 0]).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    assert_eq!((diff.min, diff.max), (0, 3));
}

#[test]
fn d_aligned_array_before_int_is_not_dominated() {
    // (float8[], int4) measures flat zero on disk for whole-float8 payloads; the swapped order
    // pays the array's long-form pad. Neither dominates: the swap wins short-form realizations.
    let kinds = [varlena(Align::Double), fixed(4, Align::Int)];
    let diff = compare(&kinds, &[0, 1], &[1, 0]).unwrap();
    assert!(!diff.b_dominates(), "{diff:?}");
    assert!(!diff.a_dominates(), "{diff:?}");
}

#[test]
fn certainty_trade_is_incomparable() {
    // (timetz, timetz, text) pads a flat 4; interposing the text trades that for 0..=7.
    let kinds = [fixed(12, Align::Double), fixed(12, Align::Double), varlena(Align::Int)];
    let diff = compare(&kinds, &[0, 1, 2], &[0, 2, 1]).unwrap();
    assert!(!diff.b_dominates(), "{diff:?}");
    assert!(!diff.a_dominates(), "{diff:?}");
    assert_eq!((diff.min, diff.max), (-3, 4));
}

#[test]
fn band_pair_is_incomparable_with_form_bands() {
    // Issue #10's W1/W2 example: which varlena gets the aligned slot after the fixed block.
    let kinds = [fixed(8, Align::Double), varlena(Align::Int), varlena(Align::Double)];
    let text_first = [0usize, 1, 2];
    let array_first = [0usize, 2, 1];
    let diff = compare(&kinds, &text_first, &array_first).unwrap();
    assert!(!diff.b_dominates(), "{diff:?}");
    assert!(!diff.a_dominates(), "{diff:?}");
    let bands = bands(&kinds, &text_first, &array_first).unwrap();
    assert_eq!(bands.len(), 4);
    let by_combo = |long: &[usize]| bands.iter().find(|b| b.long_form == long).unwrap().diff;
    assert!(by_combo(&[]).equal(), "all short: both orders pad zero");
    let text_long = by_combo(&[1]);
    assert!(
        text_long.a_dominates(),
        "text long, array short: text-first pays nothing"
    );
    let array_long = by_combo(&[2]);
    assert!(
        array_long.b_dominates(),
        "array long: array-first gives it the aligned slot"
    );
}

#[test]
fn proven_short_columns_stay_short_in_every_band() {
    let kinds = [fixed(8, Align::Double), short(), varlena(Align::Int)];
    let bands = bands(&kinds, &[0, 1, 2], &[0, 2, 1]).unwrap();
    assert_eq!(bands.len(), 2, "only the long-capable text splits bands");
    assert!(bands.iter().all(|b| !b.long_form.contains(&1)));
}

#[test]
fn identical_orders_compare_equal() {
    let kinds = [fixed(4, Align::Int), varlena(Align::Int), varlena(Align::Double)];
    let order = [0usize, 1, 2];
    let diff = compare(&kinds, &order, &order).unwrap();
    assert!(diff.equal(), "{diff:?}");
}

/// The joint residue walk and the exhaustive enumeration are two engines for the same
/// quantity; on same-sequence pairs they must agree exactly.
mod engine_agreement {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn joint_walk_matches_enumeration(
            kinds in proptest::collection::vec(
                prop_oneof![
                    Just(ColumnKind::Fixed { len: 1, align: Align::Char }),
                    Just(ColumnKind::Fixed { len: 2, align: Align::Short }),
                    Just(ColumnKind::Fixed { len: 4, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 6, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 8, align: Align::Double }),
                    Just(ColumnKind::Fixed { len: 12, align: Align::Double }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: false }),
                    Just(ColumnKind::Varlena { align: Align::Double, proven_short: false }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: true }),
                ],
                2..=6
            ),
            seed in any::<u64>()
        ) {
            // Derive a second order that keeps the varlena sequence: rotate the fixed columns.
            let n = kinds.len();
            let a: Vec<usize> = (0..n).collect();
            let fixed_positions: Vec<usize> = (0..n).filter(|&i| kinds[i].is_fixed()).collect();
            let mut b = a.clone();
            if fixed_positions.len() >= 2 {
                let rot = (seed as usize) % fixed_positions.len();
                let rotated: Vec<usize> = fixed_positions
                    .iter()
                    .cycle()
                    .skip(rot)
                    .take(fixed_positions.len())
                    .copied()
                    .collect();
                let mut slots = fixed_positions.iter();
                let mut source = rotated.iter();
                for _ in 0..fixed_positions.len() {
                    b[*slots.next().unwrap()] = *source.next().unwrap();
                }
            }
            let varlenas = varlena_sequence(&kinds, &a);
            let via_walk = joint_walk(&kinds, &a, &b, &varlenas, None);
            let via_enum = enumerate(&kinds, &a, &b, &varlenas, None);
            prop_assert_eq!(via_walk, via_enum, "kinds {:?} b {:?}", kinds, b);
        }
    }
}

#[test]
fn flagship_pairs_have_the_measured_verdicts() {
    // interleaved vs grouped (issue #1): grouping dominates, saving up to 11 B/row.
    let kinds = [
        varlena(Align::Int),
        fixed(4, Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        fixed(8, Align::Double),
        fixed(8, Align::Double),
    ];
    let interleaved = [0usize, 1, 2, 3, 4, 5, 6, 7];
    let grouped = [6usize, 7, 1, 0, 2, 3, 4, 5];
    let diff = compare(&kinds, &interleaved, &grouped).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    assert_eq!((diff.min, diff.max), (0, 11));
    // (text, macaddr) vs (macaddr, text): incomparable, since long texts with friendly
    // residues favor text-first while short payloads favor macaddr-first.
    let k3 = [varlena(Align::Int), fixed(6, Align::Int)];
    let d3 = compare(&k3, &[0, 1], &[1, 0]).unwrap();
    assert!(!d3.b_dominates() && !d3.a_dominates(), "{d3:?}");
    // (text, boolean, bigint): the aligned-slot order dominates, the classic fixed-first
    // order does not (it can lose 3 B/row to a friendly long text).
    let k4 = [varlena(Align::Int), fixed(1, Align::Char), fixed(8, Align::Double)];
    let aligned_slot = compare(&k4, &[0, 1, 2], &[2, 0, 1]).unwrap();
    assert!(aligned_slot.b_dominates(), "{aligned_slot:?}");
    assert_eq!((aligned_slot.min, aligned_slot.max), (0, 7));
    let fixed_first = compare(&k4, &[0, 1, 2], &[2, 1, 0]).unwrap();
    assert!(!fixed_first.b_dominates(), "{fixed_first:?}");
    assert_eq!(
        fixed_first.min, -3,
        "a friendly long text makes fixed-first strictly worse"
    );
}

/// A from-scratch oracle sharing no code with the engine: byte offsets instead of residues,
/// explicit alignment arithmetic, concrete payload lengths for all three storage forms (short
/// 0..=7, long 127..=134, and the 18-byte TOAST pointer). A wrong storage convention inside
/// `varlena_step` passes the engine-vs-engine agreement test unnoticed; it cannot pass this.
mod independent_oracle {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Copy)]
    enum Value {
        Short(u64),
        Long(u64),
        Toast,
    }

    fn align_up(off: u64, align: u64) -> u64 {
        off.div_ceil(align) * align
    }

    fn oracle_pad(kinds: &[ColumnKind], order: &[usize], values: &[Value], slot_of: &[usize]) -> u64 {
        let mut off = 0u64;
        let mut total = 0u64;
        for &i in order {
            match kinds[i] {
                ColumnKind::Fixed { len, align } => {
                    let aligned = align_up(off, align.bytes());
                    total += aligned - off;
                    off = aligned + len;
                }
                ColumnKind::Varlena { align, .. } => match values[slot_of[i]] {
                    Value::Short(len) => off += 1 + len,
                    Value::Long(len) => {
                        let aligned = align_up(off, align.bytes());
                        total += aligned - off;
                        off = aligned + 4 + len;
                    }
                    Value::Toast => off += 18,
                },
            }
        }
        total
    }

    fn oracle_bounds(kinds: &[ColumnKind], a: &[usize], b: &[usize]) -> DiffBounds {
        let varlenas: Vec<usize> = (0..kinds.len())
            .filter(|&i| matches!(kinds[i], ColumnKind::Varlena { .. }))
            .collect();
        let mut slot_of = vec![usize::MAX; kinds.len()];
        for (slot, &c) in varlenas.iter().enumerate() {
            slot_of[c] = slot;
        }
        let domains: Vec<Vec<Value>> = varlenas
            .iter()
            .map(|&c| {
                let mut domain: Vec<Value> = (0..8).map(Value::Short).collect();
                if long_capable(kinds[c]) {
                    domain.extend((127..135).map(Value::Long));
                    domain.push(Value::Toast);
                }
                domain
            })
            .collect();
        let mut bounds: Option<DiffBounds> = None;
        let mut values = vec![Value::Short(0); varlenas.len()];
        #[allow(clippy::too_many_arguments)]
        fn go(
            kinds: &[ColumnKind],
            a: &[usize],
            b: &[usize],
            domains: &[Vec<Value>],
            slot_of: &[usize],
            values: &mut Vec<Value>,
            depth: usize,
            bounds: &mut Option<DiffBounds>,
        ) {
            if depth == domains.len() {
                let d = oracle_pad(kinds, a, values, slot_of) as i64 - oracle_pad(kinds, b, values, slot_of) as i64;
                match bounds {
                    Some(existing) => {
                        existing.min = existing.min.min(d);
                        existing.max = existing.max.max(d);
                    }
                    slot @ None => *slot = Some(DiffBounds { min: d, max: d }),
                }
                return;
            }
            for &value in &domains[depth] {
                values[depth] = value;
                go(kinds, a, b, domains, slot_of, values, depth + 1, bounds);
            }
        }
        go(kinds, a, b, &domains, &slot_of, &mut values, 0, &mut bounds);
        bounds.expect("at least one realization")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn engine_bounds_match_the_independent_oracle(
            kinds in proptest::collection::vec(
                prop_oneof![
                    Just(ColumnKind::Fixed { len: 1, align: Align::Char }),
                    Just(ColumnKind::Fixed { len: 2, align: Align::Short }),
                    Just(ColumnKind::Fixed { len: 4, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 6, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 8, align: Align::Double }),
                    Just(ColumnKind::Fixed { len: 12, align: Align::Double }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: false }),
                    Just(ColumnKind::Varlena { align: Align::Double, proven_short: false }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: true }),
                ],
                2..=5
            ).prop_filter("at most 3 varlenas keeps the oracle enumerable", |kinds| {
                kinds.iter().filter(|k| matches!(k, ColumnKind::Varlena { .. })).count() <= 3
            }),
            rotation in 0usize..120
        ) {
            let n = kinds.len();
            let a: Vec<usize> = (0..n).collect();
            let b: Vec<usize> = (0..n).map(|i| (i + 1 + rotation % n.max(1)) % n).collect();
            let engine = compare(&kinds, &a, &b).expect("within budget");
            let oracle = oracle_bounds(&kinds, &a, &b);
            prop_assert_eq!(engine, oracle, "kinds {:?} b {:?}", kinds, b);
        }
    }
}
