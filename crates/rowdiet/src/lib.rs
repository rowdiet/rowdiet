//! rowdiet CLI implementation, shared by the `rowdiet` binary and the `cargo-rowdiet`
//! cargo-subcommand shim (`cargo rowdiet ...`).

mod render;

use clap::{Parser, ValueEnum};
use rowdiet_core::catalog::parse_assume_spec;
use rowdiet_core::{
    Analysis, Baseline, Config, Note, ParserBackend, SqlSource, analyze_sources_with, baseline, fs as core_fs,
};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "rowdiet",
    version,
    about = "Postgres column-tetris linter: static alignment-padding analysis of migration DDL (no database needed)"
)]
struct Cli {
    /// SQL files, migration directories (recursed, version-ordered), or '-' for stdin
    #[arg(required = true)]
    paths: Vec<String>,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Exit 1 if any table's avoidable bytes/row exceed this threshold
    #[arg(long, value_name = "BYTES")]
    fail_over: Option<u64>,
    /// Print a reordered CREATE TABLE for each table with avoidable waste
    #[arg(long)]
    suggest: bool,
    /// Project the per-row waste over N rows in the text report
    #[arg(long, value_name = "N")]
    rows: Option<u64>,
    /// Teach an unknown type: NAME=varlena:ALIGN or NAME=fixed:LEN:ALIGN (align c|s|i|d); repeatable
    #[arg(long = "assume-type", value_name = "SPEC")]
    assume_type: Vec<String>,
    /// Parser backend: pure-Rust sqlparser (default) or the real PG17 grammar via libpg_query
    #[arg(long, value_enum, default_value_t = ParserChoice::Sqlparser)]
    parser: ParserChoice,
    /// Baseline file with per-table accepted-debt allowances (brownfield freeze-known-debt gating)
    #[arg(long, value_name = "FILE")]
    baseline: Option<PathBuf>,
    /// Rewrite the baseline from the current analysis (entries for tables over the fail-over)
    #[arg(long, requires = "baseline", conflicts_with = "accept")]
    update_baseline: bool,
    /// Accept one table's current state into the baseline, leaving other entries untouched; repeatable
    #[arg(long, value_name = "TABLE", requires = "baseline")]
    accept: Vec<String>,
    /// Also exit 1 when statements were skipped or tables are incomplete (degraded analysis)
    #[arg(long)]
    fail_on_degraded: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
    Github,
}

#[derive(Clone, Copy, ValueEnum)]
enum ParserChoice {
    Sqlparser,
    PgExact,
}

fn backend(choice: ParserChoice) -> Result<ParserBackend, String> {
    match choice {
        ParserChoice::Sqlparser => Ok(ParserBackend::Sqlparser),
        #[cfg(feature = "pg-exact")]
        ParserChoice::PgExact => Ok(ParserBackend::PgExact),
        #[cfg(not(feature = "pg-exact"))]
        ParserChoice::PgExact => Err("this rowdiet binary was built without the pg-exact feature".to_string()),
    }
}

