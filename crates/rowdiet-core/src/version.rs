//! Migration filename ordering: `V1__x.sql`, `V1_2__x.sql` (sub-version), `V1.2__x.sql`,
//! `20240101_x.sql`, `U`/`B` prefixes. Unversioned names (e.g. Flyway `R__` repeatables) sort
//! after all versioned ones, lexicographically.

use std::cmp::Ordering;

pub fn compare(a: &str, b: &str) -> Ordering {
    sort_key(a).cmp(&sort_key(b))
}

pub fn sort_key(file_name: &str) -> (u8, Vec<u64>, String) {
    match parse_version(file_name) {
        Some(segments) => (0, segments, file_name.to_string()),
        None => (1, Vec::new(), file_name.to_string()),
    }
}

fn parse_version(name: &str) -> Option<Vec<u64>> {
    let b = name.as_bytes();
    let mut i = 0usize;
    if matches!(b.first(), Some(b'V' | b'v' | b'U' | b'u' | b'B' | b'b')) {
        i += 1;
    }
    let mut segments = Vec::new();
    loop {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        segments.push(name[start..i].parse().ok()?);
        match b.get(i) {
            Some(b'_' | b'.') if b.get(i + 1).is_some_and(|c| c.is_ascii_digit()) => i += 1,
            _ => break,
        }
    }
    Some(segments)
}

#[cfg(test)]
mod tests;
