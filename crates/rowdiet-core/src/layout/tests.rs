use super::*;

fn fixed(len: u64, align: Align) -> ColumnKind {
    ColumnKind::Fixed { len, align }
}

fn varlena(align: Align) -> ColumnKind {
    ColumnKind::Varlena {
        align,
        proven_short: false,
        payload: Payload::ANY,
    }
}

fn short() -> ColumnKind {
    ColumnKind::Varlena {
        align: Align::Int,
        proven_short: true,
        payload: Payload {
            compressible: false,
            ..Payload::ANY
        },
    }
}

#[test]
fn pad_and_maxalign() {
    assert_eq!(pad(0, 8), 0);
    assert_eq!(pad(1, 8), 7);
    assert_eq!(pad(8, 8), 0);
    assert_eq!(pad(9, 4), 3);
    assert_eq!(maxalign(23), 24);
    assert_eq!(maxalign(24), 24);
    assert_eq!(maxalign(41), 48);
}

#[test]
fn walk_classic_bool_int8_interleave() {
    let w = walk(&[
        fixed(1, Align::Char),
        fixed(8, Align::Double),
        fixed(1, Align::Char),
        fixed(8, Align::Double),
    ]);
    assert_eq!(w.padding, 14);
    assert_eq!(w.end, Some(32));
    assert_eq!(w.expected_padding(), 14.0);
    assert_eq!((w.padding_min(), w.padding_max()), (14, 14));
    let s = walk(&[
        fixed(8, Align::Double),
        fixed(8, Align::Double),
        fixed(1, Align::Char),
        fixed(1, Align::Char),
    ]);
    assert_eq!(s.padding, 0);
    assert_eq!(s.end, Some(18));
}

#[test]
fn footprint_rung_crossing() {
    let cur = walk(&[
        fixed(4, Align::Int),
        fixed(8, Align::Double),
        fixed(4, Align::Int),
        fixed(8, Align::Double),
    ]);
    assert_eq!(footprint(cur.end.unwrap()), 56);
    let sug = walk(&[
        fixed(8, Align::Double),
        fixed(8, Align::Double),
        fixed(4, Align::Int),
        fixed(4, Align::Int),
    ]);
    assert_eq!(footprint(sug.end.unwrap()), 48);
    assert_eq!(rows_per_page(56), 136);
    assert_eq!(rows_per_page(48), 157);
}

#[test]
fn footprint_rung_not_crossed() {
    let cur = walk(&[fixed(1, Align::Char), fixed(8, Align::Double), fixed(8, Align::Double)]);
    let sug = walk(&[fixed(8, Align::Double), fixed(8, Align::Double), fixed(1, Align::Char)]);
    assert_eq!(cur.padding, 7);
    assert_eq!(sug.padding, 0);
    assert_eq!(footprint(cur.end.unwrap()), footprint(sug.end.unwrap()));
}

#[test]
fn uuid_is_char_aligned_never_pads() {
    let w = walk(&[fixed(16, Align::Char), fixed(8, Align::Double), fixed(16, Align::Char)]);
    assert_eq!(w.padding, 0);
}

#[test]
fn irregulars_sort_to_group_end() {
    let kinds = [
        fixed(12, Align::Double),
        fixed(8, Align::Double),
        fixed(6, Align::Int),
        fixed(4, Align::Int),
        fixed(2, Align::Short),
    ];
    let order = suggested_order(&kinds);
    assert_eq!(order, vec![1, 0, 3, 2, 4]);
    let sug: Vec<_> = order.iter().map(|&i| kinds[i]).collect();
    assert_eq!(walk(&sug).padding, 0);
}

#[test]
fn suggested_survives_null_masks() {
    let kinds = [
        fixed(8, Align::Double),
        fixed(8, Align::Double),
        fixed(4, Align::Int),
        fixed(2, Align::Short),
        fixed(1, Align::Char),
    ];
    let n = kinds.len();
    for mask in 0u32..(1 << n) {
        let subset: Vec<_> = (0..n).filter(|&i| mask & (1 << i) != 0).map(|i| kinds[i]).collect();
        assert_eq!(walk(&subset).padding, 0, "mask {mask:b}");
    }
}

#[test]
fn two_irregulars_get_the_exact_search() {
    let kinds = [fixed(12, Align::Double), fixed(12, Align::Double), fixed(4, Align::Int)];
    let order = suggested_order(&kinds);
    let sug: Vec<_> = order.iter().map(|&i| kinds[i]).collect();
    assert_eq!(
        walk(&sug).padding,
        0,
        "int4 interposed between the two timetz: {order:?}"
    );
}

