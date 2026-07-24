//! Tolerant statement splitter. Never fails: understands PG quoting (dollar quotes, E-strings,
//! doubled quotes, nested block comments) just enough to find top-level semicolons and 1-based
//! line numbers. Whether a statement parses is the caller's per-statement concern — a linter must
//! degrade loudly per statement, never abort a whole file.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStatement {
    pub text: String,
    pub line: u32,
}

pub fn split(sql: &str) -> Vec<RawStatement> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut content_start: Option<(usize, u32)> = None;
    while i < b.len() {
        match b[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // PG block comments nest, unlike C.
                let mut depth = 1u32;
                i += 2;
                while i < b.len() && depth > 0 {
                    match b[i] {
                        b'\n' => {
                            line += 1;
                            i += 1;
                        }
                        b'/' if b.get(i + 1) == Some(&b'*') => {
                            depth += 1;
                            i += 2;
                        }
                        b'*' if b.get(i + 1) == Some(&b'/') => {
                            depth -= 1;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
            }
            b';' => {
                if let Some((start, l)) = content_start.take() {
                    push_stmt(&mut out, &sql[start..i], l);
                }
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            b'\'' => {
                mark(&mut content_start, i, line);
                // Backslash escapes only in E'..' strings (standard_conforming_strings default).
                let estring = i > 0 && matches!(b[i - 1], b'e' | b'E') && (i < 2 || !is_ident_byte(b[i - 2]));
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\n' => {
                            line += 1;
                            i += 1;
                        }
                        b'\\' if estring => {
                            if b.get(i + 1) == Some(&b'\n') {
                                line += 1;
                            }
                            i += 2;
                        }
                        b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                        b'\'' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'"' => {
                mark(&mut content_start, i, line);
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\n' => {
                            line += 1;
                            i += 1;
                        }
                        b'"' if b.get(i + 1) == Some(&b'"') => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'$' => {
                mark(&mut content_start, i, line);
                match dollar_tag_end(b, i) {
                    Some(tag_end) => {
                        let tag = &b[i..tag_end];
                        i = tag_end;
                        while i < b.len() {
                            if b[i..].starts_with(tag) {
                                i += tag.len();
                                break;
                            }
                            if b[i] == b'\n' {
                                line += 1;
                            }
                            i += 1;
                        }
                    }
                    None => i += 1,
                }
            }
            _ => {
                mark(&mut content_start, i, line);
                i += 1;
            }
        }
    }
    if let Some((start, l)) = content_start {
        push_stmt(&mut out, &sql[start..], l);
    }
    out
}

fn mark(content_start: &mut Option<(usize, u32)>, i: usize, line: u32) {
    if content_start.is_none() {
        *content_start = Some((i, line));
    }
}

fn push_stmt(out: &mut Vec<RawStatement>, text: &str, line: u32) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        out.push(RawStatement {
            text: trimmed.to_string(),
            line,
        });
    }
}

fn is_ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80
}

/// The first dollar-quoted body in `text` (`$tag$ … $tag$`), comment/string-aware up to the
/// opening tag. Used to look inside `DO` blocks; None when absent or unterminated.
pub fn dollar_quoted_body(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1u32;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'\'' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => match dollar_tag_end(b, i) {
                Some(tag_end) => {
                    let tag = &text[i..tag_end];
                    let close = text[tag_end..].find(tag)?;
                    return Some(&text[tag_end..tag_end + close]);
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }
    None
}

pub(crate) fn ident_byte(c: u8) -> bool {
    is_ident_byte(c)
}

/// `$tag$` (tag possibly empty, never starting with a digit — `$1` stays a parameter). Returns the
/// index just past the closing `$` of the opening tag.
fn dollar_tag_end(b: &[u8], start: usize) -> Option<usize> {
    if b.get(start + 1).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut j = start + 1;
    while j < b.len() && is_ident_byte(b[j]) {
        j += 1;
    }
    if j < b.len() && b[j] == b'$' { Some(j + 1) } else { None }
}

#[cfg(test)]
mod tests;

/// 1-based lines of every occurrence of `marker` that sits inside a comment of `text` —
/// line (`--`) or block (`/* */`, nested). String literals, dollar-quoted bodies, and quoted
/// identifiers are skipped, so a marker in data can never count; the ignore-marker semantics
/// are built on this.
pub(crate) fn comment_marker_lines(text: &str, marker: &str) -> Vec<u32> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let count_span = |span: &str, at_line: u32, out: &mut Vec<u32>| {
        let mut rel_line = 0u32;
        let mut rest = span;
        while let Some(pos) = rest.find(marker) {
            rel_line += rest[..pos].matches('\n').count() as u32;
            out.push(at_line + rel_line);
            rest = &rest[pos + marker.len()..];
        }
    };
    while i < b.len() {
        match b[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                count_span(&text[start..i], line, &mut out);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                let start_line = line;
                let mut depth = 1u32;
                i += 2;
                while i < b.len() && depth > 0 {
                    match b[i] {
                        b'\n' => {
                            line += 1;
                            i += 1;
                        }
                        b'/' if b.get(i + 1) == Some(&b'*') => {
                            depth += 1;
                            i += 2;
                        }
                        b'*' if b.get(i + 1) == Some(&b'/') => {
                            depth -= 1;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
                count_span(&text[start..i.min(text.len())], start_line, &mut out);
            }
            b'\'' => {
                let estring = i > 0 && matches!(b[i - 1], b'e' | b'E') && (i < 2 || !is_ident_byte(b[i - 2]));
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\n' => {
                            line += 1;
                            i += 1;
                        }
                        b'\\' if estring => {
                            if b.get(i + 1) == Some(&b'\n') {
                                line += 1;
                            }
                            i += 2;
                        }
                        b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                        b'\'' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\n' => {
                            line += 1;
                            i += 1;
                        }
                        b'"' if b.get(i + 1) == Some(&b'"') => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'$' => match dollar_tag_end(b, i) {
                Some(tag_end) => {
                    let tag = &b[i..tag_end];
                    i = tag_end;
                    while i < b.len() {
                        if b[i..].starts_with(tag) {
                            i += tag.len();
                            break;
                        }
                        if b[i] == b'\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }
    out
}
