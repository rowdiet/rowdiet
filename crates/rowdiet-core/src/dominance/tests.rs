use super::*;
use crate::layout::{Align, Payload, Start};

fn fixed(len: u64, align: Align) -> Column {
    Column::not_null(ColumnKind::Fixed { len, align })
}

fn varlena(align: Align) -> Column {
    Column::not_null(ColumnKind::Varlena {
        align,
        proven_short: false,
        payload: Payload::ANY,
    })
}

fn short() -> Column {
    Column::not_null(ColumnKind::Varlena {
        align: Align::Int,
        proven_short: true,
        payload: Payload {
            compressible: false,
            ..Payload::ANY
        },
    })
}

fn numeric() -> Column {
    Column::not_null(ColumnKind::Varlena {
        align: Align::Int,
        proven_short: false,
        payload: Payload::EVEN,
    })
}

fn nullable(column: Column) -> Column {
    Column::nullable(column.kind)
}

/// Padding bounds over every realization, NULLs included.
fn cmp(columns: &[Column], a: &[usize], b: &[usize]) -> Option<DiffBounds> {
    compare(Start::TABLE, columns, a, b, Nulls::Vary, Measure::Padding)
}

/// The joint walk alone, when it applies to the pair.
fn via_joint_walk(columns: &[Column], a: &[usize], b: &[usize], measure: Measure) -> Option<DiffBounds> {
    let pair = Pair::new(Start::TABLE, columns, a, b, Nulls::Vary, measure);
    let va = varlena_sequence(columns, a);
    if va != varlena_sequence(columns, b) {
        return None;
    }
    let schedule = Schedule::build(&pair, &va);
    (schedule.slots <= IN_FLIGHT_LIMIT).then(|| pair.joint_walk(&schedule, None))
}

/// The enumeration alone.
fn via_enumeration(columns: &[Column], a: &[usize], b: &[usize], measure: Measure) -> DiffBounds {
    let pair = Pair::new(Start::TABLE, columns, a, b, Nulls::Vary, measure);
    pair.enumerate(&varlena_sequence(columns, a), None)
}

fn pool() -> impl proptest::strategy::Strategy<Value = ColumnKind> {
    use proptest::prelude::*;
    prop_oneof![
        Just(ColumnKind::Fixed {
            len: 1,
            align: Align::Char
        }),
        Just(ColumnKind::Fixed {
            len: 2,
            align: Align::Short
        }),
        Just(ColumnKind::Fixed {
            len: 4,
            align: Align::Int
        }),
        Just(ColumnKind::Fixed {
            len: 6,
            align: Align::Int
        }),
        Just(ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }),
        Just(ColumnKind::Fixed {
            len: 12,
            align: Align::Double
        }),
        Just(ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::ANY
        }),
        Just(ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
            payload: Payload::ANY
        }),
        Just(ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true,
            payload: Payload {
                compressible: false,
                ..Payload::ANY
            }
        }),
        Just(ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true,
            payload: Payload::ANY
        }),
        Just(ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
            payload: Payload::array(8)
        }),
        Just(ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::array(4)
        }),
        Just(ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::EVEN
        }),
    ]
}

/// Columns from the pool, each nullable with probability one half.
fn columns(range: std::ops::RangeInclusive<usize>) -> impl proptest::strategy::Strategy<Value = Vec<Column>> {
    use proptest::prelude::*;
    proptest::collection::vec((pool(), any::<bool>()), range).prop_map(|cols| {
        cols.into_iter()
            .map(|(kind, nullable)| Column { kind, nullable })
            .collect()
    })
}

#[test]
fn aligned_slot_plus_char_tail_dominates_everything() {
    // (bigint, text, boolean): the text sits on a 4-boundary in every storage form and the
    // boolean never aligns, so total padding is zero in every realization.
    let kinds = [varlena(Align::Int), fixed(1, Align::Char), fixed(8, Align::Double)];
    let current = [0usize, 1, 2];
    let suggested = [2usize, 0, 1];
    let diff = cmp(&kinds, &current, &suggested).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    assert_eq!(diff.min, 0, "some realization pads the current order zero too");
    assert_eq!(diff.max, 7, "worst case: the bigint pads 7 behind the text");
}