#[test]
fn exact_search_never_worse_than_plain_sort() {
    let batteries: Vec<Vec<ColumnKind>> = vec![
        vec![fixed(12, Align::Double), fixed(12, Align::Double), fixed(4, Align::Int)],
        vec![
            fixed(12, Align::Double),
            fixed(6, Align::Int),
            fixed(6, Align::Int),
            fixed(2, Align::Short),
        ],
        vec![
            fixed(12, Align::Double),
            fixed(12, Align::Double),
            fixed(12, Align::Double),
            fixed(4, Align::Int),
        ],
        vec![
            fixed(8, Align::Double),
            fixed(4, Align::Int),
            fixed(2, Align::Short),
            fixed(1, Align::Char),
        ],
        vec![
            fixed(6, Align::Int),
            fixed(6, Align::Int),
            fixed(8, Align::Double),
            fixed(1, Align::Char),
        ],
    ];
    for kinds in batteries {
        let mut sorted: Vec<usize> = (0..kinds.len()).collect();
        sorted.sort_by_key(|&i| sort_key(&kinds[i], i));
        let plain = walk(&sorted.iter().map(|&i| kinds[i]).collect::<Vec<_>>()).padding;
        let refined = suggested_order(&kinds);
        let exact = walk(&refined.iter().map(|&i| kinds[i]).collect::<Vec<_>>()).padding;
        assert!(exact <= plain, "{kinds:?}: exact {exact} vs sorted {plain}");
    }
}

#[test]
fn exact_search_keeps_varlena_tail() {
    let kinds = [
        fixed(12, Align::Double),
        fixed(12, Align::Double),
        fixed(4, Align::Int),
        varlena(Align::Int),
    ];
    let order = suggested_order(&kinds);
    assert_eq!(order[3], 3, "varlena stays last: {order:?}");
    let fixed_part: Vec<_> = order[..3].iter().map(|&i| kinds[i]).collect();
    assert_eq!(walk(&fixed_part).padding, 0);
}

#[test]
fn regular_schemas_keep_the_heuristic_shape() {
    let kinds = [
        fixed(1, Align::Char),
        fixed(8, Align::Double),
        fixed(4, Align::Int),
        fixed(2, Align::Short),
    ];
    assert_eq!(suggested_order(&kinds), vec![1, 2, 3, 0]);
}

#[test]
fn varlena_cluster_and_align_desc() {
    let kinds = [
        varlena(Align::Int),
        fixed(8, Align::Double),
        short(),
        varlena(Align::Double),
        fixed(1, Align::Char),
    ];
    let order = suggested_order(&kinds);
    assert_eq!(order, vec![1, 4, 3, 0, 2]);
}

#[test]
fn proven_short_never_pads_and_never_restores_certainty() {
    let w = walk(&[fixed(1, Align::Char), short(), fixed(8, Align::Double)]);
    assert_eq!(w.columns[1].pad_before, PadRange::certain(0));
    assert_eq!(w.columns[1].offset, Some(1));
    // The proven-short header is 1 byte, but the payload byte length still varies (multibyte
    // encodings), so the following column pads over the full residue set.
    assert_eq!(
        w.columns[2].pad_before,
        PadRange {
            min: 0,
            max: 7,
            expected_eighths: 28
        }
    );
    assert_eq!(w.columns[2].offset, None);
    assert_eq!(w.end, None);
}

#[test]
fn fixed_after_a_varlena_pads_data_dependently() {
    let w = walk(&[varlena(Align::Int), fixed(8, Align::Double)]);
    assert_eq!(w.columns[0].pad_before, PadRange::certain(0));
    assert_eq!(w.columns[0].offset, Some(0));
    assert_eq!(w.columns[1].offset, None);
    assert_eq!(w.padding, 0);
    assert_eq!(w.uncertain_expected_eighths, 28);
    assert_eq!((w.padding_min(), w.padding_max()), (0, 7));
    assert_eq!(w.expected_padding(), 3.5);
    assert_eq!(w.end, None);
}

#[test]
fn residue_narrowing_algebra() {
    assert_eq!(Residues::FULL.aligned(8), Residues::START);
    assert_eq!(Residues(0b0000_0110).aligned(8), Residues::START);
    assert_eq!(Residues::FULL.aligned(4), Residues(0b0001_0001));
    assert_eq!(Residues::FULL.aligned(2), Residues(0b0101_0101));
    assert_eq!(Residues::FULL.aligned(1), Residues::FULL);
    assert_eq!(Residues(0b0001_0001).aligned(2), Residues(0b0001_0001));
    assert_eq!(Residues::START.shifted(3), Residues(0b0000_1000));
    assert_eq!(Residues::START.shifted(11), Residues(0b0000_1000));
    assert_eq!(Residues::FULL.shifted(5), Residues::FULL);
}

