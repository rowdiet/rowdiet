//! Criterion benches over the real pipeline stages — split, extract, fold, layout, report,
//! version ordering, gate — plus end-to-end `analyze_sources` on synthetic corpora built in
//! code (nothing checked in, nothing private). Run `cargo bench --bench pipeline`; the
//! extract benches also cover libpg_query when built with `--features pg-exact`.

use criterion::{Criterion, Throughput};
use rowdiet_core::layout::{Align, ColumnKind};
use rowdiet_core::{
    Baseline, BaselineEntry, Config, SqlSource, analyze_sources, baseline, extract, layout, split, version,
};
use std::fmt::Write as _;
use std::hint::black_box;

/// Column types cycled through generated tables — a realistic alignment mix (8/4/2/1-byte fixed,
/// plain varlena, proven-short varlena).
const TYPES: [&str; 8] = [
    "bigint",
    "int",
    "smallint",
    "boolean",
    "uuid",
    "text",
    "timestamptz",
    "varchar(20)",
];

fn create_table(name: &str, columns: usize) -> String {
    let mut sql = format!("CREATE TABLE {name} (\n");
    for i in 0..columns {
        let ty = TYPES[i % TYPES.len()];
        let comma = if i + 1 == columns { "" } else { "," };
        let _ = writeln!(sql, "    c{i} {ty} NOT NULL{comma}");
    }
    sql.push_str(");\n");
    sql
}

/// A migration series shaped like a real repo: per file one CREATE, an index, an ALTER, some DML.
fn many_files(files: usize) -> Vec<SqlSource> {
    (0..files)
        .map(|i| {
            let mut sql = format!("-- migration {i}\n");
            sql.push_str(&create_table(&format!("t{i}"), 6));
            let _ = writeln!(sql, "CREATE INDEX idx_t{i} ON t{i} (c0);");
            let _ = writeln!(sql, "ALTER TABLE t{i} ADD COLUMN extra timestamptz;");
            let _ = writeln!(sql, "INSERT INTO t{i} (c0) VALUES (1);");
            SqlSource {
                name: format!("V{i}__t{i}.sql"),
                sql,
            }
        })
        .collect()
}

/// One table accreting hundreds of single-ALTER files — the deep-fold shape.
fn fold_chain(alters: usize) -> Vec<SqlSource> {
    let mut sources = vec![SqlSource {
        name: "V1__init.sql".into(),
        sql: create_table("big", 4),
    }];
    for i in 0..alters {
        sources.push(SqlSource {
            name: format!("V{}__add.sql", i + 2),
            sql: format!("ALTER TABLE big ADD COLUMN a{i} {};\n", TYPES[i % TYPES.len()]),
        });
    }
    sources
}

/// Statements buried in comment banners. `marked` plants a `rowdiet:ignore` per file, so both
/// sides of the marker scan are covered: absent (the overwhelmingly common case) and present.
fn comment_heavy(files: usize, marked: bool) -> Vec<SqlSource> {
    let banner: String = (0..30).fold(String::new(), |mut acc, i| {
        let _ = writeln!(acc, "-- banner line {i}: lorem ipsum placeholder prose for width");
        acc
    });
    (0..files)
        .map(|i| {
            let mut sql = banner.clone();
            let _ = writeln!(sql, "/* block\n   comment\n   /* nested */\n */");
            let marker = if marked { "-- rowdiet:ignore\n" } else { "" };
            let _ = writeln!(
                sql,
                "CREATE TABLE c{i} (\n{marker}    a int NOT NULL,\n    b bigint NOT NULL\n);"
            );
            sql.push_str(&banner);
            SqlSource {
                name: format!("V{i}__c{i}.sql"),
                sql,
            }
        })
        .collect()
}

/// The DO-body shapes lib.rs special-cases: a guarded CREATE TYPE plus the dynamic
/// hash-partition loop against a modeled parent.
fn do_blocks(files: usize) -> Vec<SqlSource> {
    (0..files)
        .map(|i| {
            let mut sql =
                format!("CREATE TABLE p{i} (id bigint NOT NULL, created date NOT NULL) PARTITION BY HASH (id);\n");
            let _ = writeln!(
                sql,
                "DO $$\nBEGIN\n    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'status{i}') THEN\n        \
                 CREATE TYPE status{i} AS ENUM ('a', 'b');\n    END IF;\n    FOR n IN 0..7 LOOP\n        \
                 EXECUTE format('CREATE TABLE %I PARTITION OF p{i} FOR VALUES WITH (MODULUS 8, REMAINDER %s)', \
                 'p{i}_' || n, n);\n    END LOOP;\nEND\n$$;"
            );
            SqlSource {
                name: format!("V{i}__p{i}.sql"),
                sql,
            }
        })
        .collect()
}

fn corpus_bytes(sources: &[SqlSource]) -> u64 {
    sources.iter().map(|s| s.sql.len() as u64).sum()
}

fn concat_sql(sources: &[SqlSource]) -> String {
    sources.iter().fold(String::new(), |mut acc, s| {
        acc.push_str(&s.sql);
        acc
    })
}

fn bench_split(c: &mut Criterion) {
    let plain = concat_sql(&many_files(300));
    let comments = concat_sql(&comment_heavy(100, false));
    let mut group = c.benchmark_group("split");
    group.throughput(Throughput::Bytes(plain.len() as u64));
    group.bench_function("plain", |b| b.iter(|| split::split(black_box(&plain))));
    group.throughput(Throughput::Bytes(comments.len() as u64));
    group.bench_function("comment_heavy", |b| b.iter(|| split::split(black_box(&comments))));
    group.finish();
}

