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
