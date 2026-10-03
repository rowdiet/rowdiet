//! An independent brute-force oracle for the decision policy, sharing no code with the engine.
//!
//! It walks concrete byte offsets for every permutation of every column (no class collapsing)
//! and every realization of every varlena: a short payload stored unaligned behind a 1-byte
//! header, a long payload of 128..=135 bytes aligned behind a 4-byte header, and an 18-byte TOAST
//! pointer. Short payload lengths follow what PostgreSQL stores for the type (arrays pad their
//! elements, numeric digits are 2 bytes), stated here from the storage format and not taken from
//! the engine. A `varchar(n)` holding more than 20 bytes can be compressed in line, aligned, by a
//! wide row's toaster; one of at most 20 bytes is always short. A nullable column may also hold
//! NULL, which stores nothing at all. The tool is driven through the public API only, and every
//! claim it prints is judged against the oracle: a finding must dominate with the reported saving
//! range, an exhaustive clean verdict must have no dominating permutation, and a frontier must
//! never be an order that dominates. Tables of fixed-width columns only are judged in row size
//! (the data end rounded up to 8 bytes, since both orders of a row share its header), which is
//! what the exact tier reports; tables with a varlena are judged in padding.

use proptest::prelude::*;
use rowdiet_core::report::{BandWinner, DominanceScope};
use rowdiet_core::{Config, SqlSource, TableReport, analyze_sources};

#[derive(Clone, Copy, Debug)]
enum Kind {
    Fixed {
        len: u64,
        align: u64,
    },
    Var {
        align: u64,
        short_only: bool,
        short_residues: &'static [u64],
    },
}

const ANY: &[u64] = &[0, 1, 2, 3, 4, 5, 6, 7];

fn kind_of(ty: &str) -> Kind {
    let fixed = |len, align| Kind::Fixed { len, align };
    let var = |align, short_residues| Kind::Var {
        align,
        short_only: false,
        short_residues,
    };
    match ty {
        "text" | "jsonb" => var(4, ANY),
        "varchar(8)" => var(4, ANY),
        "varchar(5)" => Kind::Var {
            align: 4,
            short_only: true,
            short_residues: ANY,
        },
        // A MAXALIGNed array header plus 8-byte elements: 12 + 8n bytes after the 4-byte header.
        "float8[]" => var(8, &[4]),
        // 4-byte elements, or 4-byte-aligned text elements: 12 + 4n.
        "int4[]" | "text[]" => var(4, &[0, 4]),
        // A 2- or 4-byte header plus 2-byte digits.
        "numeric" => var(4, &[0, 2, 4, 6]),
        "smallint" => fixed(2, 2),
        "integer" => fixed(4, 4),
        "boolean" => fixed(1, 1),
        "bigint" => fixed(8, 8),
        "timetz" => fixed(12, 8),
        "macaddr" => fixed(6, 4),
        "uuid" => fixed(16, 1),
        other => panic!("no oracle kind for {other}"),
    }
}

#[derive(Clone, Copy, Debug)]
enum Value {
    Short(u64),
    Long(u64),
    Toast,
    Stored,
    Null,
}

/// Types whose stored lengths do not depend on the database encoding: the tool may claim that
/// nothing dominates only over these.
fn verified(ty: &str) -> bool {
    !ty.starts_with("varchar")
}

/// One column: its kind and whether it may hold NULL.
#[derive(Clone, Copy, Debug)]
struct Col {
    kind: Kind,
    nullable: bool,
}

/// The values a column takes: `band` holds a varlena to its long (true) or short, TOAST, and
/// NULL (false) values.
fn values(col: Col, band: Option<bool>) -> Vec<Value> {
    let mut out = Vec::new();
    match col.kind {
        Kind::Fixed { .. } => out.push(Value::Stored),
        Kind::Var {
            short_only,
            short_residues,
            ..
        } => {
            if band != Some(true) {
                out.extend(short_residues.iter().map(|&r| Value::Short(8 + r)));
                if !short_only {
                    out.push(Value::Toast);
                }
            }
            if !short_only && band != Some(false) {
                out.extend((128..136).map(Value::Long));
            }
        }
    }
    if col.nullable && band != Some(true) {
        out.push(Value::Null);
    }
    out
}

