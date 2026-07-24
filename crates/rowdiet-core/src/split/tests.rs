use super::*;

#[test]
fn basic_two_statements() {
    let stmts = split("CREATE TABLE a (x int);\nCREATE TABLE b (y int);");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, "CREATE TABLE a (x int)");
    assert_eq!(stmts[0].line, 1);
    assert_eq!(stmts[1].line, 2);
}

#[test]
fn dollar_quoted_do_block_with_inner_semicolons() {
    let sql = "\nCREATE TABLE a (x int);\nDO $mig$ BEGIN CREATE TYPE st AS ENUM ('a;b','c'); END $mig$;\nCREATE UNLOGGED TABLE b (y text);\nALTER TABLE a ADD COLUMN z timestamptz NOT NULL;\n";
    let stmts = split(sql);
    assert_eq!(stmts.len(), 4);
    assert!(stmts[1].text.starts_with("DO $mig$"));
    assert!(stmts[1].text.contains("'a;b'"));
    assert_eq!(stmts.iter().map(|s| s.line).collect::<Vec<_>>(), vec![2, 3, 4, 5]);
}

#[test]
fn nested_block_comments() {
    let stmts = split("/* outer /* inner; */ still; */ CREATE TABLE t (a int)");
    assert_eq!(stmts.len(), 1);
    assert_eq!(stmts[0].text, "CREATE TABLE t (a int)");
}

#[test]
fn line_comment_hides_semicolon() {
    let stmts = split("CREATE TABLE t ( -- trailing; comment\n a int);");
    assert_eq!(stmts.len(), 1);
    assert!(stmts[0].text.contains("a int"));
    assert_eq!(stmts[0].line, 1);
}

#[test]
fn estring_backslash_escape_keeps_string_open() {
    let stmts = split("INSERT INTO t VALUES (E'a\\';b');INSERT INTO t VALUES (2);");
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].text.contains(";b"));
}

#[test]
fn standard_string_doubles_quotes() {
    let stmts = split("INSERT INTO t VALUES ('it''s; fine');SELECT 1;");
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].text.contains("it''s; fine"));
}

#[test]
fn double_quoted_identifier() {
    let stmts = split("CREATE TABLE \"we;ird\" (a int);");
    assert_eq!(stmts.len(), 1);
    assert!(stmts[0].text.contains("\"we;ird\""));
}

#[test]
fn statement_line_skips_leading_comments() {
    let stmts = split("-- hi\n\n-- more\nCREATE TABLE t (a int);");
    assert_eq!(stmts.len(), 1);
    assert_eq!(stmts[0].line, 4);
    assert!(stmts[0].text.starts_with("CREATE"));
}

#[test]
fn no_trailing_semicolon() {
    let stmts = split("CREATE TABLE t (a int)");
    assert_eq!(stmts.len(), 1);
}

#[test]
fn comment_only_input() {
    assert!(split("-- nothing\n/* here */").is_empty());
    assert!(split("  \n\t").is_empty());
}

#[test]
fn unterminated_dollar_quote_consumes_tail() {
    let stmts = split("DO $x$ oops");
    assert_eq!(stmts.len(), 1);
    assert_eq!(stmts[0].text, "DO $x$ oops");
}

#[test]
fn dollar_parameter_is_not_a_quote() {
    let stmts = split("SELECT foo($1);ALTER TABLE t ADD b int;");
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].text.contains("$1"));
}

#[test]
fn anonymous_dollar_tag() {
    let stmts = split("DO $$ BEGIN NULL; END $$;SELECT 1;");
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].text.contains("BEGIN NULL;"));
}

// Additions from the first mutation campaign: 120 of the splitter's 251 mutants survived the
// suite — 65 of them in `dollar_quoted_body` (reached by e2e DO tests only on plain `$$` bodies)
// and the rest in split()'s uncommon-input branches (bare `-`/`/` outside comments, mismatched
// dollar tags, unterminated forms, line accounting across multi-line constructs).

#[test]
fn mismatched_dollar_tags_stay_inside_the_body() {
    let stmts = split("DO $fn$ a; $other$ still inside; $fn$; SELECT 4");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, "DO $fn$ a; $other$ still inside; $fn$");
    assert_eq!(stmts[1].text, "SELECT 4");
}

#[test]
fn bare_minus_and_slash_are_not_comment_openers() {
    let stmts = split("SELECT 1 - 2 / 3; SELECT 4");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, "SELECT 1 - 2 / 3");
}

#[test]
fn plain_string_backslash_is_data_not_escape() {
    let stmts = split(r"SELECT 'a\'; SELECT 2");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, r"SELECT 'a\'");
    // An identifier merely ending in `e` does not make the following string an E-string.
    let stmts = split(r"SELECT code'\'; SELECT 3");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, r"SELECT code'\'");
}

