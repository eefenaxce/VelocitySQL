//! Array subscript: `expr[index]` against PostgreSQL's one-based indexing.
//!
//! An array value is indexed directly; the text form PostgreSQL's arrays use
//! (`{1,2,3}`) is parsed on the fly, which is how clients pick elements out of
//! the catalogue's text-typed `oidvector`/`int2vector` columns.

use std::sync::Arc;

use vsql_executor::{Engine, QueryResult, Session};

fn session() -> Session {
    Session::new(Arc::new(Engine::new()))
}

fn run(session: &mut Session, sql: &str) -> QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected one result for `{sql}`");
    results.remove(0)
}

fn cells(session: &mut Session, sql: &str) -> Vec<Vec<String>> {
    run(session, sql)
        .data()
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| {
                    if value.is_null() {
                        "NULL".to_string()
                    } else {
                        value.to_string()
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn literal_arrays_are_indexed_one_based() {
    let mut session = session();
    let rows = cells(
        &mut session,
        "SELECT (ARRAY['a','b'])[1], (ARRAY['a','b'])[2]",
    );
    assert_eq!(rows, vec![vec!["a".to_string(), "b".to_string()]]);
}

#[test]
fn text_vector_literals_parse_like_postgresql() {
    let mut session = session();
    let rows = cells(&mut session, "SELECT ('{1,2,3}')[1], ('{a,NULL,c}')[2]");
    assert_eq!(rows, vec![vec!["1".to_string(), "NULL".to_string()]]);
}

#[test]
fn out_of_range_subscripts_yield_null() {
    let mut session = session();
    // Zero and negative subscripts have nothing sensible to address here, so
    // they behave like an out-of-range access: NULL, not an error.
    let rows = cells(
        &mut session,
        "SELECT (ARRAY[1,2])[3], (ARRAY[1,2])[0] IS NULL",
    );
    assert_eq!(rows, vec![vec!["NULL".to_string(), "t".to_string()]]);
}

#[test]
fn array_columns_subscript_by_column_reference() {
    let mut session = session();
    run(&mut session, "CREATE TABLE rm (m int[])");
    run(&mut session, "INSERT INTO rm VALUES (ARRAY[1,2,3])");
    let rows = cells(&mut session, "SELECT m[1], m[9] FROM rm");
    assert_eq!(rows, vec![vec!["1".to_string(), "NULL".to_string()]]);
}

#[test]
fn chained_subscripts_fold_left_to_right() {
    let mut session = session();
    let rows = cells(&mut session, "SELECT (ARRAY[ARRAY[1,2],ARRAY[3,4]])[2][1]");
    assert_eq!(rows, vec![vec!["3".to_string()]]);
}

#[test]
fn parenthesised_row_access_reads_a_qualified_column() {
    let mut session = session();
    run(&mut session, "CREATE TABLE rm (m int[])");
    run(&mut session, "INSERT INTO rm VALUES (ARRAY[1,2,3])");
    // `(alias).column` is a parenthesised spelling of `alias.column`, which
    // clients emit for row-type field access; it may chain into subscripts.
    let rows = cells(&mut session, "SELECT (rm).m[2] FROM rm");
    assert_eq!(rows, vec![vec!["2".to_string()]]);
    // Field access on a non-column base stays rejected.
    let error = session
        .execute("SELECT (ARRAY[1,2]).m")
        .unwrap_err()
        .to_string();
    assert!(error.contains("not supported"), "{error}");
}

#[test]
fn array_slices_are_rejected() {
    let mut session = session();
    let error = session
        .execute("SELECT (ARRAY[1,2])[1:2]")
        .unwrap_err()
        .to_string();
    assert!(error.contains("not supported"), "{error}");
}