fn align_up(offset: u64, align: u64) -> u64 {
    offset.div_ceil(align) * align
}

/// What a claim is judged in: padding, or the row size the exact tier reports.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Measure {
    Padding,
    RowSize,
}

fn cost(cols: &[Col], order: &[usize], realization: &[Value], measure: Measure) -> i64 {
    let mut offset = 0u64;
    let mut total = 0u64;
    for &column in order {
        match (cols[column].kind, realization[column]) {
            (_, Value::Null) => {}
            (Kind::Fixed { len, align }, _) => {
                let start = align_up(offset, align);
                total += start - offset;
                offset = start + len;
            }
            (Kind::Var { .. }, Value::Short(len)) => offset += 1 + len,
            (Kind::Var { .. }, Value::Toast) => offset += 18,
            (Kind::Var { align, .. }, Value::Long(len)) => {
                let start = align_up(offset, align);
                total += start - offset;
                offset = start + 4 + len;
            }
            (Kind::Var { .. }, Value::Stored) => unreachable!("a varlena takes a form"),
        }
    }
    match measure {
        Measure::Padding => total as i64,
        Measure::RowSize => align_up(offset, 8) as i64,
    }
}

fn padding(cols: &[Col], order: &[usize], realization: &[Value]) -> i64 {
    cost(cols, order, realization, Measure::Padding)
}

/// Visit every realization, one value per column, until `f` returns false. `pins` holds a
/// varlena to its long (true) or short, TOAST, and NULL (false) values.
fn each_realization(cols: &[Col], pins: &[Option<bool>], mut f: impl FnMut(&[Value]) -> bool) {
    let choices: Vec<Vec<Value>> = cols.iter().zip(pins).map(|(&c, &pin)| values(c, pin)).collect();
    let varied: Vec<usize> = (0..cols.len()).filter(|&c| choices[c].len() > 1).collect();
    let mut index = vec![0usize; cols.len()];
    let mut current: Vec<Value> = choices.iter().map(|c| c[0]).collect();
    loop {
        if !f(&current) {
            return;
        }
        let mut k = 0;
        while k < varied.len() {
            let c = varied[k];
            index[c] += 1;
            if index[c] < choices[c].len() {
                current[c] = choices[c][index[c]];
                break;
            }
            index[c] = 0;
            current[c] = choices[c][0];
            k += 1;
        }
        if k == varied.len() {
            return;
        }
    }
}

/// `cost(a) - cost(b)` bounds over every realization the pins and `keep` allow.
fn bounds_where(
    cols: &[Col],
    pins: &[Option<bool>],
    a: &[usize],
    b: &[usize],
    measure: Measure,
    keep: impl Fn(&[Value]) -> bool,
) -> (i64, i64) {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    each_realization(cols, pins, |r| {
        if keep(r) {
            let d = cost(cols, a, r, measure) - cost(cols, b, r, measure);
            lo = lo.min(d);
            hi = hi.max(d);
        }
        true
    });
    (lo, hi)
}

fn bounds(cols: &[Col], pins: &[Option<bool>], a: &[usize], b: &[usize], measure: Measure) -> (i64, i64) {
    bounds_where(cols, pins, a, b, measure, |_| true)
}

/// Padding bounds of one order over every realization, and over the rows without NULLs.
fn order_bounds(cols: &[Col], order: &[usize]) -> ((i64, i64), (i64, i64)) {
    let pins = vec![None; cols.len()];
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    let (mut stored_lo, mut stored_hi) = (i64::MAX, i64::MIN);
    each_realization(cols, &pins, |r| {
        let p = padding(cols, order, r);
        lo = lo.min(p);
        hi = hi.max(p);
        if !r.iter().any(|v| matches!(v, Value::Null)) {
            stored_lo = stored_lo.min(p);
            stored_hi = stored_hi.max(p);
        }
        true
    });
    ((lo, hi), (stored_lo, stored_hi))
}