#[test]
fn expected_pads_over_the_full_set_match_the_closed_form() {
    assert_eq!(
        Residues::FULL.pad_to(8),
        PadRange {
            min: 0,
            max: 7,
            expected_eighths: 28
        }
    );
    assert_eq!(
        Residues::FULL.pad_to(4),
        PadRange {
            min: 0,
            max: 3,
            expected_eighths: 12
        }
    );
    assert_eq!(
        Residues::FULL.pad_to(2),
        PadRange {
            min: 0,
            max: 1,
            expected_eighths: 4
        }
    );
    assert_eq!(Residues::FULL.pad_to(1), PadRange::certain(0));
}

#[test]
fn eight_aligned_column_restores_determinism() {
    let w = walk(&[
        varlena(Align::Int),
        fixed(8, Align::Double),
        fixed(4, Align::Int),
        fixed(8, Align::Double),
    ]);
    assert_eq!(
        w.columns[1].pad_before,
        PadRange {
            min: 0,
            max: 7,
            expected_eighths: 28
        }
    );
    // Aligning to 8 collapsed the residue set back to {0}: later pads are certain again.
    assert_eq!(w.columns[2].pad_before, PadRange::certain(0));
    assert_eq!(w.columns[3].pad_before, PadRange::certain(4));
    assert_eq!(w.padding, 4);
    assert_eq!(w.uncertain_expected_eighths, 28);
    // Offsets stay unknown even where the pad is certain: the pad is pinned mod 8 alone.
    assert_eq!(w.columns[2].offset, None);
}

#[test]
fn four_aligned_column_narrows_to_two_residues() {
    let w = walk(&[varlena(Align::Int), fixed(4, Align::Int), fixed(4, Align::Int)]);
    assert_eq!(
        w.columns[1].pad_before,
        PadRange {
            min: 0,
            max: 3,
            expected_eighths: 12
        }
    );
    // After a 4-aligned column the set is {0, 4} shifted by 4 = {0, 4}: the next 4-aligned
    // column pads zero on both members, so its pad is certain.
    assert_eq!(w.columns[2].pad_before, PadRange::certain(0));
    assert_eq!(w.padding, 0);
}

#[test]
fn varlena_pads_score_the_short_form_and_bound_the_long_form() {
    // int2 then text: the text pad is 0 short-form (unaligned) and 2 long-form. pageinspect
    // measures such tables flat at zero padding; the old long-form pin printed a certain 2.
    let w = walk(&[fixed(2, Align::Short), varlena(Align::Int)]);
    assert_eq!(
        w.columns[1].pad_before,
        PadRange {
            min: 0,
            max: 2,
            expected_eighths: 0
        }
    );
    assert_eq!(w.columns[1].offset, None, "the start depends on the storage form");
    assert_eq!(w.padding, 0);
    assert_eq!(w.expected_padding(), 0.0);
    assert_eq!((w.padding_min(), w.padding_max()), (0, 2));
}

#[test]
fn d_aligned_varlena_expects_zero_and_bounds_at_its_long_form_pad() {
    // polygon/bigint[] class after int4: long form would pad 4, short form and TOAST pointers
    // pad 0 (both measured unaligned on disk).
    let w = walk(&[fixed(4, Align::Int), varlena(Align::Double), varlena(Align::Int)]);
    assert_eq!(
        w.columns[1].pad_before,
        PadRange {
            min: 0,
            max: 4,
            expected_eighths: 0
        }
    );
    assert_eq!(w.expected_padding(), 0.0);
    assert_eq!((w.padding_min(), w.padding_max()), (0, 7));
}

#[test]
fn a_lone_proven_short_varlena_is_certain_zero() {
    let w = walk(&[short()]);
    assert_eq!(w.columns[0].pad_before, PadRange::certain(0));
    assert_eq!(w.padding, 0);
    assert_eq!((w.padding_min(), w.padding_max()), (0, 0));
    assert_eq!(w.expected_padding(), 0.0);
    assert_eq!(w.end, None, "the payload still makes the end unknowable");
    assert_eq!(tier(&[short()]), Tier::Estimate);
}

