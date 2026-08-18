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