/// True when `candidate` is never worse than `current` and better somewhere; stops at the first
/// realization where it is worse.
fn dominates(cols: &[Col], current: &[usize], candidate: &[usize], measure: Measure) -> bool {
    let pins = vec![None; cols.len()];
    let mut worse = false;
    let mut better = false;
    each_realization(cols, &pins, |r| {
        let d = cost(cols, current, r, measure) - cost(cols, candidate, r, measure);
        worse = d < 0;
        better |= d > 0;
        !worse
    });
    better && !worse
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn go(prefix: &mut Vec<usize>, used: &mut [bool], out: &mut Vec<Vec<usize>>) {
        if prefix.len() == used.len() {
            out.push(prefix.clone());
            return;
        }
        for i in 0..used.len() {
            if !used[i] {
                used[i] = true;
                prefix.push(i);
                go(prefix, used, out);
                prefix.pop();
                used[i] = false;
            }
        }
    }
    let mut out = Vec::new();
    go(&mut Vec::new(), &mut vec![false; n], &mut out);
    out
}

/// One permutation per arrangement that tells columns apart: two NOT NULL fixed columns of one
/// type are the same bytes, so swapping them changes nothing.
fn distinct_orders(types: &[&str], cols: &[Col]) -> Vec<Vec<usize>> {
    let mut seen = std::collections::BTreeSet::new();
    permutations(cols.len())
        .into_iter()
        .filter(|order| {
            let key: Vec<String> = order
                .iter()
                .map(|&c| match cols[c].kind {
                    Kind::Fixed { .. } if !cols[c].nullable => types[c].to_string(),
                    _ => format!("#{c}"),
                })
                .collect();
            seen.insert(key)
        })
        .collect()
}

/// A type spelled with a trailing `?` is nullable.
fn split(spec: &str) -> (&str, bool) {
    match spec.strip_suffix('?') {
        Some(ty) => (ty, true),
        None => (spec, false),
    }
}

fn analyze(specs: &[&str]) -> TableReport {
    let cols: Vec<String> = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let (ty, nullable) = split(spec);
            let constraint = if nullable { "" } else { " NOT NULL" };
            format!("c{i} {ty}{constraint}")
        })
        .collect();
    let sql = format!("CREATE TABLE o ({});", cols.join(", "));
    let analysis = analyze_sources(&[SqlSource::new("V1__o.sql", &sql)], &Config::default());
    analysis.tables.into_iter().next().expect("one table")
}

fn indices(names: &[String]) -> Vec<usize> {
    names
        .iter()
        .map(|n| n.trim_start_matches('c').parse().expect("cN column name"))
        .collect()
}

