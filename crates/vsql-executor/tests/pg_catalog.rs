//! The virtual `pg_catalog` relations.
//!
//! GUI tools and ORMs query these while connecting (`pg_database` for the
//! database list) and while browsing (`pg_class`/`pg_attribute` for `\d`-style
//! metadata), so a missing relation breaks a client that never runs any user
//! SQL. The rows must agree with the live catalog, not with a stale copy.

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

fn rows(session: &mut Session, sql: &str) -> Vec<Vec<String>> {
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

fn one_column(session: &mut Session, sql: &str) -> Vec<String> {
    rows(session, sql)
        .into_iter()
        .map(|mut row| row.remove(0))
        .collect()
}

#[test]
fn pg_database_lists_every_database_with_utf8() {
    let mut session = session();
    run(&mut session, "CREATE DATABASE shop");
    let rows = rows(
        &mut session,
        "SELECT datname, encoding, datistemplate, datallowconn FROM pg_database ORDER BY datname",
    );

    let names: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
    assert!(names.contains(&"velocitysql"), "{names:?}");
    assert!(names.contains(&"postgres"), "{names:?}");
    assert!(names.contains(&"shop"), "{names:?}");

    let shop = rows.iter().find(|row| row[0] == "shop").unwrap();
    assert_eq!(shop[1], "6", "6 is PostgreSQL's encoding number for UTF8");
    assert_eq!(shop[2], "f");
    assert_eq!(shop[3], "t");
}

#[test]
fn pg_database_exposes_its_oid() {
    let mut session = session();
    // Tools select `d.oid` from `pg_database d` as the row identity.
    let rows = rows(
        &mut session,
        "SELECT d.oid, d.datname FROM pg_database d ORDER BY d.oid",
    );
    assert_eq!(rows.len(), 2);
    assert!(rows[0][0] != rows[1][0], "{rows:?}");
    assert!(rows
        .iter()
        .all(|row| row[1] == "velocitysql" || row[1] == "postgres"));
}

#[test]
fn a_missing_qualified_column_is_a_column_error_not_a_from_error() {
    let mut session = session();
    // The qualifier is in scope, so the *column* is what is missing - the same
    // distinction PostgreSQL draws, and the one that keeps a typo from sending
    // the reader chasing the wrong relation.
    let error = session
        .execute("SELECT d.not_a_column FROM pg_database d")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "column \"d.not_a_column\" does not exist"
    );
    assert_eq!(error.sqlstate(), "42703");

    // An unknown qualifier is still a FROM-clause problem.
    let error = session
        .execute("SELECT nope.datname FROM pg_database d")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "missing FROM-clause entry for table \"nope\""
    );
}

#[test]
fn pg_roles_and_pg_user_report_the_accounts() {
    let mut session = session();
    run(&mut session, "CREATE ROLE job"); // NOLOGIN
    run(&mut session, "CREATE USER app PASSWORD 's3cret'");
    run(&mut session, "ALTER ROLE app CREATEDB");

    let rows = rows(
        &mut session,
        "SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolpassword FROM pg_roles ORDER BY rolname",
    );
    let app = rows.iter().find(|row| row[0] == "app").unwrap();
    assert_eq!(app[1], "f");
    assert_eq!(app[2], "t");
    assert_eq!(app[3], "t");
    // The verifier is never exposed, not even to superusers.
    assert_eq!(app[4], "NULL");

    // `pg_user` only lists roles that may log in.
    let users = one_column(&mut session, "SELECT usename FROM pg_user ORDER BY usename");
    assert!(users.contains(&"app".to_string()), "{users:?}");
    assert!(!users.contains(&"job".to_string()), "{users:?}");
}

