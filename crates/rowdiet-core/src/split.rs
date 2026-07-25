//! Tolerant statement splitter. Never fails: understands PG quoting (dollar quotes, E-strings,
//! doubled quotes, nested block comments) just enough to find top-level semicolons and 1-based
//! line numbers. Whether a statement parses is the caller's per-statement concern — a linter must
//! degrade loudly per statement, never abort a whole file.

/// One top-level statement found by [`split`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStatement {
    /// The statement text, whitespace-trimmed, terminating semicolon excluded. Comments before
    /// the first content byte are dropped; interior comments (where ignore markers live) stay.
    pub text: String,
    /// 1-based line where the statement's content begins.
    pub line: u32,
}

/// Byte cursor with 1-based line tracking. The `skip_*` helpers each consume one lexical
/// construct and leave the cursor just past it; all three scanners below (statements, comment
/// markers, dollar body) walk through this one impl, so their quoting rules cannot drift apart.
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
    line: u32,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            b: text.as_bytes(),
            i: 0,
            line: 1,
        }
    }

    fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    fn at(&self) -> u8 {
        self.b[self.i]
    }

    fn peek(&self, ahead: usize) -> Option<u8> {
        self.b.get(self.i + ahead).copied()
    }

    fn starts(&self, pair: &[u8]) -> bool {
        self.b[self.i..].starts_with(pair)
    }

    /// Advance one byte, counting newlines. Saturates at the end of input.
    fn bump(&mut self) {
        if let Some(&c) = self.b.get(self.i) {
            if c == b'\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    /// Stops at the newline (counted by the caller's next bump) or at end of input.
    fn skip_line_comment(&mut self) {
        while !self.done() && self.at() != b'\n' {
            self.i += 1;
        }
    }

    /// PG block comments nest, unlike C.
    fn skip_block_comment(&mut self) {
        let mut depth = 1u32;
        self.i += 2;
        while !self.done() && depth > 0 {
            if self.starts(b"/*") {
                depth += 1;
                self.i += 2;
            } else if self.starts(b"*/") {
                depth -= 1;
                self.i += 2;
            } else {
                self.bump();
            }
        }
    }

    /// Cursor sits on the opening quote. Backslash escapes only in E'..' strings
    /// (standard_conforming_strings default); doubled '' always.
    fn skip_single_quoted(&mut self) {
        let estring =
            self.i > 0 && matches!(self.b[self.i - 1], b'e' | b'E') && (self.i < 2 || !ident_byte(self.b[self.i - 2]));
        self.bump();
        while !self.done() {
            match self.at() {
                b'\\' if estring => {
                    self.bump();
                    self.bump();
                }
                b'\'' if self.peek(1) == Some(b'\'') => {
                    self.bump();
                    self.bump();
                }
                b'\'' => {
                    self.bump();
                    return;
                }
                _ => self.bump(),
            }
        }
    }

    fn skip_double_quoted(&mut self) {
        self.bump();
        while !self.done() {
            match self.at() {
                b'"' if self.peek(1) == Some(b'"') => {
                    self.bump();
                    self.bump();
                }
                b'"' => {
                    self.bump();
                    return;
                }
                _ => self.bump(),
            }
        }
    }

    /// Cursor sits on a `$`. False when it opens no tag (`$1` stays a parameter) — the caller
    /// bumps past it as plain content. An unterminated body swallows the rest of the input.
    fn skip_dollar_quoted(&mut self) -> bool {
        let Some(tag_end) = dollar_tag_end(self.b, self.i) else {
            return false;
        };
        let tag = &self.b[self.i..tag_end];
        self.i = tag_end;
        while !self.done() {
            if self.b[self.i..].starts_with(tag) {
                self.i += tag.len();
                return true;
            }
            self.bump();
        }
        true
    }
}