#[test]
fn line_numbers_account_for_multiline_constructs() {
    let sql = "-- one\n/* two\nthree */ CREATE TABLE a (x int);\nSELECT 'multi\nline';\nCREATE TABLE b (y int);";
    let stmts = split(sql);
    assert_eq!(stmts.len(), 3);
    assert_eq!((stmts[0].text.as_str(), stmts[0].line), ("CREATE TABLE a (x int)", 3));
    assert_eq!((stmts[1].text.as_str(), stmts[1].line), ("SELECT 'multi\nline'", 4));
    assert_eq!((stmts[2].text.as_str(), stmts[2].line), ("CREATE TABLE b (y int)", 6));
    let body = "DO $$\nline\nlines\n$$;\nCREATE TABLE c (z int);";
    let stmts = split(body);
    assert_eq!(stmts[1].line, 5);
}

#[test]
fn unterminated_forms_swallow_the_rest_without_panicking() {
    assert_eq!(
        split("SELECT 'open; SELECT hidden")[0].text,
        "SELECT 'open; SELECT hidden"
    );
    assert_eq!(
        split("SELECT \"open; SELECT hidden")[0].text,
        "SELECT \"open; SELECT hidden"
    );
    assert!(split("/* never closed ; SELECT hidden").is_empty());
    // Once content has started, the unterminated remainder is emitted whole, never dropped.
    assert_eq!(
        split("x /* never closed ; SELECT hidden")[0].text,
        "x /* never closed ; SELECT hidden"
    );
}

mod dollar_body {
    use super::super::dollar_quoted_body;

    #[test]
    fn finds_the_body_past_comments_and_strings() {
        assert_eq!(dollar_quoted_body("DO $$body$$"), Some("body"));
        assert_eq!(dollar_quoted_body("DO /* lang 'plpgsql' */ $$body$$"), Some("body"));
        assert_eq!(dollar_quoted_body("DO -- $$ not this $$\n$$real$$"), Some("real"));
        assert_eq!(dollar_quoted_body("DO LANGUAGE 'plpgsql' $$real$$"), Some("real"));
        assert_eq!(
            dollar_quoted_body("DO $fn$a $$ inner $$ b$fn$"),
            Some("a $$ inner $$ b")
        );
        assert_eq!(dollar_quoted_body("DO /* a /* nested */ ; */ $$x$$"), Some("x"));
        assert_eq!(dollar_quoted_body("DO 'it''s' $$q$$"), Some("q"));
    }

    #[test]
    fn absent_or_unterminated_bodies_are_none() {
        assert_eq!(dollar_quoted_body("DO nothing here"), None);
        assert_eq!(dollar_quoted_body("DO $$never closed"), None);
        assert_eq!(dollar_quoted_body("DO '$$ inside a string $$'"), None);
        assert_eq!(dollar_quoted_body("SELECT $1 + 2"), None);
    }
}

#[test]
fn eof_inside_line_comment_does_not_overrun() {
    // No trailing newline: the comment skip must stop at len, not index past it.
    assert!(split("-- comment at eof").is_empty());
    assert_eq!(split("SELECT 1; -- trailing at eof")[0].text, "SELECT 1");
}

#[test]
fn lone_star_and_slash_inside_block_comments_are_data() {
    let stmts = split("/* a * b / c */ SELECT 1;");
    assert_eq!(stmts.len(), 1);
    assert_eq!(stmts[0].text, "SELECT 1");
    let nested = split("/* o /* i */ tail * / */ SELECT 2;");
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].text, "SELECT 2");
}

#[test]
fn statement_initial_quotes_do_not_underflow() {
    // Statements that BEGIN with quoting exercise the i==0 guards of the E-string detector.
    assert_eq!(split("'lead'; SELECT 1").len(), 2);
    assert_eq!(split("E'a\\';x'; SELECT 1").len(), 2);
    assert_eq!(split("e'a\\';x'; SELECT 1").len(), 2);
    assert_eq!(split("\"q\"; SELECT 1").len(), 2);
    assert_eq!(split("$$b$$; SELECT 1").len(), 2);
}

#[test]
fn trailing_blank_runs_do_not_shift_statement_lines() {
    // Whitespace between statements must not mark content: the space before the newline would
    // otherwise pin the next statement to the previous line.
    let stmts = split("SELECT 1;   \n\t \nSELECT 2;");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[1].line, 3);
}

#[test]
fn nested_comment_close_positions_are_exact() {
    // The nest bookkeeping must resume content exactly after the true close.
    let stmts = split("/* x /* y */ z */abc; SELECT 1;");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, "abc");
}

/// Property: for generated scripts assembled from known statements wrapped in random quoting
/// and comment decorations, split() recovers exactly the statement texts and 1-based lines.
/// Complements the deterministic gauntlet with breadth — scanner arithmetic that survives
/// point tests corrupts some generated case instead.
mod round_trip_property {
    use super::super::split;
    use proptest::prelude::*;

    fn newlines(s: &str) -> u32 {
        s.matches('\n').count() as u32
    }

