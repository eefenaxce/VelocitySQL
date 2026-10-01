# VelocitySQL

[![CI](https://github.com/eefenaxce/VelocitySQL/actions/workflows/ci.yml/badge.svg)](https://github.com/eefenaxce/VelocitySQL/actions/workflows/ci.yml)
[![coverage](https://codecov.io/gh/eefenaxce/VelocitySQL/graph/badge.svg)](https://codecov.io/gh/eefenaxce/VelocitySQL)

An in-memory SQL engine that speaks the PostgreSQL wire protocol, written in Rust.

Point `psql`, Navicat, or a driver at it and send SQL. Tables live in memory, and a
background task keeps a snapshot on disk so a restart does not lose them.

## Status

| Milestone | Content | State |
|-----------|---------|-------|
| M1 | Parser, type system, in-memory tables, `SELECT`/`INSERT` | done |
| M2 | MVCC, write-ahead log, crash recovery | snapshot persistence done; MVCC and WAL pending |
| M3 | Wire protocol v3, per-database routing, SCRAM authentication | done, minus TLS |
| M4 | `ALTER TABLE`, sequences, `ON CONFLICT` | `ALTER TABLE` and sequences done; `ON CONFLICT` pending |
| M5 | Cost-based optimizer, hash join, window functions | not started |
| M6 | `pg_catalog` / `information_schema` compatibility | partial, see [docs/sql.md](docs/sql.md) |

240 unit and integration tests plus 4 doc-tests pass. CI runs on every push and
pull request: `rustfmt`, `clippy -- -D warnings`, the test suite on Linux and
Windows, `rustdoc` with warnings denied, and coverage. The commands to run them
locally are in [docs/development.md](docs/development.md).

## Run it

```bash
# Server: 127.0.0.1:5210, SCRAM authentication, with the demo schema preloaded.
cargo run -p vsql-server -- --demo
```

```
VelocitySQL 0.1.0
listening on 127.0.0.1:5210  -  protocol: PostgreSQL wire v3
authentication: scram-sha-256
statement log: on (RUST_LOG=vsql_protocol=warn turns it off)
connect with:  psql -h 127.0.0.1 -p 5210 -U velocitysql
press Ctrl+C to stop
```

```bash
# Any PostgreSQL client works. The bootstrap role is `velocitysql`.
psql -h 127.0.0.1 -p 5210 -U velocitysql -d velocitysql

# Drivers too, without a psql installation:
#   DSN: postgres://velocitysql:velocitysql@127.0.0.1:5210/velocitysql
```

The default password is documented (`velocitysql`) and the server prints a warning
while it is in use; pass `--superuser-password` to change it. TLS is not
implemented, so `SSLRequest` is answered with `'N'` and clients fall back to
plaintext (see [docs/protocol.md](docs/protocol.md)).

## Or use the built-in console

`velocitysql-cli` runs the engine in-process, so it needs no server and no network
round trip. Useful for trying SQL and for the meta-commands that a stock `psql`
cannot run yet.

```bash
cargo run -p vsql-cli -- --demo                        # interactive
cargo run -p vsql-cli -- --demo --timing -c "SELECT current_user"
cargo run -p vsql-cli -- -d analytics --password app-pass
```

```
velocitysql=> \l             list databases (* marks the current one)
velocitysql=> \c analytics   switch database
analytics=>  \du            list roles, their attributes and whether they have a password
analytics=>  \dt            tables in the current database
analytics=>  \d events      columns, types, nullability, defaults, and indexes
analytics=>  \dn \di \? \q
```

[docs/cli.md](docs/cli.md) documents every command and flag.

## What it answers

| Area | Summary |
|------|---------|
| DDL | `CREATE`/`DROP DATABASE`, `SCHEMA`, `TABLE` (defaults, `NOT NULL`, `PRIMARY KEY`, `UNIQUE`, `CHECK`, `REFERENCES`, `SERIAL`, `CREATE TABLE AS SELECT`), `INDEX` (btree/hash, `UNIQUE`), `VIEW` (`OR REPLACE`), `SEQUENCE`, `ROLE`/`USER`, `ALTER TABLE`, `TRUNCATE`, `DROP TABLE` |
| DML | `INSERT` (multi-row, `INSERT … SELECT`, `DEFAULT`), `UPDATE`, `DELETE`, all with `RETURNING` |
| Queries | `WHERE`, `ORDER BY` (including `NULLS FIRST/LAST`), `LIMIT`/`OFFSET`, `DISTINCT [ON]`, `GROUP BY`/`HAVING`, `count`/`sum`/`avg`/`min`/`max`/`string_agg` with `DISTINCT` and `FILTER`, `INNER`/`LEFT`/`RIGHT`/`FULL`/`CROSS JOIN`, derived tables, non-recursive CTEs, `UNION`/`INTERSECT`/`EXCEPT [ALL]`, scalar/`EXISTS`/`IN` subqueries (including correlated), `EXPLAIN [ANALYZE]` |
| Catalogs | 18 `pg_catalog` relations materialized from live metadata, 23 more present with PostgreSQL's columns but no rows, plus 6 materialized and 6 empty `information_schema` relations |
| Sessions | `SET`/`SHOW` over the 13 standard parameters PostgreSQL reports through `ParameterStatus` |
| Auth | SCRAM-SHA-256 with per-role verifiers, `SUPERUSER`/`CREATEDB`/`CREATEROLE`/`LOGIN` attributes |
| Protocol | Simple and extended query, bind parameters, text and binary result formats, `Prepare`/`Bind`/`Describe`/`Execute`/`Close` |

SQL support in detail, including the places this engine deliberately differs from
PostgreSQL, is in [docs/sql.md](docs/sql.md).

## Databases and roles

One process is a cluster in the PostgreSQL sense:

```
Engine                            one process = one cluster
  ├── Database  "velocitysql", "postgres", anything you create
  │     └── Catalog + Storage + views      one set per database, mutually invisible
  │           └── Schema (pg_catalog, public, information_schema)
  │                 └── tables, indexes, sequences
  └── Role      "velocitysql", anything you create   cluster-wide, like pg_authid
```

```sql
CREATE DATABASE shop;
CREATE DATABASE IF NOT EXISTS shop;      -- idempotent
DROP DATABASE shop;                      -- refused while someone is connected
```

A session is bound to one database for its lifetime. That is why a cross-database
query is impossible rather than merely discouraged, and why `\c` (or a new
connection) is the only way to switch. `CREATE DATABASE` and `CREATE ROLE` need the
matching attribute; an ordinary role that tries either gets `42501`.

Passwords are stored the way PostgreSQL stores them: as a SCRAM verifier
(`SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`, i.e. `pg_authid.rolpassword`),
never as plaintext. A role that does not exist still receives a challenge, so the
handshake cannot be used to enumerate accounts. `vsql-catalog::scram` is checked
against the test vectors published in RFC 7677 §3, which rules out a client and a
server agreeing on the same wrong implementation.

## Persistence

Writes go to memory and are acknowledged from memory. A background task serialises
the whole cluster — roles, databases, schemas, tables, rows, views — to
`data/velocitysql.snapshot` (bincode, so binary rather than JSON; written to a
temporary file, `fsync`ed, then renamed into place). The snapshot is triggered
promptly after every write, with a periodic fallback and one final save on
shutdown, so a restart loses at most the writes still in flight.

This is a snapshot, not a write-ahead log: there is no transaction atomicity and no
crash-point recovery. That is M2. [docs/persistence.md](docs/persistence.md) has the
format, the restore rules, and what a snapshot will not do.

## Sample data

`testdata/` holds three SQL files: a storefront with customers, products, orders and
a `order_totals` view; a second database with its own login role; and a small
warehouse to put in it.

```bash
cargo run -p vsql-server -- --init "$(cat testdata/seed.sql)"
cargo run -p vsql-cli < testdata/seed.sql
```

`crates/vsql-executor/tests/testdata.rs` loads all three on every `cargo test`, so
they cannot drift away from the engine. See [testdata/README.md](testdata/README.md).

## Logging

Every statement is logged at `info`, which is the level the server shows by default:
`sql=`, the bound parameters, and `elapsed=`. Failures are logged at `warn` with the
same fields, because "column `x` does not exist" is not actionable without knowing
which statement produced it. A password inside `CREATE`/`ALTER ROLE` is masked to
`'***'` before anything is written.

```
2026-07-01T09:14:22Z  INFO execute sql=SELECT * FROM orders WHERE customer_id = $1 params=$1=7 elapsed=412.4µs
2026-07-01T09:14:22Z  WARN statement failed error=column "v.tablespace" does not exist sql=SELECT v.tablespace FROM pg_matviews v elapsed=398.2µs
```

`RUST_LOG` narrows this: `RUST_LOG=vsql_protocol=warn` keeps the failures and drops
the statements, `RUST_LOG=debug` adds connection-level detail.

## Project layout

| Crate | Responsibility |
|-------|----------------|
| `vsql-types` | Values, the type system, comparison/ordering/hashing, the cast matrix |
| `vsql-parser` | `sqlparser-rs` (PostgreSQL dialect) → its own AST, with unsupported constructs refused during lowering |
| `vsql-catalog` | Metadata for databases, schemas, tables, columns, indexes, sequences and roles; OID allocation; SCRAM verifiers |
| `vsql-storage` | Row storage addressed by `RowId`, btree and hash indexes, constraint checks |
| `vsql-executor` | Cluster container (`Engine`/`Database`), role registry and authentication, planner, volcano-style executor, sessions, snapshot persistence |
| `vsql-protocol` | Wire protocol v3 encoding/decoding, the SCRAM server handshake, per-database and per-role routing |
| `vsql-server` | The server binary: listening socket, configuration, bootstrap |
| `vsql-cli` | The embedded console |

[docs/architecture.md](docs/architecture.md) explains how a statement travels through
these crates and the invariants each one keeps.

## Documentation

| Document | Contents |
|----------|----------|
| [docs/architecture.md](docs/architecture.md) | Crate boundaries, the path a statement takes, concurrency, the decisions behind M1 |
| [docs/sql.md](docs/sql.md) | Supported SQL, catalogs, functions and types, plus deliberate deviations from PostgreSQL |
| [docs/protocol.md](docs/protocol.md) | Startup, simple and extended query, result formats, error recovery, authentication |
| [docs/persistence.md](docs/persistence.md) | Snapshot contents, when it is written, restore and version rules |
| [docs/cli.md](docs/cli.md) | Console commands and every flag of both binaries |
| [docs/development.md](docs/development.md) | Build, test, lint, and how to add a test |
| [testdata/README.md](testdata/README.md) | Sample databases and the queries they were built for |

中文文档：[README.zh-CN.md](README.zh-CN.md)。

## Known limits

- No TLS, no `SCRAM-SHA-256-PLUS`, so the connection itself is plaintext even though
  the password never crosses it.
- No relation-level privileges: only role attributes. There is no `GRANT`/`REVOKE`,
  no table ownership, no row-level security, and no role membership — any role that
  can log in can read and write every table.
- `FOREIGN KEY` is declared and reported by `pg_constraint`, but nothing enforces it
  on insert or delete yet. `NOT NULL`, `UNIQUE`, `PRIMARY KEY` and `CHECK` are
  enforced.
- Transactions track state only: `BEGIN`/`COMMIT`/`ROLLBACK` work, but a `ROLLBACK`
  does not undo anything, and `ROLLBACK TO SAVEPOINT` is refused.
- Joins are nested loops over a materialised inner side. `UPDATE` overwrites rows in
  place. Views and CTEs are inlined at each reference and may be evaluated more than
  once.
- `varchar(n)` does not enforce its length on write.

## License

[Apache-2.0](LICENSE).