#[test]
fn pg_class_and_pg_attribute_describe_relations() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int PRIMARY KEY, label text NOT NULL)",
    );
    run(
        &mut session,
        "CREATE VIEW labels AS SELECT label FROM items",
    );
    run(&mut session, "CREATE INDEX items_label ON items (label)");

    let kinds = rows(
        &mut session,
        "SELECT relname, relkind FROM pg_class ORDER BY relname",
    );
    let kind_of = |name: &str| {
        kinds
            .iter()
            .find(|row| row[0] == name)
            .map(|row| row[1].clone())
            .unwrap_or_else(|| panic!("{name} missing from pg_class: {kinds:?}"))
    };
    assert_eq!(kind_of("items"), "r");
    assert_eq!(kind_of("labels"), "v");
    assert_eq!(kind_of("items_pkey"), "i");
    assert_eq!(kind_of("items_label"), "i");

    let columns = rows(
        &mut session,
        "SELECT a.attname, a.attnum, a.attnotnull, t.typname FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_type t ON t.oid = a.atttypid \
         WHERE c.relname = 'items' ORDER BY a.attnum",
    );
    let expected: Vec<Vec<String>> = vec![
        vec!["id".into(), "1".into(), "t".into(), "int4".into()],
        vec!["label".into(), "2".into(), "t".into(), "text".into()],
    ];
    assert_eq!(columns, expected);
}

#[test]
fn pg_type_covers_the_builtin_types() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int4 PRIMARY KEY, label text)",
    );
    let names = one_column(&mut session, "SELECT typname FROM pg_type ORDER BY typname");
    for expected in [
        "bool", "int2", "int4", "int8", "text", "float8", "numeric", "uuid", "jsonb",
    ] {
        assert!(names.contains(&expected.to_string()), "{names:?}");
    }
    // The reported OIDs are the ones the wire protocol uses, so a driver that
    // joins on them sees one consistent numbering.
    let rows = rows(
        &mut session,
        "SELECT t.oid, a.atttypid FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid LIMIT 1",
    );
    assert_eq!(rows[0][0], rows[0][1], "{:?}", rows[0]);
}

#[test]
fn pg_settings_and_the_shorthand_views() {
    let mut session = session();
    run(&mut session, "CREATE TABLE items (name text)");
    let settings = rows(&mut session, "SELECT name, setting FROM pg_settings");
    let value_of = |name: &str| {
        settings
            .iter()
            .find(|row| row[0] == name)
            .map(|row| row[1].clone())
            .unwrap_or_else(|| panic!("{name} missing from pg_settings"))
    };
    assert_eq!(value_of("datestyle"), "ISO, MDY");
    assert_eq!(value_of("timezone"), "UTC");

    let tables = one_column(
        &mut session,
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
    );
    assert!(!tables.is_empty(), "{tables:?}");

    run(&mut session, "CREATE INDEX items_name_idx ON items (name)");
    run(&mut session, "CREATE VIEW always AS SELECT 1");
    let views = one_column(
        &mut session,
        "SELECT viewname FROM pg_views WHERE schemaname = 'public'",
    );
    assert!(views.contains(&"always".to_string()), "{views:?}");

    let indexes = rows(
        &mut session,
        "SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'items' ORDER BY indexname",
    );
    let def = &indexes[0][1];
    assert!(
        def.starts_with("CREATE ") && def.contains("ON items"),
        "{def}"
    );
}

#[test]
fn pg_tablespace_lists_the_default_tablespaces() {
    let mut session = session();
    let rows = rows(
        &mut session,
        "SELECT oid, spcname FROM pg_tablespace ORDER BY oid",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1663".to_string(), "pg_default".to_string()],
            vec!["1664".to_string(), "pg_global".to_string()],
        ]
    );
}

#[test]
fn pg_constraint_reports_keys() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int PRIMARY KEY, code text UNIQUE)",
    );
    let rows = rows(
        &mut session,
        "SELECT conname, contype, conkey FROM pg_constraint WHERE conrelid = \
         (SELECT oid FROM pg_class WHERE relname = 'items') ORDER BY conname",
    );
    assert_eq!(
        rows,
        vec![
            vec![
                "items_code_key".to_string(),
                "u".to_string(),
                "{2}".to_string()
            ],
            vec!["items_pkey".to_string(), "p".to_string(), "{1}".to_string()],
        ]
    );
}

#[test]
fn pg_constraint_exposes_foreign_key_columns() {
    let mut session = session();
    run(&mut session, "CREATE TABLE items (id int PRIMARY KEY)");
    // The constraints this engine reports are primary keys and unique
    // indexes, whose foreign-key-only columns are all NULL, as in PostgreSQL.
    let rows = rows(
        &mut session,
        "SELECT conparentid, confupdtype, confdeltype, confmatchtype, conpfeqop, \
         coninhcount, connoinherit, conbin IS NULL FROM pg_constraint c \
         WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'items')",
    );
    assert_eq!(
        rows,
        vec![vec![
            "0".to_string(),
            "NULL".to_string(),
            "NULL".to_string(),
            "NULL".to_string(),
            "NULL".to_string(),
            "0".to_string(),
            "f".to_string(),
            "t".to_string(),
        ]]
    );
}

