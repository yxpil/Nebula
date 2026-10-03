//! SQL-injection tests for the Nebula parser.
//!
//! Nebula builds a strongly-typed AST; the engine only ever compares bound
//! values, never concatenates strings into SQL. These tests prove the parser's
//! front line of defense:
//!   - a single-quoted string with `''` escaping keeps an injected boolean
//!     tautology as **opaque data** inside the literal, never as keywords;
//!   -- comment sequences, stacked dangerous statements, LIKE inner wildcards
//!     and string-typed numeric predicates are rejected rather than silently
//!     reinterpreted;
//!   - an XSS payload stored as a string literal stays a string.

use nebula_sql::{parse, parse_script, Expr, Literal, Statement};

#[test]
fn escaped_quote_keeps_boolean_tautology_as_string_data() {
    // 'x'' OR ''1''=''1'  — every doubled quote is an escaped literal quote,
    // so the whole thing is ONE string, not `x' OR '1'='1` becoming keywords.
    let s = parse("SELECT * FROM memories WHERE keyword = 'x'' OR ''1''=''1'").unwrap();
    match s {
        Statement::Select(sel) => {
            assert_eq!(
                sel.filter,
                Some(Expr::KeywordEq("x' OR '1'='1".to_string())),
                "the injected OR must live inside the string literal"
            );
        }
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn comment_style_injection_is_rejected() {
    // The lexer has no comment tokens: '-' is an unexpected character, so
    // `... WHERE id = 1 -- DROP` cannot comment out the rest of the statement.
    assert!(
        parse("SELECT * FROM memories WHERE id = 1 -- drop table memories").is_err(),
        "line comments must not be tolerated"
    );
    assert!(parse("SELECT * FROM memories /* no block comments */").is_err());
}

#[test]
fn stacked_dangerous_statements_are_rejected() {
    // parse_script runs multiple statements, but every non-DDL verb is
    // restricted; a stacked "DROP TABLE" / dynamic-SQL attempt must fail.
    assert!(
        parse_script("SELECT * FROM memories; DROP TABLE users;").is_err(),
        "DROP TABLE is not a supported statement"
    );
    assert!(
        parse_script("SELECT * FROM memories; EXECUTE IMMEDIATE 'x';").is_err(),
        "dynamic SQL is unsupported"
    );
    // A single statement must not tolerate trailing tokens (no in-place stacking).
    assert!(parse("SELECT * FROM memories; DELETE FROM memories").is_err());
}

#[test]
fn like_inner_wildcard_injection_is_rejected() {
    // Only '%substring%' is allowed; an injected OR hidden inside the pattern
    // must be rejected, not silently widened.
    assert!(
        parse("SELECT * FROM memories WHERE content LIKE '%a% OR 1=1 --%'").is_err(),
        "LIKE wildcards inside the pattern are rejected"
    );
}

#[test]
fn string_where_an_integer_is_required_is_rejected() {
    // `id` compares against a u64; a string pretending to be a number must be
    // a type error, never coerced.
    assert!(
        parse("SELECT * FROM memories WHERE id = '1 OR 1=1'").is_err(),
        "id needs a non-negative integer, not a string"
    );
}

#[test]
fn xss_payload_stays_a_string_literal() {
    let s = parse("INSERT INTO memories VALUES ('<script>alert(1)</script>', 'x', 'cli', 0.5)").unwrap();
    match s {
        Statement::Insert(i) => {
            assert_eq!(
                i.values[0],
                Literal::Str("<script>alert(1)</script>".to_string())
            );
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn legitimate_boolean_or_still_parses_as_or_control() {
    // Control: a real OR between two typed predicates is a valid AST node — we
    // are not blanket-rejecting OR, only keeping it out of string literals.
    let s = parse("SELECT * FROM memories WHERE id = 1 OR importance > 0.5").unwrap();
    match s {
        Statement::Select(sel) => {
            assert!(matches!(sel.filter, Some(Expr::Or(..))));
        }
        other => panic!("expected Select, got {other:?}"),
    }
}
