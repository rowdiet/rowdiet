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
