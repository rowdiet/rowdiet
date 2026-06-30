mod render;

use clap::{Parser, ValueEnum};
use rowdiet_core::{
    analyze_sources_with, fs as core_fs, Align, Analysis, AssumedKind, Config, ParserBackend, SqlSource,
};
use std::io::Read as _;
use std::path::Path;
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

fn main() -> ExitCode {
    let cli = Cli::parse();
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
    let sources = gather_sources(&cli.paths)?;
    let analysis = analyze_sources_with(backend(cli.parser)?, &sources, &config);
    let gate = gate_exceeded(&analysis, cli.fail_over);
    let output = match cli.format {
        Format::Text => render::text(&analysis, cli.rows, cli.suggest, cli.fail_over),
        Format::Json => render::json(&analysis, cli.fail_over, gate)?,
        Format::Github => render::github(&analysis, cli.fail_over),
    };
    print!("{output}");
    Ok(if gate { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn gate_exceeded(analysis: &Analysis, fail_over: Option<u64>) -> bool {
    match fail_over {
        Some(limit) => analysis
            .tables
            .iter()
            .any(|t| !t.ignored && t.avoidable_bytes_per_row > limit),
        None => false,
    }
}

fn build_config(specs: &[String]) -> Result<Config, String> {
    let mut config = Config::default();
    for spec in specs {
        let (name, kind) = parse_assume(spec)?;
        config.assume.insert(name, kind);
    }
    Ok(config)
}

fn parse_assume(spec: &str) -> Result<(String, AssumedKind), String> {
    let usage = || format!("--assume-type `{spec}`: expected NAME=varlena:ALIGN or NAME=fixed:LEN:ALIGN");
    let (name, rest) = spec.split_once('=').ok_or_else(usage)?;
    let parts: Vec<&str> = rest.split(':').collect();
    let kind = match parts.as_slice() {
        ["varlena", align] => AssumedKind::Varlena {
            align: parse_align(align)?,
        },
        ["fixed", len, align] => AssumedKind::Fixed {
            len: len
                .parse()
                .map_err(|_| format!("--assume-type `{spec}`: bad length `{len}`"))?,
            align: parse_align(align)?,
        },
        _ => return Err(usage()),
    };
    Ok((name.trim().to_lowercase(), kind))
}

fn parse_align(s: &str) -> Result<Align, String> {
    match s {
        "c" | "char" => Ok(Align::Char),
        "s" | "short" => Ok(Align::Short),
        "i" | "int" => Ok(Align::Int),
        "d" | "double" => Ok(Align::Double),
        other => Err(format!("bad alignment `{other}` (use c|s|i|d)")),
    }
}

fn gather_sources(paths: &[String]) -> Result<Vec<SqlSource>, String> {
    let mut sources = Vec::new();
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
            for file in core_fs::collect_sql_files(path).map_err(|e| format!("{raw}: {e}"))? {
                sources.push(core_fs::read_source(&file).map_err(|e| format!("{}: {e}", file.display()))?);
            }
        } else {
            sources.push(core_fs::read_source(path).map_err(|e| format!("{raw}: {e}"))?);
        }
    }
    Ok(sources)
}