#[test]
fn irregular_shift_reaches_a_partial_coset() {
    // macaddr after a varlena: {0,4} shifted by 6 gives {2,6}, where a float8 pads 2 or 6,
    // expected 4.0, above the full set's 3.5.
    let w = walk(&[varlena(Align::Int), fixed(6, Align::Int), fixed(8, Align::Double)]);
    assert_eq!(
        w.columns[1].pad_before,
        PadRange {
            min: 0,
            max: 3,
            expected_eighths: 12
        }
    );
    assert_eq!(
        w.columns[2].pad_before,
        PadRange {
            min: 2,
            max: 6,
            expected_eighths: 32
        }
    );
    assert_eq!(w.expected_padding(), 5.5);
    assert_eq!((w.padding_min(), w.padding_max()), (2, 9));
}

#[test]
fn text_before_macaddr_surfaces_the_fixed_first_win() {
    // (text, macaddr) expects 1.5; (macaddr, text) expects 0; measured: 1.5 vs flat 0. The old
    // objective scored the swap at 2.0 and suppressed the advice.
    let current = walk(&[varlena(Align::Int), fixed(6, Align::Int)]);
    assert_eq!(current.expected_padding(), 1.5);
    let kinds = [varlena(Align::Int), fixed(6, Align::Int)];
    let order = suggested_order(&kinds);
    assert_eq!(order, vec![1, 0]);
    let suggested = walk(&order.iter().map(|&i| kinds[i]).collect::<Vec<_>>());
    assert_eq!(suggested.expected_padding(), 0.0);
}

#[test]
fn certainty_trade_splits_the_search_poles() {
    // Two timetz cannot pack flat (certain 4 between them); hiding the second behind the text
    // trades that for 0..=7. The certainty pole finds the trade, the minimax pole and the
    // heuristic refuse it, and the decision policy reports it as a frontier only.
    let kinds = [fixed(12, Align::Double), fixed(12, Align::Double), varlena(Align::Int)];
    assert_eq!(
        suggested_order(&kinds),
        vec![0, 1, 2],
        "the heuristic keeps fixed first"
    );
    let search = search(&kinds);
    assert_eq!(search.scope, SearchScope::Complete);
    assert_eq!(search.certainty_pole, Some(vec![0, 2, 1]), "timetz, varlena, timetz");
    assert_eq!(
        search.minimax_pole,
        Some(vec![0, 1, 2]),
        "a certain 4 beats a possible 7"
    );
    let veil = walk(&[kinds[0], kinds[2], kinds[1]]);
    assert_eq!(veil.padding, 0);
    assert_eq!(veil.padding_max(), 7);
}

#[test]
fn aligned_slot_and_padless_tail_reach_zero_worst_case() {
    // (text, boolean, bigint): the minimax pole hands the text the aligned slot behind the
    // bigint and parks the boolean last, reaching zero padding in every storage form.
    let kinds = [varlena(Align::Int), fixed(1, Align::Char), fixed(8, Align::Double)];
    let search = search(&kinds);
    let minimax = search.minimax_pole.unwrap();
    assert_eq!(minimax, vec![2, 0, 1]);
    let w = walk(&minimax.iter().map(|&i| kinds[i]).collect::<Vec<_>>());
    assert_eq!((w.padding, w.padding_max()), (0, 0));
}

