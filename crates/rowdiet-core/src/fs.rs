//! Filesystem conveniences (not compiled for wasm targets — disable the default `fs` feature).

use crate::{Analysis, Config, SqlSource};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// All `*.sql` files under `dir` (recursive), version-ordered within each directory.
///
/// Walk policy, kept identical across the CLI and the JVM adapters: dot-prefixed entries are
/// skipped (the explicitly passed root is exempt — only entries found during the walk are
/// filtered); symlinked directories are not entered, so a link cycle cannot duplicate files
/// (a symlinked `*.sql` file is still collected and read through the link).
pub fn collect_sql_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    files.sort_by(|a, b| {
        a.parent()
            .cmp(&b.parent())
            .then_with(|| crate::version::compare(file_name(a), file_name(b)))
    });
    Ok(files)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_cached_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if entry.file_name().as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        // file_type() does not follow symlinks, so is_dir() is false for a symlinked directory
        // and the else-branch keeps symlinked files.
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) {
            out.push(path);
        }
    }
    Ok(())
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

pub fn read_source(path: &Path) -> io::Result<SqlSource> {
    let bytes = std::fs::read(path)?;
    Ok(SqlSource {
        name: path.display().to_string(),
        sql: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// The five-line CI guard: point it at a migrations directory (e.g. in a `#[test]` or right
/// before the migration runner) and gate on the result. Uses the default parser backend; use
/// [`analyze_dir_with`] to pick one.
pub fn analyze_dir(dir: impl AsRef<Path>, config: &Config) -> io::Result<Analysis> {
    analyze_dir_with(crate::ParserBackend::Sqlparser, dir, config)
}

pub fn analyze_dir_with(backend: crate::ParserBackend, dir: impl AsRef<Path>, config: &Config) -> io::Result<Analysis> {
    let dir = dir.as_ref();
    let mut sources = Vec::new();
    for path in collect_sql_files(dir)? {
        let bytes = std::fs::read(&path)?;
        let name = path.strip_prefix(dir).unwrap_or(&path).display().to_string();
        sources.push(SqlSource {
            name,
            sql: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }
    let mut analysis = crate::analyze_sources_with(backend, &sources, config);
    if sources.is_empty() {
        analysis
            .notes
            .push(crate::fold::Note::empty_scan(&dir.display().to_string()));
    }
    Ok(analysis)
}
