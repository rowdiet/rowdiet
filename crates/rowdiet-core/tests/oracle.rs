//! An independent brute-force oracle for the decision policy, sharing no code with the engine.
//!
//! It walks concrete byte offsets for every permutation of every column (no class collapsing)
//! and every realization of every varlena: a short payload stored unaligned behind a 1-byte
//! header, a long payload of 128..=135 bytes aligned behind a 4-byte header, and an 18-byte TOAST
//! pointer. Short payload lengths follow what PostgreSQL stores for the type (arrays pad their
//! elements, numeric digits are 2 bytes), stated here from the storage format and not taken from
//! the engine. A `varchar(n)` holding more than 20 bytes can be compressed in line, aligned, by a
//! wide row's toaster; one of at most 20 bytes is always short. The tool is driven through the public API only, and every claim it prints is
//! judged against the oracle: a finding must dominate with the reported saving range, an
//! exhaustive clean verdict must have no dominating permutation, and a frontier must never be an
//! order that dominates.

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
}

/// Types whose stored lengths do not depend on the database encoding: the tool may claim that
/// nothing dominates only over these.
fn verified(ty: &str) -> bool {
    !ty.starts_with("varchar")
}

fn values(kind: Kind, band: Option<bool>) -> Vec<Value> {
    let Kind::Var {
        short_only,
        short_residues,
        ..
    } = kind
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if band != Some(true) {
        out.extend(short_residues.iter().map(|&r| Value::Short(8 + r)));
        if !short_only {
            out.push(Value::Toast);
        }
    }
    if !short_only && band != Some(false) {
        out.extend((128..136).map(Value::Long));
    }
    out
}

fn align_up(offset: u64, align: u64) -> u64 {
    offset.div_ceil(align) * align
}

fn padding(kinds: &[Kind], order: &[usize], realization: &[Value]) -> i64 {
    let mut offset = 0u64;
    let mut total = 0u64;
    for &column in order {
        match kinds[column] {
            Kind::Fixed { len, align } => {
                let start = align_up(offset, align);
                total += start - offset;
                offset = start + len;
            }
            Kind::Var { align, .. } => match realization[column] {
                Value::Short(len) => offset += 1 + len,
                Value::Toast => offset += 18,
                Value::Long(len) => {
                    let start = align_up(offset, align);
                    total += start - offset;
                    offset = start + 4 + len;
                }
            },
        }
    }
    total as i64
}