#[test]
fn interleaved_vs_grouped_kind_sequences() {
    // The issue-1 repro at the kind level: same multiset, opposite orders. Only the stranded
    // fixed columns expect padding (int4 1.5 + float8 3.5); varlena pads expect 0 (short/TOAST
    // form) and contribute only to the max. Grouping expects zero, and pageinspect measures
    // the grouped table flat at zero padding.
    let interleaved = walk(&[
        varlena(Align::Int),
        fixed(4, Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        fixed(8, Align::Double),
        fixed(8, Align::Double),
    ]);
    let grouped = walk(&[
        fixed(8, Align::Double),
        fixed(8, Align::Double),
        fixed(4, Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
        varlena(Align::Int),
    ]);
    assert_eq!(interleaved.expected_padding(), 5.0);
    assert_eq!((interleaved.padding_min(), interleaved.padding_max()), (0, 19));
    assert_eq!(grouped.expected_padding(), 0.0);
    assert_eq!(grouped.padding, 0);
    assert_eq!((grouped.padding_min(), grouped.padding_max()), (0, 12));
}

#[test]
fn tiers() {
    assert_eq!(tier(&[fixed(4, Align::Int)]), Tier::Exact);
    assert_eq!(tier(&[fixed(4, Align::Int), varlena(Align::Int)]), Tier::Estimate);
    assert_eq!(tier(&[]), Tier::Exact);
}

#[test]
fn null_bitmap_thresholds() {
    assert_eq!(bare_thoff(), 24);
    assert_eq!(null_thoff(8), 24);
    assert_eq!(null_thoff(9), 32);
    assert_eq!(null_thoff(72), 32);
    assert_eq!(null_thoff(73), 40);
}

/// Exhaustive cross-check of the fixed-block ordering against brute force: for small all-fixed
/// multisets, the suggested order's padding must equal the true minimum over every permutation.
/// This pins the DP's interior arithmetic (residue classes, cost recurrence, better-than-sorted
/// acceptance), which single-point tests left unobserved — its surviving mutants motivated this.
#[test]
fn suggested_order_matches_brute_force_minimum_on_fixed_sets() {
    let f = |len, align| ColumnKind::Fixed { len, align };
    let cases: Vec<Vec<ColumnKind>> = vec![
        vec![f(12, Align::Double), f(12, Align::Double), f(4, Align::Int)],
        vec![
            f(12, Align::Double),
            f(6, Align::Int),
            f(1, Align::Char),
            f(8, Align::Double),
        ],
        vec![
            f(52, Align::Double),
            f(12, Align::Double),
            f(4, Align::Int),
            f(2, Align::Short),
        ],
        vec![
            f(8, Align::Double),
            f(4, Align::Int),
            f(4, Align::Int),
            f(2, Align::Short),
            f(1, Align::Char),
        ],
        vec![
            f(12, Align::Double),
            f(12, Align::Double),
            f(6, Align::Int),
            f(6, Align::Int),
            f(1, Align::Char),
        ],
        vec![
            f(16, Align::Char),
            f(12, Align::Double),
            f(2, Align::Short),
            f(4, Align::Int),
            f(8, Align::Double),
        ],
        vec![
            f(52, Align::Double),
            f(6, Align::Int),
            f(12, Align::Double),
            f(1, Align::Char),
            f(2, Align::Short),
            f(4, Align::Int),
        ],
    ];
    for kinds in cases {
        let order = suggested_order(&kinds);
        let ordered: Vec<ColumnKind> = order.iter().map(|&i| kinds[i]).collect();
        let suggested_padding = walk(&ordered).padding;
        let brute = brute_force_min_padding(&kinds);
        assert_eq!(suggested_padding, brute, "kinds: {kinds:?}, order: {order:?}");
    }
}

fn brute_force_min_padding(kinds: &[ColumnKind]) -> u64 {
    fn go(kinds: &[ColumnKind], current: &mut Vec<ColumnKind>, used: &mut Vec<bool>, best: &mut u64) {
        if current.len() == kinds.len() {
            *best = (*best).min(walk(current).padding);
            return;
        }
        for i in 0..kinds.len() {
            if !used[i] {
                used[i] = true;
                current.push(kinds[i]);
                go(kinds, current, used, best);
                current.pop();
                used[i] = false;
            }
        }
    }
    let mut best = u64::MAX;
    go(kinds, &mut Vec::new(), &mut vec![false; kinds.len()], &mut best);
    best
}

/// Property form of the brute-force cross-check: random small multisets from the realistic
/// kind pool, minimality asserted against exhaustive permutation search, on the certain
/// padding for all-fixed multisets and on the expected objective once varlenas join the pool.
mod minimality_property {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn suggested_order_is_minimal_on_random_fixed_multisets(
            kinds in proptest::collection::vec(
                prop_oneof![
                    Just(ColumnKind::Fixed { len: 1, align: Align::Char }),
                    Just(ColumnKind::Fixed { len: 2, align: Align::Short }),
                    Just(ColumnKind::Fixed { len: 4, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 8, align: Align::Double }),
                    Just(ColumnKind::Fixed { len: 12, align: Align::Double }),
                    Just(ColumnKind::Fixed { len: 6, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 16, align: Align::Char }),
                    Just(ColumnKind::Fixed { len: 52, align: Align::Double }),
                ],
                3..=7
            )
        ) {
            let order = suggested_order(&kinds);
            let mut seen = vec![false; kinds.len()];
            for &i in &order {
                prop_assert!(!seen[i], "not a permutation: {:?}", order);
                seen[i] = true;
            }
            let ordered: Vec<ColumnKind> = order.iter().map(|&i| kinds[i]).collect();
            prop_assert_eq!(walk(&ordered).padding, brute_force_min_padding(&kinds), "kinds: {:?}", kinds);
        }

        /// The shipped objectives are the two lexicographic pairs over (deterministic,
        /// worst-case) padding; each search pole must hit the brute-force minimum of its pair
        /// over all permutations. Irregulars (timetz, macaddr) and varlenas are in the pool, so
        /// nonzero optima are common and the property can fail (the reviews found a
        /// regular-only pool asserting 0 == 0 everywhere).
        #[test]
        fn search_poles_minimize_their_lexicographic_objectives(
            kinds in proptest::collection::vec(
                prop_oneof![
                    Just(ColumnKind::Fixed { len: 1, align: Align::Char }),
                    Just(ColumnKind::Fixed { len: 2, align: Align::Short }),
                    Just(ColumnKind::Fixed { len: 4, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 6, align: Align::Int }),
                    Just(ColumnKind::Fixed { len: 8, align: Align::Double }),
                    Just(ColumnKind::Fixed { len: 12, align: Align::Double }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: false, payload: Payload::ANY }),
                    Just(ColumnKind::Varlena { align: Align::Double, proven_short: false, payload: Payload::ANY }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: true, payload: Payload { compressible: false, ..Payload::ANY } }),
                    Just(ColumnKind::Varlena { align: Align::Int, proven_short: true, payload: Payload::ANY }),
                ],
                3..=6
            )
        ) {
            let lex = |w: &Walk, certainty_first: bool| {
                if certainty_first {
                    w.padding * 512 + w.padding_max()
                } else {
                    w.padding_max() * 512 + w.padding
                }
            };
            let search = search(&kinds);
            prop_assert_eq!(search.scope, SearchScope::Complete);
            for (pole, certainty_first) in [
                (search.certainty_pole.as_ref().unwrap(), true),
                (search.minimax_pole.as_ref().unwrap(), false),
            ] {
                let mut seen = vec![false; kinds.len()];
                for &i in pole {
                    prop_assert!(!seen[i], "not a permutation: {:?}", pole);
                    seen[i] = true;
                }
                let ordered: Vec<ColumnKind> = pole.iter().map(|&i| kinds[i]).collect();
                let achieved = lex(&walk(&ordered), certainty_first);
                let brute = brute_force_lex_min(&kinds, certainty_first);
                prop_assert_eq!(
                    achieved,
                    brute,
                    "kinds {:?} pole {:?} certainty_first {}",
                    kinds,
                    pole,
                    certainty_first
                );
            }
        }
    }

    fn brute_force_lex_min(kinds: &[ColumnKind], certainty_first: bool) -> u64 {
        fn go(kinds: &[ColumnKind], current: &mut Vec<ColumnKind>, used: &mut Vec<bool>, best: &mut u64, cf: bool) {
            if current.len() == kinds.len() {
                let w = walk(current);
                let cost = if cf {
                    w.padding * 512 + w.padding_max()
                } else {
                    w.padding_max() * 512 + w.padding
                };
                *best = (*best).min(cost);
                return;
            }
            for i in 0..kinds.len() {
                if !used[i] {
                    used[i] = true;
                    current.push(kinds[i]);
                    go(kinds, current, used, best, cf);
                    current.pop();
                    used[i] = false;
                }
            }
        }
        let mut best = u64::MAX;
        go(
            kinds,
            &mut Vec::new(),
            &mut vec![false; kinds.len()],
            &mut best,
            certainty_first,
        );
        best
    }
}