/// Judge every claim the tool prints for `specs` against the oracle.
fn check(specs: &[&str]) -> Result<(), String> {
    let types: Vec<&str> = specs.iter().map(|s| split(s).0).collect();
    let cols: Vec<Col> = specs
        .iter()
        .map(|s| {
            let (ty, nullable) = split(s);
            Col {
                kind: kind_of(ty),
                nullable,
            }
        })
        .collect();
    let t = analyze(specs);
    let n = cols.len();
    let identity: Vec<usize> = (0..n).collect();
    let free = vec![None; n];
    let table = format!("({})", specs.join(", "));
    let all_fixed = cols.iter().all(|c| matches!(c.kind, Kind::Fixed { .. }));
    let any_nullable = cols.iter().any(|c| c.nullable);
    // Row size at the exact tier, padding elsewhere.
    let measure = if all_fixed { Measure::RowSize } else { Measure::Padding };
    let ((cur_lo, cur_hi), (stored_lo, stored_hi)) = order_bounds(&cols, &identity);
    if (t.current.padding_min as i64, t.current.padding_max as i64) != (cur_lo, cur_hi) {
        return Err(format!(
            "{table}: current bounds [{}, {}] vs the oracle's [{cur_lo}, {cur_hi}]",
            t.current.padding_min, t.current.padding_max
        ));
    }
    let reported_stored = t
        .current
        .without_nulls
        .map_or((t.current.padding_min, t.current.padding_max), |b| (b.min, b.max));
    if (reported_stored.0 as i64, reported_stored.1 as i64) != (stored_lo, stored_hi) {
        return Err(format!(
            "{table}: bounds without NULLs {reported_stored:?} vs the oracle's [{stored_lo}, {stored_hi}]"
        ));
    }
    let all_verified = types.iter().all(|t| verified(t));
    if (t.dominance_search == DominanceScope::Superset) == all_verified
        && t.dominance_search != DominanceScope::Budgeted
    {
        return Err(format!(
            "{table}: verdict {:?} while every type verified is {all_verified}",
            t.dominance_search
        ));
    }
    let suggested = indices(&t.suggested_order);
    if all_fixed && !any_nullable {
        // One realization: the headline is the row-size delta of the suggested order.
        let (lo, hi) = bounds(&cols, &free, &identity, &suggested, measure);
        if lo != hi || t.avoidable_bytes_per_row != hi as f64 {
            return Err(format!(
                "{table}: headline {} vs the oracle's row-size delta {lo}-{hi}",
                t.avoidable_bytes_per_row
            ));
        }
    } else if t.avoidable_bytes_per_row > 0.0 {
        if !dominates(&cols, &identity, &suggested, measure) {
            return Err(format!("{table}: the suggested {suggested:?} does not dominate"));
        }
        let (lo, hi) = bounds(&cols, &free, &identity, &suggested, measure);
        let saving = t
            .dominance_saving
            .ok_or_else(|| format!("{table}: a finding without a saving range"))?;
        if (saving.min as i64, saving.max as i64) != (lo, hi) {
            return Err(format!(
                "{table}: saving {}-{} but the oracle measures {lo}-{hi}",
                saving.min, saving.max
            ));
        }
        if t.avoidable_bytes_per_row != hi as f64 {
            return Err(format!(
                "{table}: headline {} vs proven maximum {hi}",
                t.avoidable_bytes_per_row
            ));
        }
        let ((sug_lo, sug_hi), _) = order_bounds(&cols, &suggested);
        if (t.suggested.padding_min as i64, t.suggested.padding_max as i64) != (sug_lo, sug_hi) {
            return Err(format!(
                "{table}: suggested bounds [{}, {}] vs the oracle's [{sug_lo}, {sug_hi}]",
                t.suggested.padding_min, t.suggested.padding_max
            ));
        }
    }
    if t.avoidable_bytes_per_row == 0.0
        && t.dominance_search == DominanceScope::Exhaustive
        && let Some(order) = distinct_orders(&types, &cols)
            .into_iter()
            .find(|order| *order != identity && dominates(&cols, &identity, order, measure))
    {
        return Err(format!(
            "{table}: \"no dominating reorder exists\", but {order:?} dominates"
        ));
    }
    if let Some(frontier) = &t.frontier {
        let alternative = indices(&frontier.order);
        if dominates(&cols, &identity, &alternative, measure) {
            return Err(format!(
                "{table}: the frontier {alternative:?} dominates the current order"
            ));
        }
        let (_, hi) = bounds(&cols, &free, &identity, &alternative, measure);
        if hi <= 0 {
            return Err(format!("{table}: the frontier {alternative:?} never wins"));
        }
        if let Some(rows) = &frontier.without_nulls {
            let no_null = |r: &[Value]| !r.iter().any(|v| matches!(v, Value::Null));
            let (lo, hi) = bounds_where(&cols, &free, &identity, &alternative, measure, no_null);
            if (rows.min_saving, rows.max_saving) != (lo, hi) {
                return Err(format!(
                    "{table}: rows without NULLs [{}, {}] vs oracle [{lo}, {hi}]",
                    rows.min_saving, rows.max_saving
                ));
            }
        }
        for band in &frontier.bands {
            let long: Vec<usize> = indices(&band.long_form);
            let pins: Vec<Option<bool>> = (0..n)
                .map(|c| match cols[c].kind {
                    Kind::Var { short_only: false, .. } => Some(long.contains(&c)),
                    _ => None,
                })
                .collect();
            let (lo, hi) = bounds(&cols, &pins, &identity, &alternative, measure);
            if (band.min_saving, band.max_saving) != (lo, hi) {
                return Err(format!(
                    "{table}: band {long:?} [{}, {}] vs oracle [{lo}, {hi}]",
                    band.min_saving, band.max_saving
                ));
            }
            let winner = match (lo, hi) {
                (0, 0) => BandWinner::Tie,
                (lo, hi) if lo >= 0 && hi > 0 => BandWinner::Alternative,
                (lo, hi) if hi <= 0 && lo < 0 => BandWinner::Current,
                _ => BandWinner::Mixed,
            };
            if band.winner != winner {
                return Err(format!(
                    "{table}: band {long:?} labeled {:?}, oracle {winner:?}",
                    band.winner
                ));
            }
        }
    }
    Ok(())
}