pub fn cli_main(args: impl IntoIterator<Item = String>) -> ExitCode {
    let cli = Cli::parse_from(args);
    match run(&cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("rowdiet: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let config = build_config(&cli.assume_type)?;
    let gathered = gather_sources(&cli.paths)?;
    let mut analysis = analyze_sources_with(backend(cli.parser)?, &gathered.sources, &config);
    for dir in &gathered.empty_dirs {
        analysis.notes.push(Note::empty_scan(dir));
    }
    let analysis = analysis;
    if cli.update_baseline || !cli.accept.is_empty() {
        let path = cli.baseline.as_deref().ok_or("--baseline FILE is required")?;
        return maintain_baseline(cli, path, &analysis);
    }
    let loaded = match &cli.baseline {
        Some(path) => Some(load_baseline(path)?),
        None => None,
    };
    let gate = baseline::evaluate(&analysis, cli.fail_over, cli.fail_on_degraded, loaded.as_ref());
    let shown_fail_over = cli.fail_over.or(loaded.as_ref().map(|b| b.fail_over));
    let output = match cli.format {
        Format::Text => render::text(&analysis, cli.rows, cli.suggest, &gate),
        Format::Json => render::json(&analysis, shown_fail_over, &gate)?,
        Format::Github => render::github(&analysis, &gate),
    };
    print!("{output}");
    if matches!(cli.format, Format::Github) {
        append_step_summary(&analysis, &gate);
    }
    Ok(if gate.exceeded {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Under GitHub Actions the annotation stream is capped by the runner (10 per severity per
/// step), so `--format github` also appends the full report to `$GITHUB_STEP_SUMMARY` when the
/// environment provides it. Best-effort: a broken summary path must not change the exit code.
fn append_step_summary(analysis: &Analysis, gate: &rowdiet_core::GateOutcome) {
    let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let summary = render::github_step_summary(analysis, gate);
    let written = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, summary.as_bytes()));
    if let Err(e) = written {
        eprintln!("rowdiet: could not append the step summary ({path}: {e})");
    }
}

/// The maintenance operations: `--update-baseline` rewrites the file wholesale;
/// `--accept <table>` refreshes exactly the named entries. Both exit 0 without gating.
fn maintain_baseline(cli: &Cli, path: &Path, analysis: &Analysis) -> Result<ExitCode, String> {
    let existing = if path.exists() {
        Some(load_baseline(path)?)
    } else {
        None
    };
    let written = if cli.update_baseline {
        let fail_over = cli
            .fail_over
            .or(existing.as_ref().map(|b| b.fail_over))
            .ok_or("--update-baseline needs --fail-over (there is no existing baseline to take it from)")?;
        let updated = baseline::build_from(analysis, fail_over, env!("CARGO_PKG_VERSION"));
        write_baseline(path, &updated)?;
        updated
    } else {
        let mut updated = existing.ok_or_else(|| {
            format!(
                "--accept needs an existing baseline file (create one with --update-baseline): {}",
                path.display()
            )
        })?;
        baseline::accept_tables(&mut updated, analysis, &cli.accept)?;
        updated.rowdiet = env!("CARGO_PKG_VERSION").to_string();
        write_baseline(path, &updated)?;
        updated
    };
    println!(
        "baseline written: {} — {} table(s) with accepted debt, fail-over {}",
        path.display(),
        written.tables.len(),
        written.fail_over
    );
    Ok(ExitCode::SUCCESS)
}

fn load_baseline(path: &Path) -> Result<Baseline, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: bad baseline JSON: {e}", path.display()))
}

fn write_baseline(path: &Path, baseline: &Baseline) -> Result<(), String> {
    let text = serde_json::to_string_pretty(baseline).map_err(|e| e.to_string())? + "\n";
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn build_config(specs: &[String]) -> Result<Config, String> {
    let mut config = Config::default();
    for spec in specs {
        let (name, kind) = parse_assume_spec(spec).map_err(|e| format!("--{e}"))?;
        config.assume.insert(name, kind);
    }
    Ok(config)
}

struct Gathered {
    sources: Vec<SqlSource>,
    /// Directory arguments that matched no SQL files — surfaced as empty-scan notes so a
    /// typo'd path cannot gate green forever.
    empty_dirs: Vec<String>,
}

fn gather_sources(paths: &[String]) -> Result<Gathered, String> {
    let mut sources = Vec::new();
    let mut empty_dirs = Vec::new();
    for raw in paths {
        if raw == "-" {
            let mut sql = String::new();
            std::io::stdin()
                .read_to_string(&mut sql)
                .map_err(|e| format!("stdin: {e}"))?;
            sources.push(SqlSource {
                name: "<stdin>".into(),
                sql,
            });
            continue;
        }
        let path = Path::new(raw);
        if path.is_dir() {
            let files = core_fs::collect_sql_files(path).map_err(|e| format!("{raw}: {e}"))?;
            if files.is_empty() {
                empty_dirs.push(raw.clone());
            }
            for file in files {
                sources.push(core_fs::read_source(&file).map_err(|e| format!("{}: {e}", file.display()))?);
            }
        } else {
            sources.push(core_fs::read_source(path).map_err(|e| format!("{raw}: {e}"))?);
        }
    }
    Ok(Gathered { sources, empty_dirs })
}