#[test]
fn stranded_int_behind_text_is_dominated_by_fixed_first() {
    let kinds = [varlena(Align::Int), fixed(4, Align::Int)];
    let diff = cmp(&kinds, &[0, 1], &[1, 0]).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    assert_eq!((diff.min, diff.max), (0, 3));
}

#[test]
fn d_aligned_array_before_int_is_not_dominated() {
    // (float8[], int4) measures flat zero on disk for whole-float8 payloads; the swapped order
    // pays the array's long-form pad. Neither dominates: the swap wins short-form realizations.
    let kinds = [varlena(Align::Double), fixed(4, Align::Int)];
    let diff = cmp(&kinds, &[0, 1], &[1, 0]).unwrap();
    assert!(!diff.b_dominates(), "{diff:?}");
    assert!(!diff.a_dominates(), "{diff:?}");
}

#[test]
fn certainty_trade_is_incomparable() {
    // (timetz, timetz, text) pads a flat 4; interposing the text trades that for 0..=7.
    let kinds = [fixed(12, Align::Double), fixed(12, Align::Double), varlena(Align::Int)];
    let diff = cmp(&kinds, &[0, 1, 2], &[0, 2, 1]).unwrap();
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
    let diff = cmp(&kinds, &text_first, &array_first).unwrap();
    assert!(!diff.b_dominates(), "{diff:?}");
    assert!(!diff.a_dominates(), "{diff:?}");
    let bands = bands(Start::TABLE, &kinds, &text_first, &array_first, Measure::Padding).unwrap();
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
    let bands = bands(Start::TABLE, &kinds, &[0, 1, 2], &[0, 2, 1], Measure::Padding).unwrap();
    assert_eq!(bands.len(), 2, "only the long-capable text splits bands");
    assert!(bands.iter().all(|b| !b.long_form.contains(&1)));
}

#[test]
fn identical_orders_compare_equal() {
    let kinds = [fixed(4, Align::Int), varlena(Align::Int), varlena(Align::Double)];
    let order = [0usize, 1, 2];
    let diff = cmp(&kinds, &order, &order).unwrap();
    assert!(diff.equal(), "{diff:?}");
}