#[test]
fn rows_carry_their_catalogue_oid_in_tableoid() {
    let mut session = session();
    run(&mut session, "CREATE TABLE items (id int PRIMARY KEY)");
    // `tableoid` is the hidden system column every PostgreSQL row carries;
    // clients filter on it to tell which catalogue a row came from.
    let rows = rows(
        &mut session,
        "SELECT DISTINCT c.tableoid FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
         WHERE c.relname = 'items'",
    );
    assert_eq!(rows, vec![vec!["1259".to_string()]]);
    // Rows of the view-shaped relations belong to no real system table.
    let views = one_column(&mut session, "SELECT DISTINCT tableoid FROM pg_tables");
    assert_eq!(views, vec!["0".to_string()]);
}

#[test]
fn pg_opclass_exists_even_though_empty() {
    let mut session = session();
    // Clients probe pg_opclass while browsing index definitions; reporting it
    // as an empty relation keeps the lookup from failing outright.
    let count = one_column(&mut session, "SELECT count(*) FROM pg_opclass");
    assert_eq!(count, vec!["0".to_string()]);
    let names = one_column(
        &mut session,
        "SELECT opcname FROM pg_opclass WHERE opcname = 'int4_ops'",
    );
    assert!(names.is_empty());
}

#[test]
fn pg_matviews_has_postgresqls_column_shape() {
    let mut session = session();
    // Navicat selects `v.tablespace` from `pg_matviews v` while opening a query
    // window; a missing column fails the whole statement even though the
    // relation is empty, so the shape has to match `pg_matviews` exactly.
    let selected = run(
        &mut session,
        "SELECT v.tablespace, v.hasindexes FROM pg_matviews v",
    );
    let QueryResult::Rows { rows, .. } = &selected else {
        panic!("expected rows");
    };
    assert!(rows.is_empty());

    let all = run(&mut session, "SELECT * FROM pg_matviews");
    let QueryResult::Rows { schema, .. } = &all else {
        panic!("expected rows");
    };
    let names: Vec<&str> = schema.iter().map(|field| field.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "schemaname",
            "matviewname",
            "matviewowner",
            "tablespace",
            "hasindexes",
            "ispopulated",
            "definition",
        ]
    );
}

#[test]
fn empty_relations_exist_but_have_no_rows() {
    let mut session = session();
    run(&mut session, "CREATE TABLE items (id int PRIMARY KEY)");
    let am = rows(&mut session, "SELECT amname FROM pg_am ORDER BY oid");
    assert_eq!(
        am,
        vec![vec!["btree".to_string()], vec!["hash".to_string()]]
    );

    // Joins against them yield an empty set, not an error.
    let sequences = one_column(&mut session, "SELECT sequencename FROM pg_sequences");
    assert!(sequences.is_empty());
    let extensions = one_column(&mut session, "SELECT extname FROM pg_extension");
    assert!(extensions.is_empty());
    let inherited = one_column(
        &mut session,
        "SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid",
    );
    assert!(inherited.is_empty());
}

#[test]
fn catalog_names_resolve_qualified_and_unqualified() {
    let mut session = session();
    run(&mut session, "SELECT datname FROM pg_database");
    run(&mut session, "SELECT datname FROM pg_catalog.pg_database");
    // A wrong schema is a wrong schema, as in PostgreSQL.
    let error = session
        .execute("SELECT datname FROM public.pg_database")
        .unwrap_err();
    assert_eq!(error.to_string(), "relation \"pg_database\" does not exist");
}

