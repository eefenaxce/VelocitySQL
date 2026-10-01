//! The VelocitySQL console binary.
//!
//! ```text
//! velocitysql-cli [--demo] [--timing] [-c "SQL"]
//! ```
//!
//! This is an **embedded** console: it runs the engine in-process instead of
//! speaking the wire protocol. That makes it the tool that always works — no
//! port, no client library — and lets it offer catalog introspection (`\dt`,
//! `\d`, `\l`) that does not depend on `pg_catalog` being populated yet.
//!
//! For wire access use `psql` (or any PostgreSQL driver) against
//! `velocitysql-server`; see `--help`.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};

use vsql_catalog::Oid;
use vsql_executor::{Database, Engine, Field, QueryResult, Session};
use vsql_types::{DataType, Value};

/// Sample schema loaded by `--demo`, identical to the server's.
const DEMO_SQL: &str = "
CREATE TABLE users (
    id INT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE,
    active BOOL DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT now()
);
INSERT INTO users (id, name, email) VALUES
    (1, 'ada', 'ada@example.com'),
    (2, 'grace', 'grace@example.com'),
    (3, 'alan', NULL);

CREATE TABLE orders (
    id INT PRIMARY KEY,
    user_id INT NOT NULL,
    total NUMERIC(10,2) NOT NULL
);
INSERT INTO orders VALUES (10, 1, 100.50), (11, 1, 20.00), (12, 2, 5.25);
CREATE INDEX orders_user_idx ON orders (user_id);
";

#[derive(Debug, Default)]
struct Args {
    command: Option<String>,
    database: Option<String>,
    username: Option<String>,
    /// `--password`: authenticate with SCRAM instead of the trusted in-process path.
    password: Option<String>,
    /// `-W`: ask for the password on standard input.
    prompt_password: bool,
    demo: bool,
    timing: bool,
    help: bool,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Args> {
        let mut args = Args::default();
        let mut argv = argv.peekable();
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "-c" | "--command" => {
                    args.command = Some(argv.next().context("-c needs SQL text")?);
                }
                "-d" | "--database" | "--dbname" => {
                    args.database = Some(argv.next().context("-d needs a database name")?);
                }
                "-U" | "--username" => {
                    args.username = Some(argv.next().context("-U needs a role name")?);
                }
                "--password" => {
                    args.password = Some(argv.next().context("--password needs a value")?);
                }
                "-W" | "--prompt-password" => args.prompt_password = true,
                "--demo" => args.demo = true,
                "--timing" => args.timing = true,
                "-h" | "--help" => args.help = true,
                "-V" | "--version" => {
                    println!("velocitysql-cli {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                other => bail!("unrecognized argument `{other}` (try --help)"),
            }
        }
        Ok(args)
    }
}

