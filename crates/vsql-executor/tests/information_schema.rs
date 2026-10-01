//! `information_schema` views and the standard pseudo-types.

use std::sync::Arc;

use vsql_executor::{Engine, Session};

fn session() -> Session {
    Session::new(Arc::new(Engine::new()))
}

fn run(session: &mut Session, sql: &str) -> vsql_executor::QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected one result for `{sql}`");
    results.remove(0)
}

fn text_rows(session: &mut Session, sql: &str) -> Vec<String> {
    let result = run(session, sql);
    result
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
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn the_standard_pseudo_types_are_ordinary_types() {
    let mut session = session();
    // Tools cast catalogue values to these when decorating their output.
    run(
        &mut session,
        "SELECT CAST('x' AS information_schema.character_data), \
         CAST('x' AS information_schema.sql_identifier), \
         CAST(1 AS information_schema.cardinal_number), \
         CAST('YES' AS information_schema.yes_or_no)",
    );
}

#[test]
fn schemata_lists_the_builtin_schemas() {
    let mut session = session();
    let rows = text_rows(
        &mut session,
        "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name",
    );
    assert!(rows.contains(&"pg_catalog".to_string()), "{rows:?}");
    assert!(rows.contains(&"public".to_string()), "{rows:?}");
    assert!(rows.contains(&"information_schema".to_string()), "{rows:?}");
}

#[test]
fn tables_and_columns_describe_user_tables() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int NOT NULL, label varchar(20) DEFAULT 'x')",
    );

    let tables = text_rows(
        &mut session,
        "SELECT table_schema, table_name, table_type FROM information_schema.tables \
         WHERE table_schema = 'public' ORDER BY table_name",
    );
    assert_eq!(tables, vec!["public|items|BASE TABLE".to_string()]);

    let columns = text_rows(
        &mut session,
        "SELECT column_name, ordinal_position, is_nullable, data_type, character_maximum_length, column_default \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'items' ORDER BY ordinal_position",
    );
    assert_eq!(
        columns,
        vec![
            "id|1|NO|integer|NULL|NULL".to_string(),
            "label|2|YES|character varying|20|'x'".to_string(),
        ]
    );
}

#[test]
fn table_constraints_and_key_columns_match_the_keys() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int PRIMARY KEY, code text UNIQUE)",
    );

    let constraints = text_rows(
        &mut session,
        "SELECT constraint_name, constraint_type FROM information_schema.table_constraints \
         WHERE table_name = 'items' ORDER BY constraint_name",
    );
    assert_eq!(
        constraints,
        vec![
            "items_code_key|UNIQUE".to_string(),
            "items_pkey|PRIMARY KEY".to_string(),
        ]
    );

    let keys = text_rows(
        &mut session,
        "SELECT constraint_name, column_name, ordinal_position FROM information_schema.key_column_usage \
         WHERE table_name = 'items' ORDER BY constraint_name, ordinal_position",
    );
    assert_eq!(
        keys,
        vec![
            "items_code_key|code|1".to_string(),
            "items_pkey|id|1".to_string(),
        ]
    );
}

#[test]
fn empty_information_schema_relations_answer() {
    let mut session = session();
    for relation in [
        "referential_constraints",
        "check_constraints",
        "routines",
        "triggers",
        "sequences",
    ] {
        let rows = text_rows(
            &mut session,
            &format!("SELECT count(*) FROM information_schema.{relation}"),
        );
        assert_eq!(rows, vec!["0".to_string()], "{relation}");
    }
}

/// Every column the view declares must have a value in every row: the columns
/// view is the widest one, and a schema/row mismatch shows up as `column slot
/// N is out of range` only once a table actually has columns to report.
#[test]
fn the_columns_view_stays_in_step_with_its_schema() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int PRIMARY KEY, label text)",
    );
    run(&mut session, "INSERT INTO items VALUES (1, 'x')");

    // A whole-row projection walks every declared column in order.
    let rows = run(
        &mut session,
        "SELECT * FROM information_schema.columns WHERE table_name = 'items'",
    )
    .data()
    .to_vec();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), rows[1].len(), "every row has the same arity");

    // The columns client metadata queries reach for, by name.
    let columns = text_rows(
        &mut session,
        "SELECT column_name, domain_catalog, domain_schema, udt_catalog, udt_schema, udt_name \
         FROM information_schema.columns WHERE table_name = 'items' ORDER BY ordinal_position",
    );
    assert_eq!(
        columns,
        vec![
            "id|NULL|NULL|NULL|pg_catalog|int4".to_string(),
            "label|NULL|NULL|NULL|pg_catalog|text".to_string(),
        ]
    );
}

/// Clients that put `information_schema` on their search path ask for
/// `parameters` by its bare name. The view answers with no rows, because the
/// engine has no functions whose parameters it could report.
#[test]
fn bare_information_schema_names_resolve_when_no_user_relation_claims_them() {
    let mut session = session();
    let rows = text_rows(
        &mut session,
        "SELECT count(*), min(parameter_mode) FROM parameters",
    );
    assert_eq!(rows, vec!["0|NULL".to_string()]);
}

/// The fallback never shadows a user relation: a table called `parameters` is
/// still the table.
#[test]
fn a_user_table_beats_the_information_schema_fallback() {
    let mut session = session();
    run(&mut session, "CREATE TABLE parameters (x INT)");
    run(&mut session, "INSERT INTO parameters VALUES (7)");
    assert_eq!(
        text_rows(&mut session, "SELECT x FROM parameters"),
        vec!["7".to_string()]
    );
}
