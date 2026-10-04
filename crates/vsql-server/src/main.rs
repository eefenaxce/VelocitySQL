//! The VelocitySQL server binary.
//!
//! ```text
//! velocitysql-server [--host 127.0.0.1] [--port 5210] [--demo] [--init "SQL"]
//! ```
//!
//! It listens for PostgreSQL Wire Protocol v3 clients, so `psql`, `pgAdmin`,
//! `tokio-postgres`, `sqlx` and JDBC drivers can connect and run statements.
//!
//! Reads and writes are served from memory; durability is always on — a
//! background task snapshots the cluster (roles, databases, schemas, tables,
//! rows and views) to `data/velocitysql.snapshot` promptly after every write and
//! once more on shutdown, so a restart loses at most the writes still in flight.
//! `--init`/`--demo` still run first, but a snapshot now rebuilds the objects it
//! created too, not just the rows. The full write-ahead log is milestone M2.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use vsql_executor::persist::{self, PersistTrigger};
use vsql_executor::{Engine, PersistHook, Session, DEFAULT_PASSWORD, DEFAULT_SUPERUSER};
use vsql_protocol::{bind, serve, AuthMethod, ServerConfig};

/// Sample schema loaded by `--demo`, so a fresh server has something to query.
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
    host: String,
    port: u16,
    demo: bool,
    init: Option<String>,
    /// `--auth`: how clients prove who they are.
    auth: String,
    /// `--superuser-password`: password of the bootstrap role.
    superuser_password: String,
    /// `--snapshot-path`: where the snapshot is written and read back from.
    ///
    /// Defaults to `data/velocitysql.snapshot` relative to the working
    /// directory, which is nobody's choice when the server is started at boot
    /// by a service or a scheduled task.
    snapshot_path: Option<PathBuf>,
    help: bool,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Args> {
        let mut args = Args {
            host: "127.0.0.1".to_string(),
            port: 5210,
            auth: AuthMethod::default().as_str().to_string(),
            superuser_password: DEFAULT_PASSWORD.to_string(),
            ..Args::default()
        };
        let mut argv = argv.peekable();
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--host" => {
                    args.host = argv.next().context("--host needs a value")?;
                }
                "-h" | "--help" => args.help = true,
                "--port" | "-p" => {
                    let value = argv.next().context("--port needs a value")?;
                    args.port = value
                        .parse()
                        .with_context(|| format!("`{value}` is not a valid port"))?;
                }
                "--init" => {
                    args.init = Some(argv.next().context("--init needs SQL text")?);
                }
                "--snapshot-path" => {
                    let value = argv.next().context("--snapshot-path needs a path")?;
                    args.snapshot_path = Some(PathBuf::from(value));
                }
                "--demo" => args.demo = true,
                "--auth" => {
                    args.auth = argv.next().context("--auth needs a method")?;
                }
                "--superuser-password" => {
                    args.superuser_password =
                        argv.next().context("--superuser-password needs a value")?;
                }
                "--version" | "-V" => {
                    println!("velocitysql-server {}", env!("CARGO_PKG_VERSION"));
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
velocitysql-server {version}
An in-memory, PostgreSQL-compatible SQL engine (milestone M1/M3).

USAGE:
    velocitysql-server [OPTIONS]

OPTIONS:
    --host <ADDR>                 Address to listen on      [default: 127.0.0.1]
    --port <PORT>                 Port to listen on         [default: 5210]
    --auth <METHOD>               trust | scram-sha-256     [default: scram-sha-256]
    --superuser-password <VALUE>  Password of the bootstrap role
                                  [default: {default_password}]
    --demo                        Create the sample `users`/`orders` schema first
    --init <SQL>                  Run SQL before serving
    --snapshot-path <PATH>        Where the snapshot is written
                                  [default: data/velocitysql.snapshot]
    -h, --help                    Print this help
    -V, --version                 Print the version

CLIENTS:
    psql -h 127.0.0.1 -p 5210 -U velocitysql
    (TLS is not implemented: `SSLRequest` is answered with 'N')

ACCOUNTS:
    The bootstrap role `{default_superuser}` is a superuser and can `CREATE ROLE`.
    Use --superuser-password (or change it with ALTER ROLE) before exposing the
    server beyond localhost.

LOGGING:
    Every statement is logged, with its bound parameters and its duration
    (`elapsed=1.23ms`), at the default level (`info`): finding out what a client
    is sending, and how long it took, should not need a restart. The line is
    written once the statement has finished, when the duration is known.
    A failing statement is logged at `warn`, with the SQL either way, since a
    message like `column \"x\" does not exist` is hard to act on without knowing
    which statement produced it. RUST_LOG narrows this: `vsql_protocol=warn`
    keeps the failures and drops the statements, `debug` adds connection detail.
    A password in CREATE/ALTER ROLE is masked to `'***'` before it is written.

NOTES:
    Rows are snapshotted to data/velocitysql.snapshot, so a restart keeps the
    data (see the README). `pg_catalog` is virtual, but psql's `\\d` still needs
    functions that are not implemented; use `velocitysql-cli` for \\dt / \\d.",
        version = env!("CARGO_PKG_VERSION"),
        default_password = DEFAULT_PASSWORD,
        default_superuser = DEFAULT_SUPERUSER
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print_help();
        return Ok(());
    }

    let auth = AuthMethod::parse(&args.auth)
        .with_context(|| format!("unknown authentication method `{}`", args.auth))?;

    let engine = Arc::new(Engine::with_superuser_password(&args.superuser_password));
    if args.superuser_password == DEFAULT_PASSWORD {
        println!(
            "warning: role \"{DEFAULT_SUPERUSER}\" still uses the documented default password; \
             pass --superuser-password to change it"
        );
    }
    if auth == AuthMethod::Trust {
        println!("warning: --auth trust accepts any existing role without a password");
    }
    if let Some(sql) = &args.init {
        run_script(&engine, sql).context("--init failed")?;
        info!("applied --init SQL");
    }
    if args.demo {
        run_script(&engine, DEMO_SQL).context("--demo failed")?;
        info!("loaded the demo schema (users, orders)");
    }

    // Restore the previous run's roles, databases, schema, rows and views
    // before accepting clients, so a restart picks up where it left off.
    // Objects `--init`/`--demo` already created are left alone; a database,
    // role or view created at runtime is reinstalled. Durability is always on,
    // so this file is expected to exist after the first run.
    let snapshot_path = args
        .snapshot_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(persist::DEFAULT_SNAPSHOT_PATH));
    // A deployment points this at a directory of its own choosing, which on a
    // first run does not exist yet.
    prepare_snapshot_path(&snapshot_path)?;
    match persist::Snapshot::load(&snapshot_path) {
        Ok(Some(snapshot)) => {
            snapshot.restore(&engine);
            info!(
                path = %snapshot_path.display(),
                "restored cluster state from snapshot"
            );
        }
        Ok(None) => {}
        Err(error) => {
            // The file cannot be read: an older on-disk layout, or damage. Move
            // it out of the way rather than let the first background save
            // overwrite it — data the operator may still want is not something
            // to destroy quietly.
            let preserved = snapshot_path.with_extension("snapshot.bak");
            let preserved = match std::fs::rename(&snapshot_path, &preserved) {
                Ok(()) => preserved.display().to_string(),
                Err(_) => String::new(),
            };
            warn!(
                %error,
                path = %snapshot_path.display(),
                preserved = %preserved,
                "could not load snapshot; starting with an empty database"
            );
        }
    }

    let config = ServerConfig {
        host: args.host.clone(),
        port: args.port,
        auth,
    };
    let listener = bind(&config)
        .await
        .with_context(|| format!("cannot listen on {}:{}", args.host, args.port))?;
    let address = listener.local_addr().context("local address")?;

    println!("VelocitySQL {}", env!("CARGO_PKG_VERSION"));
    println!("listening on {address}  -  protocol: PostgreSQL wire v3");
    println!("authentication: {}", auth.as_str());
    if tracing::enabled!(tracing::Level::INFO) {
        println!("statement log: on (RUST_LOG=vsql_protocol=warn turns it off)");
    } else {
        println!("statement log: off (RUST_LOG=info turns it on)");
    }
    println!(
        "connect with:  psql -h {} -p {} -U {DEFAULT_SUPERUSER}",
        args.host,
        address.port()
    );
    println!("press Ctrl+C to stop\n");

    // Background durability: after every write the session marks the trigger
    // dirty and the saver snapshots promptly — the client's result is already
    // back by then, so persistence never blocks a query. The interval is the
    // safety net for in-flight writes at any moment.
    let stop = Arc::new(AtomicBool::new(false));
    let trigger = Arc::new(PersistTrigger::new());
    engine.set_persist_hook(PersistHook::new({
        let trigger = trigger.clone();
        move || trigger.mark_dirty()
    }));
    persist::start_background_saver(
        engine.clone(),
        snapshot_path.clone(),
        persist::DEFAULT_SNAPSHOT_INTERVAL,
        stop.clone(),
        trigger,
    );

    tokio::select! {
        result = serve(engine.clone(), listener, auth) => {
            result.context("accept loop failed")?;
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("cannot listen for Ctrl+C")?;
            println!("\nshutting down (saving snapshot)");
        }
    }
    stop.store(true, Ordering::SeqCst);
    if let Err(error) = persist::Snapshot::take(&engine).save(&snapshot_path) {
        warn!(%error, "final snapshot save failed");
    }
    Ok(())
}