fn print_help() {
    println!(
        "\
velocitysql-cli {version}
Embedded console for the VelocitySQL engine (runs in-process).

USAGE:
    velocitysql-cli [OPTIONS]

OPTIONS:
    -c, --command <SQL>      Run one SQL script and exit
    -d, --database <NAME>    Connect to this database   [default: velocitysql]
    -U, --username <ROLE>    Act as this role           [default: velocitysql]
    -W, --prompt-password    Ask for the password before connecting
        --password <VALUE>   Password to authenticate with
        --demo               Load the sample `users`/`orders` schema first
        --timing             Print how long each statement took
    -h, --help               Print this help
    -V, --version            Print the version

AUTHENTICATION:
    With -W or --password the console logs in through the engine's SCRAM
    verifier, exactly as a client does over the wire. Without either, it opens a
    trusted in-process session: the process already links the engine, so there is
    no boundary for a password to protect - the same reasoning as PostgreSQL's
    single-user mode, and the reason it is not reachable over a socket.

META COMMANDS (interactive):
    \\?             this help
    \\du            list roles
    \\l             list databases
    \\c <database>  connect to another database
    \\dn            list schemas
    \\dt            list tables
    \\di            list indexes
    \\d <table>     describe a table
    \\timing        toggle statement timing
    \\q             quit

NOTES:
    NULL is displayed as `NULL` (psql prints nothing) - a deliberate choice for
    a console that is also used to inspect data.",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn main() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print_help();
        return Ok(());
    }

    let engine = Arc::new(Engine::new());
    let database = args
        .database
        .clone()
        .unwrap_or_else(|| Engine::default_database_name().to_string());
    let user = args
        .username
        .clone()
        .unwrap_or_else(|| "velocitysql".to_string());
    // `-W`/`--password` exercise the real login path; without them the console
    // connects in-process, where the process boundary is the boundary.
    let password = match (&args.password, args.prompt_password) {
        (Some(password), _) => Some(password.clone()),
        (None, true) => Some(read_password()?),
        (None, false) => None,
    };
    // Connecting reports `database "x" does not exist` (3D000) or a rejected
    // credential (28P01) and exits non-zero, exactly like `psql -d`.
    let mut session = match &password {
        Some(password) => Session::login(engine, &database, &user, Some(password.as_str()))?,
        None => Session::connect(engine, &database, &user)?,
    };

    if args.demo {
        run_script(&mut session, DEMO_SQL).context("--demo failed")?;
    }

    match args.command.clone() {
        Some(sql) => {
            let ok = execute_and_print(&mut session, &sql, args.timing);
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        None => repl(&mut session, args.timing),
    }
}

fn repl(session: &mut Session, mut timing: bool) -> Result<()> {
    // Piped input (`echo "SELECT 1;" | velocitysql-cli`) is a script, not a
    // conversation: prompts and the banner would only corrupt the output.
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        println!(
            "VelocitySQL {} - connected to database \"{}\" as \"{}\"",
            env!("CARGO_PKG_VERSION"),
            session.database_name(),
            session.user()
        );
        println!("Type SQL terminated by `;`, `\\?` for help, `\\q` to quit.\n");
    }

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut buffer = String::new();
    loop {
        if interactive {
            // The prompt tracks the connection, so `\c` is visible immediately.
            print!(
                "{}=> ",
                if buffer.is_empty() {
                    session.database_name().to_string()
                } else {
                    format!("{}->", session.database_name())
                }
            );
            std::io::stdout().flush()?;
        }

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            println!();
            return Ok(());
        }

        let trimmed = line.trim();
        if buffer.is_empty() && trimmed.starts_with('\\') {
            match meta_command(session, trimmed, &mut timing) {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(error) => {
                    eprintln!("ERROR:  {error}");
                    continue;
                }
            }
        }

        buffer.push_str(&line);
        if !buffer.trim_end().ends_with(';') {
            continue;
        }
        let sql = std::mem::take(&mut buffer);
        if sql.trim() == ";" {
            continue;
        }
        execute_and_print(session, &sql, timing);
    }
}