#[test]
fn pg_index_maps_indexes_back_to_tables() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE items (id int PRIMARY KEY, label text)",
    );
    run(&mut session, "CREATE INDEX items_label ON items (label)");

    let rows = rows(
        &mut session,
        "SELECT c.relname AS table_name, i.indisprimary, i.indisunique, i.indkey \
         FROM pg_index i \
         JOIN pg_class idx ON idx.oid = i.indexrelid \
         JOIN pg_class c ON c.oid = i.indrelid \
         ORDER BY idx.relname",
    );
    // Ordered by index name: `items_label` is a plain (non-unique) index and
    // `items_pkey` backs the primary key. `indkey` uses PostgreSQL's vector
    // text form, positions separated by spaces.
    let expected: Vec<Vec<String>> = vec![
        vec!["items".into(), "f".into(), "f".into(), "2".into()],
        vec!["items".into(), "t".into(), "t".into(), "1".into()],
    ];
    assert_eq!(rows, expected);
}

#[test]
fn description_lookups_return_null() {
    let mut session = session();
    // `\l+` and the GUI tools decorate their listings with optional comment
    // columns; without `COMMENT ON` there is nothing to describe.
    let rows = rows(
        &mut session,
        "SELECT shobj_description(d.oid, 'pg_database'), \
         obj_description(d.oid, 'pg_database'), \
         col_description(d.oid, 1) \
         FROM pg_database d ORDER BY d.oid",
    );
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|row| row.iter().all(|value| value == "NULL")));
}

#[test]
fn the_oid_family_of_pseudo_types_is_castable() {
    let mut session = session();
    // Tools cast catalogue values to `oid` (or write `::regclass` etc. under the
    // `pg_catalog.` qualifier); PostgreSQL's object-identifier types are unsigned
    // 4-byte integers here.
    let rows = rows(
        &mut session,
        "SELECT CAST(5 AS oid), 5::oid, \
         CAST(16385 AS pg_catalog.oid), 5::xid, 5::regproc",
    );
    assert_eq!(
        rows,
        vec![vec![
            "5".to_string(),
            "5".to_string(),
            "16385".to_string(),
            "5".to_string(),
            "5".to_string(),
        ]]
    );

    // `regclass` renders as the relation name in PostgreSQL; the text mapping
    // reproduces exactly that display form.
    let name = one_column(&mut session, "SELECT 'users'::regclass");
    assert_eq!(name, vec!["users".to_string()]);

    // And the casts work against real catalogue columns, as `\l` uses them.
    let oids = one_column(
        &mut session,
        "SELECT CAST(oid AS pg_catalog.oid) FROM pg_database ORDER BY oid",
    );
    assert_eq!(oids.len(), 2);
}

#[test]
fn column_defaults_show_up_in_pg_attrdef() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE attrdef_demo (id int4 PRIMARY KEY, score int4 DEFAULT 0, note text DEFAULT 'n/a')",
    );

    // The classic tool query: join defaults back to their columns exactly the
    // way `\d`-style browsers do.
    let rows = rows(
        &mut session,
        "SELECT a.attname, a.attnum, pg_get_expr(ad.adbin, ad.adrelid) AS default_expr \
         FROM pg_class c \
         JOIN pg_attribute a ON a.attrelid = c.oid \
         JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         WHERE c.relname = 'attrdef_demo' \
         ORDER BY a.attnum",
    );
    assert_eq!(
        rows,
        vec![
            vec!["score".to_string(), "2".to_string(), "0".to_string()],
            vec!["note".to_string(), "3".to_string(), "'n/a'".to_string(),],
        ]
    );

    // `atthasdef` must agree with the rows `pg_attrdef` reports.
    let hasdef = one_column(
        &mut session,
        "SELECT atthasdef FROM pg_attribute a JOIN pg_class c ON a.attrelid = c.oid \
         WHERE c.relname = 'attrdef_demo' ORDER BY attnum",
    );
    assert_eq!(
        hasdef,
        vec!["f".to_string(), "t".to_string(), "t".to_string()]
    );

    // Columns without defaults have no row at all.
    let count = one_column(
        &mut session,
        "SELECT count(*) FROM pg_attrdef ad JOIN pg_class c ON ad.adrelid = c.oid \
         WHERE c.relname = 'attrdef_demo'",
    );
    assert_eq!(count, vec!["2".to_string()]);
}

