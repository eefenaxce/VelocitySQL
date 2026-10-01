//! Roles, passwords and privileges.
//!
//! Passwords are never stored in the clear: a role holds a SCRAM-SHA-256 verifier
//! in PostgreSQL's own `pg_authid.rolpassword` format, and authentication proves
//! knowledge of the password without transmitting it.

use std::sync::Arc;

use vsql_executor::{Engine, ExecError, QueryResult, Session, DEFAULT_PASSWORD, DEFAULT_SUPERUSER};
use vsql_types::Value;

fn engine() -> Arc<Engine> {
    Arc::new(Engine::new())
}

fn run(session: &mut Session, sql: &str) -> QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected one result for `{sql}`");
    results.remove(0)
}

fn text_rows(session: &mut Session, sql: &str) -> Vec<String> {
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
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn failure(session: &mut Session, sql: &str) -> ExecError {
    match session.execute(sql) {
        Ok(_) => panic!("`{sql}` unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[test]
fn the_engine_starts_with_a_login_superuser() {
    let engine = engine();
    let role = engine.role(DEFAULT_SUPERUSER).expect("bootstrap role");
    assert!(role.superuser && role.can_login && role.create_db && role.create_role);
    assert!(
        role.check_password(DEFAULT_PASSWORD),
        "the documented default password must work"
    );
    assert_eq!(engine.superuser_name(), DEFAULT_SUPERUSER);
    assert_eq!(Session::new(engine).user(), DEFAULT_SUPERUSER);
}

#[test]
fn create_user_creates_a_login_role_with_a_password() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());

    run(&mut admin, "CREATE USER app PASSWORD 's3cret'");
    let role = engine.role("app").expect("app exists");
    assert!(role.can_login, "CREATE USER implies LOGIN");
    assert!(!role.superuser && !role.create_db && !role.create_role);
    assert!(role.check_password("s3cret"));
    assert!(!role.check_password("s3cre"));
    assert!(!role.check_password(""));

    // `CREATE ROLE` keeps PostgreSQL's NOLOGIN default.
    run(&mut admin, "CREATE ROLE job");
    assert!(!engine.role("job").unwrap().can_login);
}

#[test]
fn passwords_are_stored_as_scram_verifiers() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(
        &mut admin,
        "CREATE ROLE app LOGIN PASSWORD 'plaintext-password'",
    );

    let stored = engine.role("app").unwrap().password.expect("a password");
    assert!(
        stored.starts_with("SCRAM-SHA-256$4096:"),
        "PostgreSQL's stored format: {stored}"
    );
    assert!(
        !stored.contains("plaintext-password"),
        "the cleartext must never be stored: {stored}"
    );
    // The verifier round-trips through its own textual form.
    let verifier = vsql_catalog::ScramVerifier::parse(&stored).expect("a parseable verifier");
    assert!(verifier.verify_password("plaintext-password"));

    // Two roles with the same password must not share a verifier: the salt is
    // per role.
    run(
        &mut admin,
        "CREATE ROLE twin LOGIN PASSWORD 'plaintext-password'",
    );
    assert_ne!(
        stored,
        engine.role("twin").unwrap().password.unwrap(),
        "salts must differ"
    );
}

#[test]
fn login_checks_the_password() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");

    let session = Session::login(engine.clone(), "velocitysql", "app", Some("s3cret"))
        .expect("the right password connects");
    assert_eq!(session.user(), "app");
    assert!(!session.is_superuser());

    let error = Session::login(engine.clone(), "velocitysql", "app", Some("nope")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "password authentication failed for user \"app\""
    );
    assert_eq!(error.sqlstate(), "28P01");

    // A missing password is not an empty one.
    assert!(Session::login(engine.clone(), "velocitysql", "app", None).is_err());

    // The bootstrap role still uses the documented password.
    assert!(Session::login(
        engine,
        "velocitysql",
        DEFAULT_SUPERUSER,
        Some(DEFAULT_PASSWORD)
    )
    .is_ok());
}

#[test]
fn an_unknown_role_is_indistinguishable_from_a_wrong_password() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");

    let unknown =
        Session::login(engine.clone(), "velocitysql", "ghost", Some("s3cret")).unwrap_err();
    let wrong = Session::login(engine, "velocitysql", "app", Some("s3cret!")).unwrap_err();

    // The name is echoed in PostgreSQL's message as well, so the two differ only
    // by it; the shape of the failure is identical, which is what a client can
    // observe. The decoy challenge that keeps the *handshake* uniform is covered
    // by the protocol tests, where the exchange is actually visible.
    assert_eq!(unknown.sqlstate(), wrong.sqlstate());
    assert_eq!(unknown.sqlstate(), "28P01");
    assert_eq!(
        unknown.to_string().replace("ghost", "<role>"),
        wrong.to_string().replace("app", "<role>")
    );
}

#[test]
fn a_role_without_login_is_refused() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE silent");
    let error = Session::login(engine, "velocitysql", "silent", None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "role \"silent\" is not permitted to log in"
    );
    assert_eq!(error.sqlstate(), "28000");
}