/// Split `sql` at top-level semicolons. Never fails: comment-only stretches yield nothing, and a
/// trailing statement without a semicolon is still returned.
pub fn split(sql: &str) -> Vec<RawStatement> {
    let mut c = Cursor::new(sql);
    let mut out = Vec::new();
    let mut content_start: Option<(usize, u32)> = None;
    while !c.done() {
        match c.at() {
            b'-' if c.starts(b"--") => c.skip_line_comment(),
            b'/' if c.starts(b"/*") => c.skip_block_comment(),
            b';' => {
                if let Some((start, line)) = content_start.take() {
                    push_stmt(&mut out, &sql[start..c.i], line);
                }
                c.bump();
            }
            w if w.is_ascii_whitespace() => c.bump(),
            b'\'' => {
                mark(&mut content_start, &c);
                c.skip_single_quoted();
            }
            b'"' => {
                mark(&mut content_start, &c);
                c.skip_double_quoted();
            }
            b'$' => {
                mark(&mut content_start, &c);
                if !c.skip_dollar_quoted() {
                    c.bump();
                }
            }
            _ => {
                mark(&mut content_start, &c);
                c.bump();
            }
        }
    }
    if let Some((start, line)) = content_start {
        push_stmt(&mut out, &sql[start..], line);
    }
    out
}

fn mark(content_start: &mut Option<(usize, u32)>, c: &Cursor) {
    if content_start.is_none() {
        *content_start = Some((c.i, c.line));
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

pub(crate) fn ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80
}

/// The first dollar-quoted body in `text` (`$tag$ … $tag$`), comment/string-aware up to the
/// opening tag. Used to look inside `DO` blocks; None when absent or unterminated.
pub fn dollar_quoted_body(text: &str) -> Option<&str> {
    let mut c = Cursor::new(text);
    while !c.done() {
        match c.at() {
            b'-' if c.starts(b"--") => c.skip_line_comment(),
            b'/' if c.starts(b"/*") => c.skip_block_comment(),
            b'\'' => c.skip_single_quoted(),
            b'"' => c.skip_double_quoted(),
            b'$' => match dollar_tag_end(c.b, c.i) {
                Some(tag_end) => {
                    let tag = &text[c.i..tag_end];
                    let close = text[tag_end..].find(tag)?;
                    return Some(&text[tag_end..tag_end + close]);
                }
                None => c.bump(),
            },
            _ => c.bump(),
        }
    }
    None
}

/// `$tag$` (tag possibly empty, never starting with a digit — `$1` stays a parameter). Returns the
/// index just past the closing `$` of the opening tag.
fn dollar_tag_end(b: &[u8], start: usize) -> Option<usize> {
    if b.get(start + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut j = start + 1;
    while j < b.len() && ident_byte(b[j]) {
        j += 1;
    }
    if j < b.len() && b[j] == b'$' { Some(j + 1) } else { None }
}

/// 1-based lines of every occurrence of `marker` that sits inside a comment of `text` —
/// line (`--`) or block (`/* */`, nested). String literals, dollar-quoted bodies, and quoted
/// identifiers are skipped, so a marker in data can never count; the ignore-marker semantics
/// are built on this.
pub(crate) fn comment_marker_lines(text: &str, marker: &str) -> Vec<u32> {
    // Almost no input contains the marker at all, and a plain substring miss is an order of
    // magnitude cheaper than the quote-aware walk below — which only exists to decide whether
    // an occurrence sits in a comment, so zero occurrences need no walk.
    if !text.contains(marker) {
        return Vec::new();
    }
    let mut c = Cursor::new(text);
    let mut out = Vec::new();
    while !c.done() {
        match c.at() {
            b'-' if c.starts(b"--") => {
                let (start, line) = (c.i, c.line);
                c.skip_line_comment();
                count_marker(&text[start..c.i], marker, line, &mut out);
            }
            b'/' if c.starts(b"/*") => {
                let (start, line) = (c.i, c.line);
                c.skip_block_comment();
                count_marker(&text[start..c.i], marker, line, &mut out);
            }
            b'\'' => c.skip_single_quoted(),
            b'"' => c.skip_double_quoted(),
            b'$' => {
                if !c.skip_dollar_quoted() {
                    c.bump();
                }
            }
            _ => c.bump(),
        }
    }
    out
}

fn count_marker(span: &str, marker: &str, at_line: u32, out: &mut Vec<u32>) {
    let mut rel_line = 0u32;
    let mut rest = span;
    while let Some(pos) = rest.find(marker) {
        rel_line += rest[..pos].matches('\n').count() as u32;
        out.push(at_line + rel_line);
        rest = &rest[pos + marker.len()..];
    }
}

#[cfg(test)]
mod tests;