fn bench_extract(c: &mut Criterion) {
    let wide = create_table("wide", 120);
    let alter = "ALTER TABLE wide ADD COLUMN late timestamptz NOT NULL";
    let mut group = c.benchmark_group("extract");
    group.bench_function("create_wide_120", |b| {
        b.iter(|| extract::extract(black_box(&wide)).unwrap());
    });
    group.bench_function("alter_add", |b| b.iter(|| extract::extract(black_box(alter)).unwrap()));
    // The preprocess probe runs on every statement; almost none needs the rewrite.
    group.bench_function("preprocess_miss", |b| b.iter(|| extract::preprocess(black_box(&wide))));
    #[cfg(feature = "pg-exact")]
    {
        use rowdiet_core::extract_pgq;
        group.bench_function("pgq_create_wide_120", |b| {
            b.iter(|| extract_pgq::extract(black_box(&wide)).unwrap());
        });
    }
    group.finish();
}

fn bench_analyze(c: &mut Criterion) {
    let config = Config::default();
    let cases = [
        ("many_files_1000", many_files(1000)),
        ("fold_chain_400", fold_chain(400)),
        (
            "wide_tables",
            (0..20)
                .map(|i| SqlSource {
                    name: format!("V{i}__w.sql"),
                    sql: create_table(&format!("w{i}"), 120),
                })
                .collect(),
        ),
        ("do_blocks_100", do_blocks(100)),
        ("comment_heavy_200", comment_heavy(200, false)),
        ("comment_heavy_marked_200", comment_heavy(200, true)),
    ];
    let mut group = c.benchmark_group("analyze");
    group.sample_size(30);
    for (name, sources) in cases {
        group.throughput(Throughput::Bytes(corpus_bytes(&sources)));
        group.bench_function(name, |b| b.iter(|| analyze_sources(black_box(&sources), &config)));
    }
    group.finish();
}

fn kinds_regular(n: usize) -> Vec<ColumnKind> {
    let cycle = [
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double,
        },
        ColumnKind::Fixed {
            len: 4,
            align: Align::Int,
        },
        ColumnKind::Fixed {
            len: 2,
            align: Align::Short,
        },
        ColumnKind::Fixed {
            len: 1,
            align: Align::Char,
        },
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
        },
    ];
    (0..n).map(|i| cycle[i % cycle.len()]).collect()
}

/// 24 fixed columns, 6 padding classes, two of them irregular (timetz, macaddr) — the shape
/// that defeats the sort heuristic and sends `suggested_order` into the exact memoized search.
fn kinds_irregular_24() -> Vec<ColumnKind> {
    let cycle = [
        ColumnKind::Fixed {
            len: 12,
            align: Align::Double,
        },
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double,
        },
        ColumnKind::Fixed {
            len: 6,
            align: Align::Int,
        },
        ColumnKind::Fixed {
            len: 4,
            align: Align::Int,
        },
        ColumnKind::Fixed {
            len: 2,
            align: Align::Short,
        },
        ColumnKind::Fixed {
            len: 1,
            align: Align::Char,
        },
    ];
    (0..24).map(|i| cycle[i % cycle.len()]).collect()
}

fn bench_layout(c: &mut Criterion) {
    let regular = kinds_regular(100);
    let irregular = kinds_irregular_24();
    // The DP must actually engage, or the bench silently measures the plain sort.
    assert_ne!(
        layout::suggested_order(&irregular),
        {
            let mut sorted: Vec<usize> = (0..irregular.len()).collect();
            sorted.sort_by_key(|&i| std::cmp::Reverse(irregular[i].align().bytes()));
            sorted
        },
        "irregular fixture no longer exercises the refinement search"
    );
    let mut group = c.benchmark_group("layout");
    group.bench_function("walk_100", |b| b.iter(|| layout::walk(black_box(&regular))));
    group.bench_function("suggested_order_regular_100", |b| {
        b.iter(|| layout::suggested_order(black_box(&regular)));
    });
    group.bench_function("suggested_order_irregular_24", |b| {
        b.iter(|| layout::suggested_order(black_box(&irregular)));
    });
    group.finish();
}

fn bench_version(c: &mut Criterion) {
    let names: Vec<String> = (0..1000)
        .map(|i| match i % 4 {
            0 => format!("V{}__one.sql", 1_700_000_000 + i),
            1 => format!("V{}_{}__two.sql", 100 + i, i % 7),
            2 => format!("{}_three.sql", 20_240_000 + i),
            _ => format!("R__repeat_{i}.sql"),
        })
        .collect();
    c.bench_function("version/sort_1000", |b| {
        b.iter(|| {
            let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
            sorted.sort_by(|a, b| version::compare(a, b));
            sorted
        });
    });
}

fn bench_gate(c: &mut Criterion) {
    let analysis = analyze_sources(&many_files(500), &Config::default());
    let entries: std::collections::BTreeMap<String, BaselineEntry> = analysis
        .tables
        .iter()
        .step_by(2)
        .map(|t| {
            (
                t.name.clone(),
                BaselineEntry {
                    bytes: t.avoidable_bytes_per_row,
                    layout: t.layout_signature.clone(),
                },
            )
        })
        .collect();
    let base = Baseline {
        rowdiet: "bench".into(),
        fail_over: 0,
        tables: entries,
    };
    c.bench_function("gate/evaluate_500", |b| {
        b.iter(|| baseline::evaluate(black_box(&analysis), Some(0), true, Some(&base)));
    });
}

// Hand-rolled runner instead of criterion_group!/criterion_main!: the macros expand to a pub fn,
// which the missing_docs ratchet would reject.
fn main() {
    let mut c = Criterion::default().configure_from_args();
    bench_split(&mut c);
    bench_extract(&mut c);
    bench_analyze(&mut c);
    bench_layout(&mut c);
    bench_version(&mut c);
    bench_gate(&mut c);
    c.final_summary();
}