/// Reads a password from standard input.
fn read_password() -> Result<String> {
    use std::io::Write as _;
    eprint!("Password: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Runs a script, printing each statement's result.
fn run_script(session: &mut Session, sql: &str) -> Result<()> {
    if !execute_and_print(session, sql, false) {
        bail!("script failed");
    }
    Ok(())
}

/// Executes SQL and prints it, returning `false` when a statement failed.
fn execute_and_print(session: &mut Session, sql: &str, timing: bool) -> bool {
    let started = Instant::now();
    let outcome = session.execute(sql);
    let elapsed = started.elapsed();

    let ok = match outcome {
        Ok(results) => {
            for result in &results {
                match result {
                    QueryResult::Rows { schema, rows } => {
                        print!("{}", render_rows(schema, rows));
                    }
                    QueryResult::Command { tag } => println!("{tag}"),
                }
            }
            true
        }
        Err(error) => {
            // The message already reads like PostgreSQL's; SQLSTATE is what
            // scripts branch on, so it is shown too.
            eprintln!("ERROR:  {} (SQLSTATE {})", error, error.sqlstate());
            if let Some(position) = error.position() {
                eprintln!("POSITION:  {position}");
            }
            false
        }
    };
    if timing {
        println!("Time: {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    }
    ok
}

/// Handles a `\`-prefixed command. Returns `false` when the console should exit.
fn meta_command(session: &mut Session, line: &str, timing: &mut bool) -> Result<bool> {
    let mut parts = line.split_whitespace();
    let command = parts.next().unwrap_or_default();
    let argument = parts.next();
    match command {
        "\\q" | "\\quit" => return Ok(false),
        "\\?" | "\\h" | "\\help" => print_help(),
        "\\timing" => {
            *timing = !*timing;
            println!("Timing is {}.", if *timing { "on" } else { "off" });
        }
        "\\l" | "\\list" => list_databases(session.engine(), session.database_name())?,
        "\\c" | "\\connect" => match argument {
            Some(name) => {
                session.switch_database(name)?;
                println!(
                    "You are now connected to database \"{}\".",
                    session.database_name()
                );
            }
            None => bail!("\\c needs a database name (try \\l to list them)"),
        },
        "\\du" | "\\dg" => list_roles(session.engine(), session.user())?,
        "\\dn" => list_schemas(session.database())?,
        "\\dt" => list_tables(session.database())?,
        "\\di" => list_indexes(session.database())?,
        "\\d" => match argument {
            Some(name) => describe_table(session.database(), name)?,
            None => list_tables(session.database())?,
        },
        other => bail!("unrecognized command `{other}` (try \\?)"),
    }
    Ok(true)
}

/// `\l`: every database in the cluster, marking the connected one.
fn list_databases(engine: &Arc<Engine>, current: &str) -> Result<()> {
    let mut rows = Vec::new();
    for database in engine.databases() {
        rows.push(vec![
            database.name().to_string(),
            database.def().owner.clone(),
            "UTF8".to_string(),
            if database.name() == current {
                "*".to_string()
            } else {
                String::new()
            },
        ]);
    }
    print_table(
        &["Name", "Owner", "Encoding", "Current"],
        &rows,
        &[false, false, false, false],
    );
    Ok(())
}

/// `\du`: every role in the cluster, marking the connected one.
fn list_roles(engine: &Arc<Engine>, current: &str) -> Result<()> {
    let mut rows = Vec::new();
    for role in engine.roles() {
        rows.push(vec![
            role.name.clone(),
            role.attributes().join(", "),
            if role.password.is_some() {
                "********".to_string()
            } else {
                String::new()
            },
            if role.name == current {
                "*".to_string()
            } else {
                String::new()
            },
        ]);
    }
    print_table(
        &["Role name", "Attributes", "Password", "Current"],
        &rows,
        &[false, false, false, false],
    );
    Ok(())
}
/// `\dn`: the schemas of the connected database.
fn list_schemas(database: &Arc<Database>) -> Result<()> {
    let tables = database.catalog.tables();
    let mut rows = Vec::new();
    for schema in database.catalog.schemas() {
        let count = tables
            .iter()
            .filter(|table| table.schema_oid == schema.oid)
            .count();
        rows.push(vec![
            schema.name,
            if schema.is_system {
                "system".to_string()
            } else {
                String::new()
            },
            count.to_string(),
        ]);
    }
    print_table(&["Name", "Kind", "Tables"], &rows, &[false, false, true]);
    Ok(())
}

/// `\dt`: the tables of the connected database.
fn list_tables(database: &Arc<Database>) -> Result<()> {
    let schemas: HashMap<Oid, String> = database
        .catalog
        .schemas()
        .into_iter()
        .map(|schema| (schema.oid, schema.name))
        .collect();
    let mut rows = Vec::new();
    for table in database.catalog.tables() {
        if table.is_system {
            continue;
        }
        rows.push(vec![
            schemas.get(&table.schema_oid).cloned().unwrap_or_default(),
            table.name.clone(),
            table.rows().to_string(),
        ]);
    }
    print_table(&["Schema", "Name", "Rows"], &rows, &[false, false, true]);
    Ok(())
}

/// `\di`: the indexes of the connected database.
fn list_indexes(database: &Arc<Database>) -> Result<()> {
    let mut rows = Vec::new();
    for table in database.catalog.tables() {
        for index in database.catalog.table_indexes(table.oid) {
            let columns: Vec<String> = index
                .columns
                .iter()
                .filter_map(|ordinal| table.columns.get(*ordinal))
                .map(|column| column.name.clone())
                .collect();
            rows.push(vec![
                table.name.clone(),
                index.name.clone(),
                columns.join(", "),
                if index.primary {
                    "PRIMARY KEY".to_string()
                } else if index.unique {
                    "UNIQUE".to_string()
                } else {
                    String::new()
                },
                index.method.am_name().to_string(),
            ]);
        }
    }
    print_table(
        &["Table", "Index", "Columns", "Constraint", "Method"],
        &rows,
        &[false, false, false, false, false],
    );
    Ok(())
}

/// `\d <table>`: the columns and indexes of one relation.
fn describe_table(database: &Arc<Database>, name: &str) -> Result<()> {
    let def = database
        .catalog
        .resolve_table(&[name.to_string()])
        .with_context(|| format!("cannot describe `{name}`"))?;

    let mut rows = Vec::new();
    for column in def.columns.iter().filter(|column| !column.dropped) {
        rows.push(vec![
            column.name.clone(),
            column.data_type.format_type(),
            if column.nullable {
                String::new()
            } else {
                "not null".to_string()
            },
            column.default.clone().unwrap_or_default(),
        ]);
    }
    println!("Table \"{}\"", def.name);
    print_table(
        &["Column", "Type", "Nullable", "Default"],
        &rows,
        &[false, false, false, false],
    );

    let indexes = database.catalog.table_indexes(def.oid);
    if !indexes.is_empty() {
        let mut rows = Vec::new();
        for index in indexes {
            let columns: Vec<String> = index
                .columns
                .iter()
                .filter_map(|ordinal| def.columns.get(*ordinal))
                .map(|column| column.name.clone())
                .collect();
            rows.push(vec![
                index.name.clone(),
                columns.join(", "),
                if index.primary {
                    "PRIMARY KEY".to_string()
                } else if index.unique {
                    "UNIQUE".to_string()
                } else {
                    String::new()
                },
            ]);
        }
        println!("Indexes:");
        print_table(
            &["Index", "Columns", "Constraint"],
            &rows,
            &[false, false, false],
        );
    }
    Ok(())
}
/// Renders a query result the way `psql` does.
fn render_rows(schema: &[Field], rows: &[Vec<Value>]) -> String {
    let mut rendered: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        rendered.push(
            row.iter()
                .map(|value| {
                    if value.is_null() {
                        "NULL".to_string()
                    } else {
                        value.to_string()
                    }
                })
                .collect(),
        );
    }
    let headers: Vec<String> = schema.iter().map(|field| field.name.clone()).collect();
    let right_aligned: Vec<bool> = schema
        .iter()
        .map(|field| is_right_aligned(&field.data_type))
        .collect();
    let mut text = width_adjust(headers, rendered, &right_aligned);
    text.push_str(&row_count_footer(rows.len()));
    text.push_str("\n\n");
    text
}

fn is_right_aligned(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Int2
            | DataType::Int4
            | DataType::Int8
            | DataType::Float4
            | DataType::Float8
            | DataType::Numeric { .. }
    )
}

/// Prints a table followed by its row count, the way `psql` does.
fn print_table(headers: &[&str], rows: &[Vec<String>], right: &[bool]) {
    let headers: Vec<String> = headers.iter().map(|header| header.to_string()).collect();
    let count = rows.len();
    let mut text = width_adjust(headers, rows.to_vec(), right);
    text.push_str(&row_count_footer(count));
    println!("{text}");
}

/// `(0 rows)` / `(1 row)` / `(N rows)`, pluralised like PostgreSQL.
fn row_count_footer(count: usize) -> String {
    match count {
        0 => "(0 rows)".to_string(),
        1 => "(1 row)".to_string(),
        other => format!("({other} rows)"),
    }
}

/// Lays out a table with psql's `+----+` separators.
fn width_adjust(headers: Vec<String>, rows: Vec<Vec<String>>, right: &[bool]) -> String {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        // Zipping rather than indexing keeps a short row from panicking; the
        // remaining columns simply keep their current width.
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let separator: String = widths
        .iter()
        .map(|width| "-".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("+");
    let separator = format!("{separator}\n");

    let mut out = String::new();
    out.push_str(&separator);
    out.push_str(&format_row(&headers, &widths, right));
    out.push_str(&separator);
    for row in &rows {
        let padded: Vec<String> = (0..columns)
            .map(|index| row.get(index).cloned().unwrap_or_default())
            .collect();
        out.push_str(&format_row(&padded, &widths, right));
    }
    out.push_str(&separator);
    out
}

fn format_row(cells: &[String], widths: &[usize], right: &[bool]) -> String {
    // Callers pad every row to the same width, so `cells` and `widths` always
    // have the same length; zipping keeps that invariant obvious.
    let mut out = String::new();
    for (position, (cell, width)) in cells.iter().zip(widths.iter()).enumerate() {
        let padding = width.saturating_sub(cell.chars().count());
        if right.get(position).copied().unwrap_or(false) {
            out.push_str(&format!(" {}{} ", " ".repeat(padding), cell));
        } else {
            out.push_str(&format!(" {}{} ", cell, " ".repeat(padding)));
        }
        if position + 1 < cells.len() {
            out.push('|');
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_count_footer_is_pluralised_like_postgres() {
        assert_eq!(row_count_footer(0), "(0 rows)");
        assert_eq!(row_count_footer(1), "(1 row)");
        assert_eq!(row_count_footer(7), "(7 rows)");
    }

    #[test]
    fn width_adjust_lays_out_like_psql() {
        let table = width_adjust(
            vec!["id".to_string(), "name".to_string()],
            vec![
                vec!["1".to_string(), "ada".to_string()],
                vec!["22".to_string(), "grace".to_string()],
            ],
            &[true, false],
        );
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines,
            vec![
                "----+-------",
                " id | name  ",
                "----+-------",
                // numbers are right-aligned, text is left-aligned
                "  1 | ada   ",
                " 22 | grace ",
                "----+-------",
            ]
        );
    }

    #[test]
    fn only_numeric_types_are_right_aligned() {
        assert!(is_right_aligned(&DataType::Int8));
        assert!(is_right_aligned(&DataType::Numeric {
            precision: None,
            scale: None,
        }));
        assert!(!is_right_aligned(&DataType::Text));
        assert!(!is_right_aligned(&DataType::Bool));
    }
}