#[test]
fn navicat_browsing_relations_and_functions_answer() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE probe (id int4 PRIMARY KEY, note varchar(50), ratio numeric(10,2))",
    );
    run(
        &mut session,
        "CREATE VIEW probe_view AS SELECT id FROM probe WHERE id > 0",
    );
    run(&mut session, "CREATE INDEX probe_note_idx ON probe (note)");

    // `pg_sequence` (catalog) and `pg_sequences` (view) exist with their real
    // column shapes; the engine has no sequences yet, so zero rows.
    for relation in ["pg_sequence", "pg_sequences"] {
        let sql = format!("SELECT count(*) FROM {relation}");
        assert_eq!(one_column(&mut session, &sql), vec!["0".to_string()]);
    }

    // The tool query: types rendered through `format_type`, round-tripping the
    // `atttypid` + `atttypmod` pair `pg_attribute` reports.
    let types = rows(
        &mut session,
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod) \
         FROM pg_attribute a JOIN pg_class c ON a.attrelid = c.oid \
         WHERE c.relname = 'probe' AND a.attnum > 0 ORDER BY a.attnum",
    );
    assert_eq!(
        types,
        vec![
            vec!["id".to_string(), "integer".to_string()],
            vec!["note".to_string(), "character varying(50)".to_string(),],
            vec!["ratio".to_string(), "numeric(10,2)".to_string(),],
        ]
    );
    // Bare calls too: PostgreSQL's standard OIDs and typmod encodings.
    assert_eq!(
        one_column(&mut session, "SELECT format_type(23)"),
        vec!["integer".to_string()]
    );
    assert_eq!(
        one_column(&mut session, "SELECT format_type(1700, 655366)"),
        vec!["numeric(10,2)".to_string()]
    );

    // View definitions render back to the SELECT the user wrote, by name,
    // qualified name and the synthetic OID `pg_class` reports.
    let view_oid = one_column(
        &mut session,
        "SELECT c.oid FROM pg_class c WHERE c.relname = 'probe_view' AND c.relkind = 'v'",
    );
    assert_eq!(view_oid.len(), 1);
    for argument in [
        "'probe_view'".to_string(),
        "'public.probe_view'".to_string(),
        view_oid[0].clone(),
    ] {
        let sql = format!("SELECT pg_get_viewdef({argument})");
        let definition = one_column(&mut session, &sql);
        assert_eq!(
            definition,
            vec!["SELECT id FROM probe WHERE (id > 0)".to_string()],
            "`{sql}`"
        );
    }

    // Index DDL, read from `pg_index` the way a browser's DDL tab does.
    let indexdef = one_column(
        &mut session,
        "SELECT pg_get_indexdef(indexrelid) FROM pg_index ORDER BY indexrelid",
    );
    assert!(
        indexdef.contains(&"CREATE UNIQUE INDEX probe_pkey ON probe USING btree (id)".to_string()),
        "{indexdef:?}"
    );
    assert!(
        indexdef.contains(&"CREATE INDEX probe_note_idx ON probe USING btree (note)".to_string()),
        "{indexdef:?}"
    );

    // Session probes browsers make while connecting.
    assert_eq!(
        one_column(&mut session, "SELECT pg_is_in_recovery()"),
        vec!["f".to_string()]
    );
    let search_path = one_column(&mut session, "SELECT current_setting('search_path')");
    assert!(search_path[0].contains("public"), "{search_path:?}");
}

#[test]
fn catalog_helper_functions_answer() {
    let mut session = session();
    let who = one_column(
        &mut session,
        "SELECT pg_get_userbyid(oid) FROM pg_roles WHERE rolname = 'velocitysql'",
    );
    assert_eq!(who, vec!["velocitysql".to_string()]);

    let encoding = one_column(
        &mut session,
        "SELECT pg_encoding_to_char(encoding) FROM pg_database WHERE datname = 'velocitysql'",
    );
    assert_eq!(encoding, vec!["UTF8".to_string()]);

    let version = one_column(&mut session, "SELECT version()");
    assert!(
        version[0].starts_with("PostgreSQL 15.0 (VelocitySQL"),
        "{version:?}"
    );

    assert_eq!(
        one_column(&mut session, "SELECT current_schema()"),
        vec!["public".to_string()]
    );
}

#[test]
fn explain_shows_the_catalogue_scan() {
    let mut session = session();
    let plan = rows(&mut session, "EXPLAIN SELECT datname FROM pg_database");
    let rendered = plan
        .into_iter()
        .map(|row| row.join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Seq Scan on pg_database"), "{rendered}");
}