/// Simulate one concrete tuple layout: every varlena gets a storage form and a payload length,
/// fixed columns pad exactly as Postgres places them. This is the model's oracle: the reported
/// bounds and expectation are checked against exhaustive enumeration of concrete rows.
fn concrete_padding(kinds: &[ColumnKind], varlena_vals: &[(bool, u64)]) -> u64 {
    let mut off = 0u64;
    let mut total = 0u64;
    let mut vi = 0;
    for kind in kinds {
        match kind {
            ColumnKind::Fixed { len, align } => {
                let p = pad(off, align.bytes());
                total += p;
                off += p + len;
            }
            ColumnKind::Varlena {
                align, proven_short, ..
            } => {
                let (short, payload) = varlena_vals[vi];
                vi += 1;
                assert!(short || !proven_short, "proven-short columns store short-form only");
                if short {
                    off += 1 + payload;
                } else {
                    let p = pad(off, align.bytes());
                    total += p;
                    off += 4 + payload;
                }
            }
        }
    }
    total
}

/// Every combination of per-varlena (form, payload residue). Short payloads use 0..=7 directly;
/// long payloads use 128 + r, which is a legal long-form length and sweeps every residue.
fn enumerate_concrete(kinds: &[ColumnKind], short_only: bool) -> Vec<u64> {
    let varlenas: Vec<bool> = kinds
        .iter()
        .filter_map(|k| match k {
            ColumnKind::Varlena { proven_short, .. } => Some(*proven_short),
            ColumnKind::Fixed { .. } => None,
        })
        .collect();
    let v = varlenas.len();
    let forms_per = if short_only { 1 } else { 2 };
    let combos = (forms_per * 8u64).pow(v as u32);
    let mut out = Vec::with_capacity(combos as usize);
    for combo in 0..combos {
        let mut vals = Vec::with_capacity(v);
        let mut rest = combo;
        let mut legal = true;
        for &proven in &varlenas {
            let residue = rest % 8;
            rest /= 8;
            let long = !short_only && rest % 2 == 1;
            rest /= forms_per;
            if long && proven {
                legal = false;
                break;
            }
            if long {
                vals.push((false, 128 + residue));
            } else {
                vals.push((true, residue));
            }
        }
        if legal {
            out.push(concrete_padding(kinds, &vals));
        }
    }
    out
}