    fn piece() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z][a-z0-9_]{0,5}".prop_map(|w| w),
            Just("1 - 2 / 3".to_string()),
            "[a-z ;]{0,8}".prop_map(|inner| format!("'{inner}'")),
            proptest::collection::vec(
                prop_oneof![Just("ab".to_string()), Just("\\'".to_string()), Just(";".to_string())],
                0..4
            )
            .prop_map(|parts| format!("E'{}'", parts.concat())),
            "[a-z;]{1,6}".prop_map(|i| format!("\"{i}\"")),
            (prop_oneof![Just(""), Just("x"), Just("fn")], "[a-z ;]{0,10}")
                .prop_map(|(tag, inner)| format!("${tag}${inner}${tag}$")),
        ]
    }

    fn statement() -> impl Strategy<Value = String> {
        proptest::collection::vec(piece(), 1..=3).prop_map(|pieces| pieces.join(" "))
    }

    fn filler() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(" ".to_string()),
            Just("\n".to_string()),
            Just("  \n\t ".to_string()),
            "[a-z ;]{0,6}".prop_map(|c| format!("-- {c}\n")),
            "[a-z ;]{0,6}".prop_map(|c| format!("/* {c} */ ")),
            "[a-z ;]{0,4}".prop_map(|c| format!("/* a /* {c} */ b */\n")),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn split_round_trips_generated_scripts(
            stmts in proptest::collection::vec((proptest::collection::vec(filler(), 0..3), statement()), 1..5),
            trailing in proptest::option::of(filler()),
        ) {
            let mut script = String::new();
            let mut expected: Vec<(String, u32)> = Vec::new();
            let mut line = 1u32;
            for (fillers, text) in &stmts {
                for f in fillers {
                    script.push_str(f);
                    line += newlines(f);
                }
                expected.push((text.clone(), line));
                script.push_str(text);
                line += newlines(text);
                script.push_str(";\n");
                line += 1;
            }
            if let Some(t) = trailing {
                script.push_str(&t);
            }
            let got: Vec<(String, u32)> = split(&script).into_iter().map(|s| (s.text, s.line)).collect();
            prop_assert_eq!(got, expected, "script: {:?}", script);
        }
    }
}

#[test]
fn estring_escaped_newline_still_counts_the_line() {
    // A backslash before a newline inside an E-string skipped the newline uncounted, shifting
    // every later statement's reported line by one.
    let sql = "INSERT INTO t VALUES (E'a\\\n b');\n\nCREATE TABLE t9 (x int);";
    let stmts = split(sql);
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[1].line, 4, "{stmts:#?}");
}

/// Property for `comment_marker_lines`: markers injected into generated comments must be
/// found at exactly their lines; markers inside string literals, dollar bodies, and quoted
/// identifiers must never count. The scanner duplicates split()'s quoting rules, so it gets
/// the same generator treatment (its first mutation campaign left it almost unpinned).
mod marker_property {
    use super::super::comment_marker_lines;
    use proptest::prelude::*;

    const MARK: &str = "rowdiet:ignore";

    #[derive(Clone, Debug)]
    enum Piece {
        LineCommentMarked,
        BlockCommentMarked,
        LineComment,
        PlainStringMarked,
        DollarBodyMarked,
        QuotedIdentMarked,
        Code,
        Newline,
    }

    fn piece() -> impl Strategy<Value = Piece> {
        prop_oneof![
            Just(Piece::LineCommentMarked),
            Just(Piece::BlockCommentMarked),
            Just(Piece::LineComment),
            Just(Piece::PlainStringMarked),
            Just(Piece::DollarBodyMarked),
            Just(Piece::QuotedIdentMarked),
            Just(Piece::Code),
            Just(Piece::Newline),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(192))]
        #[test]
        fn finds_exactly_the_comment_markers(pieces in proptest::collection::vec(piece(), 1..14)) {
            let mut text = String::new();
            let mut line = 1u32;
            let mut expected: Vec<u32> = Vec::new();
            for p in &pieces {
                match p {
                    Piece::LineCommentMarked => {
                        text.push_str(&format!("-- x {MARK} y\n"));
                        expected.push(line);
                        line += 1;
                    }
                    Piece::BlockCommentMarked => {
                        text.push_str(&format!("/* a\n{MARK} */ "));
                        expected.push(line + 1);
                        line += 1;
                    }
                    Piece::LineComment => {
                        text.push_str("-- nothing here\n");
                        line += 1;
                    }
                    Piece::PlainStringMarked => text.push_str(&format!("'{MARK}' ")),
                    Piece::DollarBodyMarked => text.push_str(&format!("$x$ -- {MARK}\n oops $x$ ")),
                    Piece::QuotedIdentMarked => text.push_str(&format!("\"{MARK}\" ")),
                    Piece::Code => text.push_str("select 1 "),
                    Piece::Newline => {
                        text.push('\n');
                        line += 1;
                    }
                }
                if matches!(p, Piece::DollarBodyMarked) {
                    line += 1;
                }
            }
            let got = comment_marker_lines(&text, MARK);
            prop_assert_eq!(got, expected, "text: {:?}", text);
        }
    }
}
