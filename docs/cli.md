# Command line

## `velocitysql-server`

```bash
cargo run -p vsql-server -- [options]
```

| Flag | Value | Default | Meaning |
|------|-------|---------|---------|
| `--host <ADDR>` | address | `127.0.0.1` | Address to listen on |
| `-p`, `--port <PORT>` | port | `5210` | Port to listen on |
| `--auth <METHOD>` | `scram-sha-256`, `trust` | `scram-sha-256` | How to authenticate a login |
| `--superuser-password <VALUE>` | text | `velocitysql` | Password of the bootstrap role |
| `--demo` | — | off | Create the sample `users`/`orders` schema before serving |
| `--init <SQL>` | SQL text | — | Run SQL before serving (see below) |
| `-h`, `--help` | — | — | Print help |
| `-V`, `--version` | — | — | Print the version |

The server prints a banner when it is ready, and two warnings when they apply: one
while the bootstrap role still uses the documented default password, one while
`--auth trust` accepts any existing role without a password.

`--init` takes SQL text, not a path:

```bash
cargo run -p vsql-server -- --init "$(cat testdata/seed.sql)"
cargo run -p vsql-server -- --init "CREATE TABLE t (id SERIAL PRIMARY KEY)"
```

The snapshot lives at `data/velocitysql.snapshot`, relative to the working
directory; run the server from the same place to keep its data (see
[persistence.md](persistence.md)).

## `velocitysql-cli`

The console runs the engine in the same process, so it needs no server. It is also
the fallback for the introspection `psql` cannot do yet, because its meta-commands
read the catalog through the Rust API instead of `pg_catalog`.

```bash
cargo run -p vsql-cli -- [options]
```

| Flag | Value | Default | Meaning |
|------|-------|---------|---------|
| `-c`, `--command <SQL>` | SQL text | — | Run this and exit |
| `-d`, `--database`, `--dbname <NAME>` | name | `velocitysql` | Database to connect to |
| `-U`, `--username <ROLE>` | role | `velocitysql` | Role to connect as |
| `--password <VALUE>` | text | — | Authenticate with SCRAM, as a driver would |
| `-W`, `--prompt-password` | — | off | Read the password from stdin |
| `--demo` | — | off | Create the sample schema first |
| `--timing` | — | off | Print how long each statement took |
| `-h`, `--help` | — | — | Print help |
| `-V`, `--version` | — | — | Print the version |

Without `--password` the console connects in-process without authenticating, which
is what makes it usable as a scratchpad. With it, the login takes the same SCRAM
path a driver takes, so a role's password and attributes can be checked for real.

When stdin is not a terminal the banner and the prompt are suppressed, which makes
piping a file work:

```bash
cargo run -p vsql-cli < testdata/seed.sql
```

In interactive mode, statements are terminated by `;` and may span lines; the
prompt is the database name (`shop=>`, continuation `shop->`).

## Meta-commands

| Command | Prints |
|---------|--------|
| `\l`, `\list` | Every database: name, owner, encoding, and `*` on the current one |
| `\c`, `\connect <database>` | Switches database, then says which one you are in |
| `\du`, `\dg` | Every role: name, attributes, whether it has a password, and `*` on the current one |
| `\dn` | Schemas: name, kind (`system` or user), number of tables |
| `\dt` | Tables in the current database: schema, name, row count (system tables are skipped) |
| `\di` | Indexes: table, index, columns, the constraint it backs (`PRIMARY KEY`/`UNIQUE`), access method |
| `\d <table>` | The columns of one table — name, type, nullability, default — followed by its indexes |
| `\d` | Same as `\dt` |
| `\timing` | Toggles per-statement timing |
| `\?`, `\h`, `\help` | The same help text as `--help` |
| `\q`, `\quit` | Quits |

Results are rendered as `psql` renders them: `+----+` rules, numbers right-aligned,
`NULL` spelled out, and a footer with `(1 row)` or `(N rows)`. A statement that
fails prints the PostgreSQL message and SQLSTATE and leaves the session usable.

## Examples

```bash
# A one-off query against a fresh in-memory cluster.
cargo run -p vsql-cli -- --demo -c "SELECT count(*) FROM orders"

# Start where a driver would: a role with a password, in its own database.
cargo run -p vsql-cli -- -d analytics -U app --password app-pass

# Load the sample storefront, then look around.
cargo run -p vsql-cli < testdata/seed.sql
```