/// Visit every realization, one value per column (fixed columns hold a placeholder), until `f`
/// returns false. `pins` holds a varlena to its long (true) or short/TOAST (false) values.
fn each_realization(kinds: &[Kind], pins: &[Option<bool>], mut f: impl FnMut(&[Value]) -> bool) {
    let choices: Vec<Vec<Value>> = kinds.iter().zip(pins).map(|(&k, &pin)| values(k, pin)).collect();
    let varied: Vec<usize> = (0..kinds.len()).filter(|&c| !choices[c].is_empty()).collect();
    let mut index = vec![0usize; kinds.len()];
    let mut current: Vec<Value> = (0..kinds.len())
        .map(|c| choices[c].first().copied().unwrap_or(Value::Short(0)))
        .collect();
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

/// `pad(a) - pad(b)` bounds over every realization the pins allow.
fn bounds(kinds: &[Kind], pins: &[Option<bool>], a: &[usize], b: &[usize]) -> (i64, i64) {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    each_realization(kinds, pins, |r| {
        let d = padding(kinds, a, r) - padding(kinds, b, r);
        lo = lo.min(d);
        hi = hi.max(d);
        true
    });
    (lo, hi)
}

/// True when `candidate` is never worse than `current` and better somewhere; stops at the first
/// realization where it is worse.
/// Padding bounds of one order over every realization.
fn order_bounds(kinds: &[Kind], order: &[usize]) -> (i64, i64) {
    let pins = vec![None; kinds.len()];
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    each_realization(kinds, &pins, |r| {
        let p = padding(kinds, order, r);
        lo = lo.min(p);
        hi = hi.max(p);
        true
    });
    (lo, hi)
}

fn dominates(kinds: &[Kind], current: &[usize], candidate: &[usize]) -> bool {
    let pins = vec![None; kinds.len()];
    let mut worse = false;
    let mut better = false;
    each_realization(kinds, &pins, |r| {
        let d = padding(kinds, current, r) - padding(kinds, candidate, r);
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

/// One permutation per arrangement that tells columns apart: two fixed columns of one type are
/// the same bytes, so swapping them changes nothing.
fn distinct_orders(types: &[&str], kinds: &[Kind]) -> Vec<Vec<usize>> {
    let mut seen = std::collections::BTreeSet::new();
    permutations(kinds.len())
        .into_iter()
        .filter(|order| {
            let key: Vec<String> = order
                .iter()
                .map(|&c| match kinds[c] {
                    Kind::Fixed { .. } => types[c].to_string(),
                    Kind::Var { .. } => format!("#{c}"),
                })
                .collect();
            seen.insert(key)
        })
        .collect()
}

fn analyze(types: &[&str]) -> TableReport {
    let cols: Vec<String> = types
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("c{i} {ty} NOT NULL"))
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

/// Judge every claim the tool prints for `types` against the oracle.
fn check(types: &[&str]) -> Result<(), String> {
    let kinds: Vec<Kind> = types.iter().map(|t| kind_of(t)).collect();
    let t = analyze(types);
    let n = kinds.len();
    let identity: Vec<usize> = (0..n).collect();
    let free = vec![None; n];
    let table = format!("({})", types.join(", "));
    let (cur_lo, cur_hi) = order_bounds(&kinds, &identity);
    if (t.current.padding_min as i64, t.current.padding_max as i64) != (cur_lo, cur_hi) {
        return Err(format!(
            "{table}: current bounds [{}, {}] vs the oracle's [{cur_lo}, {cur_hi}]",
            t.current.padding_min, t.current.padding_max
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
    if t.avoidable_bytes_per_row > 0.0 {
        let suggested = indices(&t.suggested_order);
        if !dominates(&kinds, &identity, &suggested) {
            return Err(format!("{table}: the suggested {suggested:?} does not dominate"));
        }
        let (lo, hi) = bounds(&kinds, &free, &identity, &suggested);
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
        let (sug_lo, sug_hi) = order_bounds(&kinds, &suggested);
        if (t.suggested.padding_min as i64, t.suggested.padding_max as i64) != (sug_lo, sug_hi) {
            return Err(format!(
                "{table}: suggested bounds [{}, {}] vs the oracle's [{sug_lo}, {sug_hi}]",
                t.suggested.padding_min, t.suggested.padding_max
            ));
        }
    } else if t.dominance_search == DominanceScope::Exhaustive
        && let Some(order) = distinct_orders(types, &kinds)
            .into_iter()
            .find(|order| *order != identity && dominates(&kinds, &identity, order))
    {
        return Err(format!(
            "{table}: \"no dominating reorder exists\", but {order:?} dominates"
        ));
    }
    if let Some(frontier) = &t.frontier {
        let alternative = indices(&frontier.order);
        if dominates(&kinds, &identity, &alternative) {
            return Err(format!(
                "{table}: the frontier {alternative:?} dominates the current order"
            ));
        }
        let (_, hi) = bounds(&kinds, &free, &identity, &alternative);
        if hi <= 0 {
            return Err(format!("{table}: the frontier {alternative:?} never wins"));
        }
        for band in &frontier.bands {
            let long: Vec<usize> = indices(&band.long_form);
            let pins: Vec<Option<bool>> = (0..n)
                .map(|c| match kinds[c] {
                    Kind::Var { short_only: false, .. } => Some(long.contains(&c)),
                    _ => None,
                })
                .collect();
            let (lo, hi) = bounds(&kinds, &pins, &identity, &alternative);
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

/// Tables of 3 to 5 columns with `varlenas` of them drawn from the varlena pool.
fn table(varlenas: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<&'static str>> {
    (3usize..=5, varlenas).prop_flat_map(|(n, v)| {
        let v = v.min(n);
        (
            proptest::collection::vec(proptest::sample::select(&VARLENAS[..]), v),
            proptest::collection::vec(proptest::sample::select(&FIXED[..]), n - v),
            any::<proptest::sample::Index>(),
        )
            .prop_map(|(mut var, fixed, shuffle)| {
                var.extend(fixed);
                let len = var.len();
                var.rotate_left(shuffle.index(len));
                var
            })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn every_claim_holds_against_the_oracle(types in table(1..=3)) {
        check(&types).map_err(TestCaseError::fail)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn every_claim_holds_with_four_or_five_varlenas(types in table(4..=5)) {
        check(&types).map_err(TestCaseError::fail)?;
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
