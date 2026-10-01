//! Session configuration parameters (`SET` / `SHOW` / `ParameterStatus`).
//!
//! libpq-based clients inspect these at connect time - some run `SHOW
//! datestyle` before they consider a session usable - so the standard
//! PostgreSQL parameters must answer instead of failing.

use std::sync::Arc;

use vsql_executor::{ExecError, QueryResult, Session};
use vsql_types::Value;

fn session() -> Session {
    Session::new(Arc::new(vsql_executor::Engine::new()))
}

fn run(session: &mut Session, sql: &str) -> QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected one result for `{sql}`");
    results.remove(0)
}

fn text(session: &mut Session, sql: &str) -> String {
    match run(session, sql) {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "`{sql}` returned more than one row");
            match &rows[0][0] {
                Value::Text(text) => text.clone(),
                other => panic!("`{sql}` returned {other:?}"),
            }
        }
        other => panic!("`{sql}` returned {other:?}"),
    }
}

fn failure(session: &mut Session, sql: &str) -> ExecError {
    match session.execute(sql) {
        Ok(_) => panic!("`{sql}` unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[test]
fn standard_parameters_answer_show() {
    let mut session = session();
    // The case that broke a client: `SHOW datestyle` after connecting.
    assert_eq!(text(&mut session, "SHOW datestyle"), "ISO, MDY");
    // Names are case-insensitive, as in PostgreSQL.
    assert_eq!(text(&mut session, "SHOW DateStyle"), "ISO, MDY");
    assert_eq!(text(&mut session, "SHOW DATESTYLE"), "ISO, MDY");

    assert_eq!(text(&mut session, "SHOW timezone"), "UTC");
    assert_eq!(text(&mut session, "SHOW TimeZone"), "UTC");
    assert_eq!(text(&mut session, "SHOW intervalstyle"), "postgres");
    assert_eq!(text(&mut session, "SHOW extra_float_digits"), "1");
    assert_eq!(text(&mut session, "SHOW bytea_output"), "hex");
    assert_eq!(text(&mut session, "SHOW integer_datetimes"), "on");
    assert_eq!(text(&mut session, "SHOW server_encoding"), "UTF8");
    assert_eq!(text(&mut session, "SHOW client_encoding"), "UTF8");
    assert_eq!(
        text(&mut session, "SHOW session_authorization"),
        "velocitysql"
    );
    assert_eq!(text(&mut session, "SHOW is_superuser"), "on");
    assert_eq!(
        text(&mut session, "SHOW server_version"),
        format!("15.0 (VelocitySQL {})", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        text(&mut session, "SHOW transaction_isolation"),
        "read committed"
    );
}

#[test]
fn set_changes_and_show_reports_it() {
    let mut session = session();
    run(&mut session, "SET datestyle TO 'ISO, DMY'");
    assert_eq!(text(&mut session, "SHOW datestyle"), "ISO, DMY");

    // Quoting styles clients actually use.
    run(&mut session, "SET TIME ZONE 'Asia/Shanghai'");
    assert_eq!(text(&mut session, "SHOW timezone"), "Asia/Shanghai");
    run(&mut session, "SET application_name = 'dbeaver'");
    assert_eq!(text(&mut session, "SHOW application_name"), "dbeaver");

    // `TO DEFAULT` forgets the override instead of storing the word "DEFAULT".
    run(&mut session, "SET datestyle TO DEFAULT");
    assert_eq!(text(&mut session, "SHOW datestyle"), "ISO, MDY");

    // The current transaction's read-only state stays separate from the default.
    assert_eq!(text(&mut session, "SHOW transaction_read_only"), "off");
    assert_eq!(
        text(&mut session, "SHOW default_transaction_read_only"),
        "off"
    );
}

#[test]
fn unknown_parameters_are_still_refused() {
    let mut session = session();
    let error = failure(&mut session, "SHOW this_parameter_does_not_exist");
    assert_eq!(
        error.to_string(),
        "unrecognized configuration parameter \"this_parameter_does_not_exist\""
    );
}

#[test]
fn read_only_parameters_cannot_be_set() {
    let mut session = session();
    let error = failure(&mut session, "SET server_version TO '9.0'");
    assert_eq!(
        error.to_string(),
        "parameter \"server_version\" cannot be changed"
    );
    assert_eq!(error.sqlstate(), "55P02");
    failure(&mut session, "SET is_superuser TO off");
    failure(&mut session, "SET session_authorization TO 'mallory'");

    // The engine cannot transcode, so it refuses instead of lying.
    let error = failure(&mut session, "SET client_encoding TO 'LATIN1'");
    assert!(error.to_string().contains("LATIN1"), "{error}");
    assert_eq!(error.sqlstate(), "0A000");
}

#[test]
fn show_all_lists_every_parameter() {
    let mut session = session();
    let rows = match run(&mut session, "SHOW ALL") {
        QueryResult::Rows { schema, rows } => {
            let names: Vec<&str> = schema.iter().map(|field| field.name.as_str()).collect();
            assert_eq!(names, vec!["name", "setting", "description"], "{schema:?}");
            rows
        }
        other => panic!("SHOW ALL returned {other:?}"),
    };
    let value_of = |name: &str| -> String {
        rows.iter()
            .find(|row| row[0] == Value::Text(name.to_string()))
            .unwrap_or_else(|| panic!("{name} missing from SHOW ALL"))[1]
            .to_string()
    };
    assert_eq!(value_of("datestyle"), "ISO, MDY");
    assert_eq!(value_of("search_path"), "\"$user\", public");
    assert_eq!(value_of("statement_timeout"), "0");
    assert_eq!(value_of("server_version_num"), "150000");
}

#[test]
fn changed_parameters_are_reported_for_parameter_status() {
    let mut session = session();

    // Nothing changed yet.
    assert!(session.take_parameter_updates().is_empty());

    run(&mut session, "SET TIME ZONE 'Asia/Shanghai'");
    let updates = session.take_parameter_updates();
    assert_eq!(
        updates,
        vec![("TimeZone".to_string(), "Asia/Shanghai".to_string())]
    );

    // Drained: the wire layer reports each change exactly once.
    assert!(session.take_parameter_updates().is_empty());

    // A parameter outside PostgreSQL's reported set produces no update.
    run(&mut session, "SET statement_timeout TO '5s'");
    assert!(session.take_parameter_updates().is_empty());
}

#[test]
fn the_startup_parameter_list_matches_postgresql() {
    let session = session();
    let reported = session.reported_parameters();
    let names: Vec<&str> = reported.iter().map(|(name, _)| *name).collect();
    // Wire names are case-sensitive: these three are mixed case.
    assert_eq!(
        names,
        vec![
            "application_name",
            "client_encoding",
            "DateStyle",
            "default_transaction_read_only",
            "in_hot_standby",
            "integer_datetimes",
            "IntervalStyle",
            "is_superuser",
            "server_encoding",
            "server_version",
            "session_authorization",
            "standard_conforming_strings",
            "TimeZone",
        ]
    );
    let value_of = |name: &str| {
        reported
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert_eq!(value_of("DateStyle"), "ISO, MDY");
    assert_eq!(value_of("is_superuser"), "on");
    assert_eq!(value_of("session_authorization"), "velocitysql");
    assert_eq!(value_of("integer_datetimes"), "on");
}

#[test]
fn application_name_comes_from_the_startup_packet() {
    let engine = Arc::new(vsql_executor::Engine::new());
    let mut session = Session::new(engine);
    assert_eq!(
        text(&mut session, "SHOW application_name"),
        "",
        "no name was announced"
    );

    session.set_parameter("application_name", "dbeaver 24.1".to_string());
    assert_eq!(text(&mut session, "SHOW application_name"), "dbeaver 24.1");
    assert!(session
        .reported_parameters()
        .iter()
        .any(|(name, value)| *name == "application_name" && value == "dbeaver 24.1"));
}