#[test]
fn only_createrole_may_manage_roles() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");

    let mut app = Session::login(engine.clone(), "velocitysql", "app", Some("s3cret")).unwrap();
    let error = failure(&mut app, "CREATE ROLE other");
    assert_eq!(error.to_string(), "permission denied to create role");
    assert_eq!(error.sqlstate(), "42501");
    let error = failure(&mut app, "CREATE DATABASE shop");
    assert_eq!(error.to_string(), "permission denied to create database");

    // With CREATEROLE it can, and the role it made is visible to everyone.
    run(&mut admin, "ALTER ROLE app CREATEROLE");
    let mut app = Session::login(engine.clone(), "velocitysql", "app", Some("s3cret")).unwrap();
    run(&mut app, "CREATE ROLE delegated");
    assert!(engine.role("delegated").is_some());

    // ... but creating a superuser is reserved for superusers.
    let mut admin2 = Session::new(engine.clone());
    run(&mut admin2, "CREATE ROLE boss SUPERUSER");
    assert!(engine.role("boss").unwrap().superuser);

    // CREATEDB is enough for a database.
    run(&mut admin, "ALTER ROLE app CREATEDB");
    let mut app = Session::login(engine, "velocitysql", "app", Some("s3cret")).unwrap();
    run(&mut app, "CREATE DATABASE shop");
}

#[test]
fn alter_role_changes_password_and_attributes() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 'first'");

    run(&mut admin, "ALTER ROLE app PASSWORD 'second'");
    assert!(!engine.role("app").unwrap().check_password("first"));
    assert!(engine.role("app").unwrap().check_password("second"));
    assert!(Session::login(engine.clone(), "velocitysql", "app", Some("second")).is_ok());

    // `PASSWORD NULL` removes the password, which disables password login.
    run(&mut admin, "ALTER ROLE app PASSWORD NULL");
    assert!(engine.role("app").unwrap().password.is_none());
    assert!(Session::login(engine.clone(), "velocitysql", "app", Some("second")).is_err());

    run(&mut admin, "ALTER ROLE app NOLOGIN SUPERUSER");
    let role = engine.role("app").unwrap();
    assert!(!role.can_login && role.superuser);
}

#[test]
fn role_names_are_folded_and_validated() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());

    run(&mut admin, "CREATE ROLE \"Mixed\"");
    assert!(engine.role("mixed").is_some(), "names fold to lower case");

    let error = failure(&mut admin, "CREATE ROLE pg_reserved");
    assert!(error.to_string().contains("unacceptable"), "{error}");
    assert_eq!(error.sqlstate(), "42939");

    let long = "x".repeat(64);
    let error = failure(&mut admin, &format!("CREATE ROLE {long}"));
    assert_eq!(error.sqlstate(), "42622", "{error}");

    let error = failure(&mut admin, "CREATE ROLE mixed");
    assert_eq!(error.to_string(), "role \"mixed\" already exists");
    assert_eq!(error.sqlstate(), "42710");
    run(&mut admin, "CREATE ROLE IF NOT EXISTS mixed");
}

#[test]
fn drop_role_rules() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");

    // A role still holding a connection is dropped anyway (PostgreSQL would
    // refuse; VelocitySQL keeps no per-role connection count yet).
    run(&mut admin, "DROP ROLE app");
    assert!(engine.role("app").is_none());
    run(&mut admin, "DROP ROLE IF EXISTS app");

    let error = failure(&mut admin, "DROP ROLE app");
    assert_eq!(error.to_string(), "role \"app\" does not exist");
    assert_eq!(error.sqlstate(), "42704");

    // The current role and the bootstrap superuser are off limits: the first
    // would be a self-inflicted outage, the second would lock the cluster.
    let mut app = {
        run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");
        Session::login(engine.clone(), "velocitysql", "app", Some("s3cret")).unwrap()
    };
    let error = failure(&mut app, "DROP ROLE app");
    assert_eq!(error.to_string(), "current user cannot be dropped");
    assert_eq!(error.sqlstate(), "55006");

    // The bootstrap superuser is off limits even to another superuser: without it
    // the cluster would have no way back in, and VelocitySQL has no single-user
    // recovery mode to repair that.
    run(
        &mut admin,
        "CREATE ROLE boss SUPERUSER LOGIN PASSWORD 's3cret'",
    );
    let mut boss = Session::login(engine.clone(), "velocitysql", "boss", Some("s3cret")).unwrap();
    let error = failure(&mut boss, "DROP ROLE velocitysql");
    assert!(error.to_string().contains("bootstrap superuser"), "{error}");
    assert_eq!(error.sqlstate(), "55006");

    // Renaming and `VALID UNTIL` are not implemented and must say so.
    let error = failure(&mut admin, "ALTER ROLE app VALID UNTIL '2030-01-01'");
    assert!(
        error.to_string().contains("feature not supported"),
        "{error}"
    );
}

#[test]
fn roles_are_cluster_wide() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE DATABASE shop");
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");

    // The role created while connected to the default database logs in to `shop`.
    let session = Session::login(engine.clone(), "shop", "app", Some("s3cret"))
        .expect("roles are not per database");
    assert_eq!(session.database_name(), "shop");
    assert_eq!(session.user(), "app");

    // And a role created from inside `shop` is visible from the default one.
    let mut shop = Session::login(engine, "shop", "app", Some("s3cret")).unwrap();
    assert_eq!(
        failure(&mut shop, "CREATE ROLE other").sqlstate(),
        "42501",
        "the new role has no CREATEROLE"
    );
}

#[test]
fn current_user_reports_the_connected_role() {
    let engine = engine();
    let mut admin = Session::new(engine.clone());
    run(&mut admin, "CREATE ROLE app LOGIN PASSWORD 's3cret'");
    assert_eq!(
        text_rows(&mut admin, "SELECT current_user, session_user"),
        vec![format!("{DEFAULT_SUPERUSER}|{DEFAULT_SUPERUSER}")]
    );

    let mut app = Session::login(engine, "velocitysql", "app", Some("s3cret")).unwrap();
    assert_eq!(text_rows(&mut app, "SELECT current_user"), vec!["app"]);
    // The role name reaches the planner as a plain value.
    assert_eq!(
        run(&mut app, "SELECT current_user").data(),
        &[vec![Value::Text("app".to_string())]]
    );
}
