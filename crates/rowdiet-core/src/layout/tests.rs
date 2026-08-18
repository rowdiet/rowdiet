use super::*;

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
    // Offsets stay unknown even where the pad is certain: certainty is mod 8, not absolute.
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
fn interleaved_vs_grouped_kind_sequences() {
    // The issue-1 repro at the kind level: same multiset, opposite orders.
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
    assert_eq!(interleaved.expected_padding(), 9.5);
    assert_eq!((interleaved.padding_min(), interleaved.padding_max()), (0, 19));
    assert_eq!(grouped.expected_padding(), 6.0);
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
/// kind pool, minimality asserted against exhaustive permutation search.
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