/// The joint residue walk and the exhaustive enumeration are two engines for the same
/// quantity; on same-sequence pairs they must agree exactly, NULL bits in flight included.
mod engine_agreement {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn joint_walk_matches_enumeration(
            cols in columns(2..=6),
            seed in any::<u64>(),
            row_size in any::<bool>(),
        ) {
            // Derive a second order that keeps the varlena sequence: rotate the fixed columns.
            let n = cols.len();
            let a: Vec<usize> = (0..n).collect();
            let fixed_positions: Vec<usize> = (0..n).filter(|&i| cols[i].kind.is_fixed()).collect();
            let mut b = a.clone();
            if fixed_positions.len() >= 2 {
                let rot = (seed as usize) % fixed_positions.len();
                for (k, &slot) in fixed_positions.iter().enumerate() {
                    b[slot] = fixed_positions[(k + rot) % fixed_positions.len()];
                }
            }
            let measure = if row_size { Measure::RowSize } else { Measure::Padding };
            let walked = via_joint_walk(&cols, &a, &b, measure).expect("few NULL bits");
            prop_assert_eq!(walked, via_enumeration(&cols, &a, &b, measure), "{:?} b {:?}", cols, b);
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
    let diff = cmp(&kinds, &interleaved, &grouped).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    assert_eq!((diff.min, diff.max), (0, 11));
    // (text, macaddr) vs (macaddr, text): incomparable, since long texts with friendly
    // residues favor text-first while short payloads favor macaddr-first.
    let k3 = [varlena(Align::Int), fixed(6, Align::Int)];
    let d3 = cmp(&k3, &[0, 1], &[1, 0]).unwrap();
    assert!(!d3.b_dominates() && !d3.a_dominates(), "{d3:?}");
    // (text, boolean, bigint): the aligned-slot order dominates, the classic fixed-first
    // order does not (it can lose 3 B/row to a friendly long text).
    let k4 = [varlena(Align::Int), fixed(1, Align::Char), fixed(8, Align::Double)];
    let aligned_slot = cmp(&k4, &[0, 1, 2], &[2, 0, 1]).unwrap();
    assert!(aligned_slot.b_dominates(), "{aligned_slot:?}");
    assert_eq!((aligned_slot.min, aligned_slot.max), (0, 7));
    let fixed_first = cmp(&k4, &[0, 1, 2], &[2, 1, 0]).unwrap();
    assert!(!fixed_first.b_dominates(), "{fixed_first:?}");
    assert_eq!(
        fixed_first.min, -3,
        "a friendly long text makes fixed-first strictly worse"
    );
}

/// A from-scratch oracle sharing no code with the engine: byte offsets instead of residues,
/// explicit alignment arithmetic, concrete payload lengths for all three storage forms (short
/// lengths the type stores, long 127..=134, and the 18-byte TOAST pointer), and a NULL that
/// stores nothing. A wrong storage convention inside `varlena_step` passes the engine-vs-engine
/// agreement test unnoticed; it cannot pass this.
mod independent_oracle {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Copy)]
    enum Value {
        Short(u64),
        Long(u64),
        Toast,
        Present,
        Null,
    }

    fn align_up(off: u64, align: u64) -> u64 {
        off.div_ceil(align) * align
    }

    /// (padding, data end) of one order under one realization.
    fn oracle_walk(columns: &[Column], order: &[usize], values: &[Value]) -> (u64, u64) {
        let mut off = 0u64;
        let mut total = 0u64;
        for &i in order {
            match (columns[i].kind, values[i]) {
                (_, Value::Null) => {}
                (ColumnKind::Fixed { len, align }, _) => {
                    let aligned = align_up(off, align.bytes());
                    total += aligned - off;
                    off = aligned + len;
                }
                (ColumnKind::Varlena { .. }, Value::Short(len)) => off += 1 + len,
                (ColumnKind::Varlena { .. }, Value::Toast) => off += 18,
                (ColumnKind::Varlena { align, .. }, Value::Long(len)) => {
                    let aligned = align_up(off, align.bytes());
                    total += aligned - off;
                    off = aligned + 4 + len;
                }
                (ColumnKind::Varlena { .. }, Value::Present) => unreachable!("varlenas take a form"),
            }
        }
        (total, off)
    }

    fn values(column: Column) -> Vec<Value> {
        let mut out = match column.kind {
            ColumnKind::Fixed { .. } => vec![Value::Present],
            ColumnKind::Varlena { payload, .. } => {
                // Concrete uncompressed lengths the type can store: 4 + k * step bytes.
                let step = u64::from(payload.step);
                let mut out: Vec<Value> = (0..8).map(|k| Value::Short((4 + k * step) % 16)).collect();
                if !column.kind.always_short() {
                    out.extend((127..135).map(Value::Long));
                    out.push(Value::Toast);
                }
                out
            }
        };
        if column.nullable {
            out.push(Value::Null);
        }
        out
    }

    /// Call `f` with every realization of `columns`, one value per column.
    fn each_realization(columns: &[Column], mut f: impl FnMut(&[Value])) {
        let domains: Vec<Vec<Value>> = columns.iter().map(|&c| values(c)).collect();
        let mut current = vec![Value::Present; columns.len()];
        let mut index = vec![0usize; columns.len()];
        loop {
            for (c, domain) in domains.iter().enumerate() {
                current[c] = domain[index[c]];
            }
            f(&current);
            let mut k = 0;
            while k < index.len() {
                index[k] += 1;
                if index[k] < domains[k].len() {
                    break;
                }
                index[k] = 0;
                k += 1;
            }
            if k == index.len() {
                break;
            }
        }
    }

    fn oracle_bounds(columns: &[Column], a: &[usize], b: &[usize], measure: Measure) -> DiffBounds {
        let mut bounds: Option<DiffBounds> = None;
        each_realization(columns, |current| {
            let size = |order: &[usize]| {
                let (padding, end) = oracle_walk(columns, order, current);
                match measure {
                    Measure::Padding => padding as i64,
                    Measure::RowSize => end.div_ceil(8) as i64 * 8,
                }
            };
            let d = size(a) - size(b);
            merge_into(&mut bounds, DiffBounds { min: d, max: d });
        });
        bounds.expect("at least one realization")
    }

    /// Bounds of what `order` of a block costs behind `prefix`: its own padding, or the bytes its
    /// rows spend over the same rows with the block unpadded.
    fn oracle_block_cost(prefix: &[Column], block: &[Column], order: &[usize], measure: Measure) -> (u64, u64) {
        let whole: Vec<Column> = prefix.iter().chain(block).copied().collect();
        let head: Vec<usize> = (0..prefix.len()).collect();
        let lifted: Vec<usize> = head
            .iter()
            .copied()
            .chain(order.iter().map(|&i| prefix.len() + i))
            .collect();
        let (mut lo, mut hi) = (u64::MAX, 0u64);
        each_realization(&whole, |current| {
            let (prefix_padding, _) = oracle_walk(&whole, &head, current);
            let (padding, end) = oracle_walk(&whole, &lifted, current);
            let block_padding = padding - prefix_padding;
            let cost = match measure {
                Measure::Padding => block_padding,
                Measure::RowSize => end.div_ceil(8) * 8 - (end - block_padding).div_ceil(8) * 8,
            };
            lo = lo.min(cost);
            hi = hi.max(cost);
        });
        (lo, hi)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn block_summaries_match_the_independent_oracle(
            prefix in columns(0..=3),
            block in columns(1..=3),
            row_size in any::<bool>(),
        ) {
            let varlenas = prefix.iter().chain(&block).filter(|c| !c.kind.is_fixed()).count();
            prop_assume!(varlenas <= 3);
            prop_assume!(!row_size || varlenas == 0);
            let measure = if row_size { Measure::RowSize } else { Measure::Padding };
            let order: Vec<usize> = (0..block.len()).rev().collect();
            let got = summary(Start::after(&prefix), &block, &order, Nulls::Vary, measure);
            let want = oracle_block_cost(&prefix, &block, &order, measure);
            prop_assert_eq!((got.min, got.max), want, "{:?} after {:?} {:?}", block, prefix, measure);
        }

        #[test]
        fn a_block_compares_like_the_whole_table(
            prefix in columns(0..=3),
            block in columns(1..=4),
            rotation in 0usize..24,
            row_size in any::<bool>(),
        ) {
            let varlenas = prefix.iter().chain(&block).filter(|c| !c.kind.is_fixed()).count();
            prop_assume!(varlenas <= 3);
            let n = block.len();
            let a: Vec<usize> = (0..n).collect();
            let b: Vec<usize> = (0..n).map(|i| (i + rotation) % n).collect();
            let whole: Vec<Column> = prefix.iter().chain(&block).copied().collect();
            let lift = |order: &[usize]| -> Vec<usize> {
                (0..prefix.len()).chain(order.iter().map(|&i| prefix.len() + i)).collect()
            };
            let measure = if row_size { Measure::RowSize } else { Measure::Padding };
            for nulls in [Nulls::Vary, Nulls::Stored] {
                let scoped = compare(Start::after(&prefix), &block, &a, &b, nulls, measure);
                let full = compare(Start::TABLE, &whole, &lift(&a), &lift(&b), nulls, measure);
                prop_assert_eq!(scoped, full, "{:?} after {:?} b {:?} {:?}", block, prefix, b, nulls);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn engine_bounds_match_the_independent_oracle(
            cols in columns(2..=5).prop_filter("at most 3 varlenas keeps the oracle enumerable", |cols| {
                cols.iter().filter(|c| !c.kind.is_fixed()).count() <= 3
            }),
            rotation in 0usize..120,
            row_size in any::<bool>(),
        ) {
            let n = cols.len();
            let a: Vec<usize> = (0..n).collect();
            let b: Vec<usize> = (0..n).map(|i| (i + 1 + rotation % n.max(1)) % n).collect();
            let measure = if row_size { Measure::RowSize } else { Measure::Padding };
            let engine = compare(Start::TABLE, &cols, &a, &b, Nulls::Vary, measure).expect("within budget");
            let oracle = oracle_bounds(&cols, &a, &b, measure);
            prop_assert_eq!(engine, oracle, "{:?} b {:?} {:?}", cols, b, measure);
        }
    }
}

#[test]
fn a_nullable_filler_is_no_longer_a_proven_fix() {
    // (n int2 NULL, i int4, s int2) pads 2 when n is stored and 0 when it is NULL; (s, n, i)
    // pads 0 when n is stored and 2 when it is NULL. Without NULLs the second order dominates;
    // with them neither does, which is what the frontier's no-NULL row reports.
    let columns = [
        nullable(fixed(2, Align::Short)),
        fixed(4, Align::Int),
        fixed(2, Align::Short),
    ];
    let with_nulls = cmp(&columns, &[0, 1, 2], &[2, 0, 1]).unwrap();
    assert!(!with_nulls.b_dominates() && !with_nulls.a_dominates(), "{with_nulls:?}");
    assert_eq!((with_nulls.min, with_nulls.max), (-2, 2));
    let stored = compare(
        Start::TABLE,
        &columns,
        &[0, 1, 2],
        &[2, 0, 1],
        Nulls::Stored,
        Measure::Padding,
    )
    .unwrap();
    assert!(stored.b_dominates(), "{stored:?}");
    // The regular desc-aligned order pads zero under every NULL pattern and dominates both.
    let sorted = cmp(&columns, &[0, 1, 2], &[1, 0, 2]).unwrap();
    assert!(sorted.b_dominates(), "{sorted:?}");
}

#[test]
fn null_bits_cross_varlenas_in_the_joint_walk() {
    // A nullable int4 moved from behind a text to the front: the walk must carry its NULL bit
    // across the text, and the result must match the enumeration exactly.
    let columns = [
        varlena(Align::Int),
        nullable(fixed(4, Align::Int)),
        fixed(8, Align::Double),
        varlena(Align::Int),
        nullable(fixed(2, Align::Short)),
    ];
    let a = [0usize, 1, 2, 3, 4];
    let b = [2usize, 1, 4, 0, 3];
    let pair = Pair::new(Start::TABLE, &columns, &a, &b, Nulls::Vary, Measure::Padding);
    let schedule = Schedule::build(&pair, &varlena_sequence(&columns, &a));
    assert!(schedule.slots >= 1, "a NULL bit must be in flight");
    let walked = via_joint_walk(&columns, &a, &b, Measure::Padding).unwrap();
    assert_eq!(walked, via_enumeration(&columns, &a, &b, Measure::Padding));
    assert_eq!((walked.min, walked.max), (-2, 8));
}

#[test]
fn a_null_array_is_a_step_its_values_never_take() {
    // (i int4, a float8[], s int2) against (a, s, i). A stored float8[] advances 5 bytes short
    // (payload 4 mod 8), 18 as a TOAST pointer, or aligned in the long form, and under those
    // forms the second order never pads more. A NULL array advances 0 bytes, which no stored
    // value does: the first order then packs i and s flush while the second pads 2 before i.
    let array = Column::not_null(ColumnKind::Varlena {
        align: Align::Double,
        proven_short: false,
        payload: Payload::array(8),
    });
    let stored = [fixed(4, Align::Int), array, fixed(2, Align::Short)];
    let diff = cmp(&stored, &[0, 1, 2], &[1, 2, 0]).unwrap();
    assert!(diff.b_dominates(), "NOT NULL: {diff:?}");
    let with_null = [fixed(4, Align::Int), nullable(array), fixed(2, Align::Short)];
    let diff = cmp(&with_null, &[0, 1, 2], &[1, 2, 0]).unwrap();
    assert_eq!((diff.min, diff.max), (-2, 4), "nullable: the NULL row loses 2");
    assert!(nullable(array).null_varies());
    assert!(
        nullable(numeric()).null_varies(),
        "short numerics advance an odd number of bytes"
    );
    assert!(
        !nullable(varlena(Align::Int)).null_varies(),
        "a 7-byte text advances 8, like a NULL"
    );
}

#[test]
fn row_size_decides_where_padding_does_not() {
    // (c0 timetz NULL, c1 int2, c2 timetz NULL) against (c0, c2, c1). Measured on PostgreSQL 16:
    // rows without NULLs 56 vs 56 B, c0 NULL 48 vs 40 B, c2 NULL 40 vs 40 B, both NULL 32 vs 32
    // B. In padding the first order wins the rows without NULLs; in row size it never does.
    let columns = [
        nullable(fixed(12, Align::Double)),
        fixed(2, Align::Short),
        nullable(fixed(12, Align::Double)),
    ];
    let a = [0usize, 1, 2];
    let b = [0usize, 2, 1];
    let padding = cmp(&columns, &a, &b).unwrap();
    assert!(!padding.b_dominates() && !padding.a_dominates(), "{padding:?}");
    let rows = compare(Start::TABLE, &columns, &a, &b, Nulls::Vary, Measure::RowSize).unwrap();
    assert!(rows.b_dominates(), "{rows:?}");
    assert_eq!((rows.min, rows.max), (0, 8));
    let stored = compare(Start::TABLE, &columns, &a, &b, Nulls::Stored, Measure::RowSize).unwrap();
    assert!(stored.equal(), "rows without NULLs tie: {stored:?}");
}

#[test]
fn an_order_that_never_pads_costs_nothing_in_either_measure() {
    // 24 nullable regular columns: the alignment-sorted order pads zero in every NULL pattern,
    // so its summary proves dominance with no pair comparison, while the pair itself carries
    // more NULL bits than the joint walk holds and more realizations than the budget.
    let kinds = [
        fixed(1, Align::Char),
        fixed(2, Align::Short),
        fixed(4, Align::Int),
        fixed(8, Align::Double),
    ];
    let columns: Vec<Column> = (0..24).map(|i| nullable(kinds[i % 4])).collect();
    let written: Vec<usize> = (0..24).collect();
    let mut sorted = written.clone();
    sorted.sort_by_key(|&i| std::cmp::Reverse(columns[i].kind.align()));
    for measure in [Measure::Padding, Measure::RowSize] {
        let cost = summary(Start::TABLE, &columns, &sorted, Nulls::Vary, measure);
        assert_eq!((cost.min, cost.max), (0, 0), "{measure:?}");
        let current = summary(Start::TABLE, &columns, &written, Nulls::Vary, measure);
        assert!(current.max > 0, "{measure:?}: {current:?}");
    }
    assert_eq!(
        compare(Start::TABLE, &columns, &written, &sorted, Nulls::Vary, Measure::RowSize),
        None
    );
}

#[test]
fn payload_residues_follow_the_storage_format() {
    assert_eq!(Payload::ANY.residues(), 0xFF);
    assert_eq!(Payload::EVEN.residues(), 0b0101_0101);
    assert_eq!(Payload::array(8).residues(), 0b0001_0000, "float8[]: payload 4 mod 8");
    assert_eq!(Payload::array(16).residues(), 0b0001_0000, "timetz[] strides 16");
    assert_eq!(Payload::array(4).residues(), 0b0001_0001, "int4[], text[]: 0 or 4");
    assert_eq!(Payload::array(2).residues(), 0b0101_0101);
    assert_eq!(Payload::array(1).residues(), 0xFF, "bool[] reaches every length");
}

#[test]
fn pinned_array_residues_decide_the_closure_review_pair() {
    // (smallint, float8[], float8[], macaddr): a short uncompressed float8[] always has payload
    // 4 mod 8, and under that model (macaddr, smallint, a2, a1) dominates. Measured on
    // PostgreSQL 16 over 3,000 rows: never worse, 5.803 to 1.454 B/row.
    let array = Column::not_null(ColumnKind::Varlena {
        align: Align::Double,
        proven_short: false,
        payload: Payload::array(8),
    });
    let kinds = [fixed(2, Align::Short), array, array, fixed(6, Align::Int)];
    let diff = cmp(&kinds, &[0, 1, 2, 3], &[3, 0, 2, 1]).unwrap();
    assert!(diff.b_dominates(), "{diff:?}");
    let any = varlena(Align::Double);
    let unpinned = [fixed(2, Align::Short), any, any, fixed(6, Align::Int)];
    let loose = cmp(&unpinned, &[0, 1, 2, 3], &[3, 0, 2, 1]).unwrap();
    assert!(
        !loose.b_dominates(),
        "any-residue arrays leave the pair a frontier: {loose:?}"
    );
}

mod summaries {
    use super::*;
    use crate::layout::walk;
    use proptest::prelude::*;

    /// One column's value under brute force: a varlena's (long form, payload) or a NULL, and
    /// whether it belongs to the uniform short sub-distribution.
    type Choice = (Option<(bool, u64)>, bool);

    /// Bounds and short-form mean of one order by enumerating every realization outright, and
    /// the bounds in rows that store every column.
    fn brute(columns: &[Column], order: &[usize]) -> ((u64, u64, u64), (u64, u64)) {
        let options: Vec<Vec<Choice>> = columns
            .iter()
            .map(|&column| {
                let mut out = Vec::new();
                match column.kind {
                    ColumnKind::Fixed { .. } => out.push((None, true)),
                    ColumnKind::Varlena { payload, .. } => {
                        let domain = Domain::of(column, Nulls::Stored);
                        for p in 0..MAXALIGN {
                            for form_long in [false, true] {
                                if domain.allows(form_long, p) {
                                    let short_uniform = !form_long && payload.residues() & (1 << p) != 0;
                                    out.push((Some((form_long, p)), short_uniform));
                                }
                            }
                        }
                    }
                }
                if column.nullable {
                    // NULL: no bytes, no pad, outside the uniform short sub-distribution.
                    out.push((Some((false, u64::MAX)), false));
                }
                out
            })
            .collect();
        let (mut lo, mut hi, mut sum, mut count) = (u64::MAX, 0u64, 0u64, 0u64);
        let (mut stored_lo, mut stored_hi) = (u64::MAX, 0u64);
        let mut index = vec![0usize; columns.len()];
        loop {
            let mut residue = 0u64;
            let mut total = 0u64;
            let mut uniform = true;
            let mut every_stored = true;
            for &i in order {
                let (choice, short_uniform) = options[i][index[i]];
                uniform &= short_uniform;
                match (columns[i].kind, choice) {
                    (_, Some((_, u64::MAX))) => every_stored = false,
                    (ColumnKind::Fixed { len, align }, _) => {
                        let p = pad(residue, align.bytes());
                        total += p;
                        residue = (residue + p + len) % MAXALIGN;
                    }
                    (ColumnKind::Varlena { align, .. }, Some((form_long, payload))) => {
                        let (p, next) = varlena_step(residue, align.bytes(), form_long, payload);
                        total += p;
                        residue = next;
                    }
                    (ColumnKind::Varlena { .. }, None) => unreachable!(),
                }
            }
            lo = lo.min(total);
            hi = hi.max(total);
            if every_stored {
                stored_lo = stored_lo.min(total);
                stored_hi = stored_hi.max(total);
            }
            if uniform {
                sum += total;
                count += 1;
            }
            let mut k = 0;
            while k < index.len() {
                index[k] += 1;
                if index[k] < options[k].len() {
                    break;
                }
                index[k] = 0;
                k += 1;
            }
            if k == index.len() {
                break;
            }
        }
        ((lo, hi, sum * MAXALIGN / count), (stored_lo, stored_hi))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn summary_matches_brute_force_and_the_walk(
            cols in columns(1..=6).prop_filter("at most 3 varlenas keeps brute force small", |cols| {
                cols.iter().filter(|c| !c.kind.is_fixed()).count() <= 3
            })
        ) {
            let order: Vec<usize> = (0..cols.len()).collect();
            let got = summary(Start::TABLE, &cols, &order, Nulls::Vary, Measure::Padding);
            let stored = summary(Start::TABLE, &cols, &order, Nulls::Stored, Measure::Padding);
            let ((lo, hi, mean), (stored_lo, stored_hi)) = brute(&cols, &order);
            prop_assert_eq!((got.min, got.max, got.short_mean_eighths), (lo, hi, mean), "{:?}", cols);
            prop_assert_eq!((stored.min, stored.max), (stored_lo, stored_hi), "{:?}", cols);
            prop_assert_eq!(stored.short_mean_eighths, mean, "the mean is over stored rows: {:?}", cols);
            let narrowed = cols
                .iter()
                .any(|c| matches!(c.kind, ColumnKind::Varlena { payload, .. } if payload.step > 1));
            if !narrowed {
                let kinds: Vec<ColumnKind> = cols.iter().map(|c| c.kind).collect();
                let w = walk(&kinds);
                prop_assert_eq!(
                    (stored.min, stored.max, stored.short_mean_eighths),
                    (w.padding_min(), w.padding_max(), w.expected_padding_eighths()),
                    "without narrowing the stored-row summary is the walk's: {:?}", cols
                );
            }
        }

        #[test]
        fn row_size_summary_is_the_cost_over_an_unpadded_row(
            cols in columns(1..=7).prop_map(|cols| cols.into_iter().filter(|c| c.kind.is_fixed()).collect::<Vec<_>>())
        ) {
            let order: Vec<usize> = (0..cols.len()).collect();
            let got = summary(Start::TABLE, &cols, &order, Nulls::Vary, Measure::RowSize);
            let (mut lo, mut hi) = (u64::MAX, 0u64);
            let mut stored_cost = 0;
            for pattern in 0u32..(1 << cols.len()) {
                if (0..cols.len()).any(|c| pattern & (1 << c) != 0 && !cols[c].nullable) {
                    continue;
                }
                let (mut off, mut data) = (0u64, 0u64);
                for (c, column) in cols.iter().enumerate() {
                    let ColumnKind::Fixed { len, align } = column.kind else { unreachable!() };
                    if pattern & (1 << c) == 0 {
                        off = off.div_ceil(align.bytes()) * align.bytes() + len;
                        data += len;
                    }
                }
                let cost = off.div_ceil(8) * 8 - data.div_ceil(8) * 8;
                lo = lo.min(cost);
                hi = hi.max(cost);
                if pattern == 0 {
                    stored_cost = cost;
                }
            }
            prop_assert_eq!((got.min, got.max, got.short_mean_eighths), (lo, hi, stored_cost * 8), "{:?}", cols);
        }
    }
}

#[test]
fn a_column_written_only_under_plain_holds_no_toast_pointer() {
    let plain = Column::not_null(ColumnKind::Varlena {
        align: Align::Int,
        proven_short: false,
        payload: Payload {
            toastable: false,
            ..Payload::EVEN
        },
    });
    let domain = Domain::of(plain, Nulls::Vary);
    assert_eq!(domain.short, Payload::EVEN.residues(), "no 18-byte pointer");
    assert_eq!(
        Domain::of(numeric(), Nulls::Vary).short,
        Payload::EVEN.residues() | TOAST_RESIDUE
    );
}