const VARLENAS: [&str; 8] = [
    "text",
    "jsonb",
    "float8[]",
    "varchar(8)",
    "varchar(5)",
    "numeric",
    "int4[]",
    "text[]",
];
const FIXED: [&str; 7] = ["smallint", "integer", "boolean", "bigint", "timetz", "macaddr", "uuid"];

/// Tables of 3 to 5 columns with `varlenas` of them drawn from the varlena pool; with `nulls`,
/// each column is nullable with probability one half.
fn table(varlenas: std::ops::RangeInclusive<usize>, nulls: bool) -> impl Strategy<Value = Vec<String>> {
    (3usize..=5, varlenas).prop_flat_map(move |(n, v)| {
        let v = v.min(n);
        (
            proptest::collection::vec(proptest::sample::select(&VARLENAS[..]), v),
            proptest::collection::vec(proptest::sample::select(&FIXED[..]), n - v),
            proptest::collection::vec(any::<bool>(), n),
            any::<proptest::sample::Index>(),
        )
            .prop_map(move |(mut var, fixed, nullable, shuffle)| {
                var.extend(fixed);
                let len = var.len();
                var.rotate_left(shuffle.index(len));
                var.iter()
                    .zip(nullable)
                    .map(|(ty, null)| {
                        if nulls && null {
                            format!("{ty}?")
                        } else {
                            ty.to_string()
                        }
                    })
                    .collect()
            })
    })
}

fn check_owned(specs: &[String]) -> Result<(), TestCaseError> {
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
    check(&specs).map_err(TestCaseError::fail)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn every_claim_holds_against_the_oracle(specs in table(1..=3, false)) {
        check_owned(&specs)?;
    }

    #[test]
    fn every_claim_holds_over_null_patterns(specs in table(1..=3, true)) {
        check_owned(&specs)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]
    #[test]
    fn every_exact_tier_claim_holds_in_row_size(specs in table(0..=0, true)) {
        check_owned(&specs)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn every_claim_holds_with_four_or_five_varlenas(specs in table(4..=5, false)) {
        check_owned(&specs)?;
    }
}

#[test]
fn closure_review_repros_hold_against_the_oracle() {
    for types in [
        vec!["bigint", "varchar(5)", "varchar(5)", "varchar(5)", "varchar(5)", "text"],
        vec!["timetz", "varchar(8)", "macaddr", "text"],
        vec![
            "varchar(5)",
            "text",
            "jsonb",
            "varchar(8)",
            "macaddr",
            "smallint",
            "varchar(5)",
        ],
        vec![
            "macaddr",
            "jsonb",
            "varchar(8)",
            "text",
            "integer",
            "float8[]",
            "bigint",
        ],
        vec![
            "varchar(5)",
            "text",
            "integer",
            "varchar(8)",
            "integer",
            "varchar(5)",
            "varchar(8)",
        ],
        vec!["smallint", "float8[]", "float8[]", "macaddr"],
        vec!["text", "macaddr", "text", "macaddr"],
        vec!["text", "bigint", "macaddr"],
        vec!["varchar(8)", "text", "float8[]", "text", "float8[]"],
        vec!["bigint", "text", "float8[]"],
        vec!["timetz", "timetz", "text"],
    ] {
        check(&types).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn stack_review_repros_hold_against_the_oracle() {
    for specs in [
        // The exact tier judged in row size: (c0, c2, c1) wins only there.
        vec!["timetz?", "smallint", "timetz?"],
        // A NULL array advances 0 bytes, which no stored array does.
        vec!["integer", "float8[]?", "smallint"],
        vec!["integer", "numeric?", "smallint", "text?"],
        // A nullable filler, and waste only NULL rows carry.
        vec!["macaddr?", "text", "smallint"],
        vec!["smallint", "smallint?", "integer", "text"],
        vec!["boolean", "boolean", "integer?", "timetz"],
    ] {
        check(&specs).unwrap_or_else(|e| panic!("{e}"));
    }
}