/// The model's three semantic claims, checked against enumerated concrete tuples:
/// min/max bound every storage-form/payload combination and are both attained (joint
/// achievability), and the expectation equals the exact mean over short-form tuples with
/// uniform independent payload residues (the two stated assumptions, made executable).
#[test]
fn bounds_and_expectation_match_enumerated_concrete_tuples() {
    let cases: Vec<Vec<ColumnKind>> = vec![
        vec![fixed(2, Align::Short), varlena(Align::Int)],
        vec![varlena(Align::Int), fixed(6, Align::Int), fixed(8, Align::Double)],
        vec![fixed(4, Align::Int), varlena(Align::Double), varlena(Align::Int)],
        vec![
            varlena(Align::Int),
            fixed(4, Align::Int),
            varlena(Align::Int),
            fixed(8, Align::Double),
        ],
        vec![fixed(12, Align::Double), varlena(Align::Int), fixed(12, Align::Double)],
        vec![fixed(1, Align::Char), short(), fixed(8, Align::Double)],
        vec![varlena(Align::Double), varlena(Align::Double), fixed(8, Align::Double)],
        vec![
            fixed(8, Align::Double),
            fixed(4, Align::Int),
            short(),
            varlena(Align::Int),
        ],
    ];
    for kinds in cases {
        let w = walk(&kinds);
        let all = enumerate_concrete(&kinds, false);
        let observed_min = *all.iter().min().unwrap();
        let observed_max = *all.iter().max().unwrap();
        assert_eq!(observed_min, w.padding_min(), "min bound: {kinds:?}");
        assert_eq!(observed_max, w.padding_max(), "max bound: {kinds:?}");
        let short_runs = enumerate_concrete(&kinds, true);
        let sum: u64 = short_runs.iter().sum();
        let n = short_runs.len() as u64;
        assert_eq!(sum * 8 % n, 0, "the short-form mean must be an exact eighth: {kinds:?}");
        assert_eq!(
            sum * 8 / n,
            w.expected_padding_eighths(),
            "expectation vs short-form uniform mean: {kinds:?}"
        );
    }
}

/// Reachable residue sets stay cosets in Z/8 (sizes 1, 2, 4, 8; equally spaced members), so
/// expected pads divide exactly into eighths, the arithmetic [`Residues::pad_to`] relies on.
mod residue_coset_property {
    use super::*;
    use proptest::prelude::*;

    fn is_coset(mask: u8) -> bool {
        let size = mask.count_ones();
        if !matches!(size, 1 | 2 | 4 | 8) {
            return false;
        }
        let step = 8 / size;
        let tz = mask.trailing_zeros();
        (0..size).all(|k| mask & (1u8 << ((tz + k * step) % 8)) != 0)
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Aligned(u64),
        Shifted(u64),
        Widen,
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn walk_operations_preserve_cosets_and_exact_eighths(
            ops in proptest::collection::vec(
                prop_oneof![
                    (0u32..4).prop_map(|i| Op::Aligned(1 << i)),
                    (0u64..16).prop_map(Op::Shifted),
                    Just(Op::Widen),
                ],
                0..12
            )
        ) {
            let mut set = Residues::START;
            for op in ops {
                set = match op {
                    Op::Aligned(a) => set.aligned(a),
                    Op::Shifted(l) => set.shifted(l),
                    Op::Widen => Residues::FULL,
                };
                prop_assert!(is_coset(set.0), "not a coset: {:#010b}", set.0);
                for align in [1u64, 2, 4, 8] {
                    let range = set.pad_to(align);
                    let mut sum = 0u64;
                    let mut count = 0u64;
                    for r in 0..8u64 {
                        if set.contains(r) {
                            sum += pad(r, align);
                            count += 1;
                        }
                    }
                    prop_assert_eq!(sum * 8 % count, 0, "mean not an exact eighth");
                    prop_assert_eq!(range.expected_eighths, sum * 8 / count);
                    prop_assert!(range.min <= range.max);
                }
            }
        }
    }
}