/// Runs a script of semicolon-separated statements, reporting each tag.
fn run_script(engine: &Arc<Engine>, sql: &str) -> Result<()> {
    let mut session = Session::new(engine.clone());
    for result in session.execute(sql)? {
        info!(tag = %result.tag(), "statement applied");
    }
    Ok(())
}

/// Makes sure the directory the snapshot lives in exists.
///
/// The built-in default sits next to the working directory, so its parent
/// exists by construction. A deployment passes something under `ProgramData` or
/// `/var/lib` instead, where the parent is not there on a first run: saying so at
/// startup beats a background save that fails where nobody is looking.
fn prepare_snapshot_path(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Args> {
        Args::parse(arguments.iter().map(|argument| argument.to_string()))
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let args = parse(&[]).expect("no arguments");
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.port, 5210);
        assert_eq!(args.auth, "scram-sha-256");
        assert_eq!(args.superuser_password, DEFAULT_PASSWORD);
        assert!(
            args.snapshot_path.is_none(),
            "without the flag the built-in path is used"
        );
    }

    #[test]
    fn the_snapshot_path_can_be_moved() {
        // What an installer passes: a path under `ProgramData`, which no service
        // or scheduled task ever has as its working directory.
        let moved = r"C:\ProgramData\VelocitySQL\data\velocitysql.snapshot";
        let args = parse(&["--snapshot-path", moved]).expect("a path");
        assert_eq!(args.snapshot_path.as_deref(), Some(Path::new(moved)));
    }

    #[test]
    fn flags_without_their_value_are_refused_by_name() {
        for (arguments, expected) in [
            (vec!["--snapshot-path"], "--snapshot-path needs a path"),
            (vec!["--port"], "--port needs a value"),
            (vec!["--host"], "--host needs a value"),
        ] {
            let error = parse(&arguments).expect_err("a value is required");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        let error = parse(&["--nope"]).expect_err("unknown flag");
        assert!(
            error.to_string().contains("unrecognized argument"),
            "{error}"
        );
    }

    #[test]
    fn a_snapshot_directory_that_does_not_exist_is_created() {
        let root = std::env::temp_dir().join("velocitysql-snapshot-path");
        let _ = std::fs::remove_dir_all(&root);
        let snapshot = root.join("nested").join("velocitysql.snapshot");

        prepare_snapshot_path(&snapshot).expect("the parent is created");

        assert!(snapshot.parent().expect("parent").is_dir());
        // The default is a bare relative name, whose parent is the working
        // directory and needs nothing done to it.
        prepare_snapshot_path(Path::new("data/velocitysql.snapshot")).expect("the default path");
        let _ = std::fs::remove_dir_all(&root);
    }
}