#[cfg(feature = "serde")]
#[test]
fn tier_display_matches_serde() {
    for tier in [Tier::Exact, Tier::Estimate, Tier::Unknown] {
        let json = serde_json::to_value(tier).unwrap();
        assert_eq!(json.as_str().unwrap(), tier.to_string(), "{tier:?}");
    }
}

#[test]
fn the_search_finishes_on_one_class_as_large_as_the_budget_admits() {
    // A class count past 255 wrapped a u8 and spun the reconstruction forever; a recursive memo
    // overflowed the stack at this depth. The bottom-up DP needs neither.
    let kinds = vec![fixed(12, Align::Double); 69_904];
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(search(&kinds));
    });
    let found = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the search terminates");
    assert_eq!(found.scope, SearchScope::Complete);
    let mut pole = found.certainty_pole.expect("the DP ran");
    pole.sort_unstable();
    assert!(pole.iter().copied().eq(0..69_904));
}

#[test]
fn the_fixed_block_repack_runs_past_24_columns() {
    // 30 fixed columns whose written order pads 100 B: the repack must reach zero at any width.
    let mut kinds = Vec::new();
    for _ in 0..10 {
        kinds.extend([fixed(1, Align::Char), fixed(8, Align::Double)]);
    }
    for _ in 0..5 {
        kinds.extend([fixed(2, Align::Short), fixed(8, Align::Double)]);
    }
    kinds.push(varlena(Align::Int));
    let mut order: Vec<usize> = (0..kinds.len()).collect();
    refine_leading_fixed(&kinds, &mut order);
    let ordered: Vec<ColumnKind> = order.iter().map(|&i| kinds[i]).collect();
    assert_eq!(walk(&kinds).padding, 100);
    assert_eq!(walk(&ordered).padding, 0);
    assert_eq!(order.last(), Some(&30), "the varlena tail stays in place");
}

/// A block walked from where its prefix can end places every column exactly as the whole-table
/// walk does, in rows that store every column; only the absolute offsets are withheld.
mod block_walk {
    use super::*;
    use proptest::prelude::*;

    fn pool() -> impl Strategy<Value = ColumnKind> {
        prop_oneof![
            Just(fixed(1, Align::Char)),
            Just(fixed(2, Align::Short)),
            Just(fixed(4, Align::Int)),
            Just(fixed(6, Align::Int)),
            Just(fixed(8, Align::Double)),
            Just(fixed(12, Align::Double)),
            Just(varlena(Align::Int)),
            Just(varlena(Align::Double)),
            Just(short()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn block_pads_match_the_whole_walk(
            prefix in proptest::collection::vec(pool(), 0..=4),
            block in proptest::collection::vec(pool(), 1..=4),
        ) {
            let whole: Vec<ColumnKind> = prefix.iter().chain(&block).copied().collect();
            let columns: Vec<Column> = prefix.iter().map(|&k| Column::nullable(k)).collect();
            let w = walk_from(Start::after(&columns), &block);
            let full = walk(&whole);
            let pads: Vec<PadRange> = w.columns.iter().map(|c| c.pad_before).collect();
            let tail: Vec<PadRange> = full.columns[prefix.len()..].iter().map(|c| c.pad_before).collect();
            prop_assert_eq!(pads, tail, "{:?} after {:?}", block, prefix);
            prop_assert!(w.end.is_none());
        }
    }
}

#[test]
fn a_prefix_start_reaches_null_and_stored_residues() {
    // (int8, bool NULL): rows that store the bool end at 9, rows that do not at 8.
    let prefix = [
        Column::not_null(fixed(8, Align::Double)),
        Column::nullable(fixed(1, Align::Char)),
    ];
    let start = Start::after(&prefix);
    assert_eq!(start.residues(false), 0b0000_0010);
    assert_eq!(start.residues(true), 0b0000_0011);
    assert_eq!(
        Start::after(&[Column::not_null(varlena(Align::Int))]).residues(true),
        0xFF
    );
    assert_eq!(Start::TABLE.residues(true), 1);
}
