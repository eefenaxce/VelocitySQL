# Architecture

## Crates

The workspace is layered, and the dependencies only ever point downwards:

```
vsql-types        values, types, comparison, casts          (no dependencies)
vsql-parser       SQL text -> AST                           types
vsql-catalog      metadata + OIDs + SCRAM verifiers         types
vsql-storage      rows + indexes + constraints              types, catalog
vsql-executor     cluster, planner, executor, sessions      all of the above
vsql-protocol     wire protocol, SCRAM server, routing      executor
vsql-server       the server binary                         protocol
vsql-cli          the embedded console                      executor
```

Two boundaries are worth explaining, because they look arbitrary until you know
why they are where they are:

- `DatabaseDef` and `RoleDef` (the metadata) live in `vsql-catalog`, but "owning a
  database" and "owning a set of roles" live in `vsql-executor`. Holding a
  `Storage` means depending on `vsql-storage`, which depends on `vsql-catalog`, so
  the crate that combines them has to sit above both.
- The planner ships inside `vsql-executor` for now. It becomes its own crate when
  M5 introduces a cost model and the plan representation stops being an
  implementation detail of the executor.

## The path of a statement

1. **Parse.** `vsql_parser::parse_statement` hands the text to `sqlparser-rs` with
   the PostgreSQL dialect, then `lower.rs` walks the result into the crate's own
   AST. Any construct the engine cannot execute is refused here with
   `feature not supported: …` (`0A000`) rather than ignored. This is the one place
   that knows both ASTs.
2. **Normalise identifiers.** An unquoted name folds to lower case, a quoted one
   stays as written, and the comparison is on `String` from then on.
3. **Plan.** `Planner` resolves every column reference to a slot in a scope and
   produces a `Plan` tree. Names disappear: an expression reads an index, not a
   name, so execution does no string lookups. Correlated subqueries resolve
   through a stack of enclosing scopes into `ScalarExpr::Outer { levels_up, index }`,
   which is what makes them correct without a separate de-correlation pass.
4. **Execute.** `exec.rs` turns the plan into a tree of operators with a
   `next() -> Option<Row>` interface, so memory use follows the plan rather than the
   size of the tables (sort, aggregate and the join's inner side materialise; a
   scan does not).
5. **Answer.** The result is a `QueryResult`, and `vsql-protocol` encodes it —
   `RowDescription` for the header, `DataRow` per row, `CommandComplete` with the
   PostgreSQL tag (`SELECT 3`, `INSERT 0 1`).

## Planning details that shape the rest

- **Aggregates bind to the aggregate's output row.** When a query groups, the
  projection is resolved against the grouped row, and an expression structurally
  identical to a `GROUP BY` expression is rewritten to that key's output slot —
  which is why `SELECT a + 1 … GROUP BY a + 1` is legal. Any other bare column gets
  PostgreSQL's `column "x" must appear in the GROUP BY clause …` (`42803`).
- **Index selection happens in the planner.** `WHERE pk = ?` becomes an `Index
  Scan`, as does an equality on a unique index; everything else is a sequential
  scan. There is no cost model yet, so the choice is rule-based.
- **Parameters are untyped until they meet a type.** A driver sends bind parameters
  as text, and `int_col = $1` plans a cast of the parameter to `int`, so the
  comparison — and any index probe built from it — uses the column's type rather
  than the text's.

## Storage

`Table` owns `TableData` behind an `RwLock` and addresses rows by `RowId`, a dense
index into the slot vector. Each slot holds an optional `RowVersion`, which already
carries `xmin`/`xmax`; M1 always writes `XID_NONE`, so MVCC can be filled in later
without changing the layout or rebuilding an index.

Constraints are checked in this layer, not in the planner, so every path that
writes a row is covered — including a WAL replay later on:

- `NOT NULL` and `CHECK` walk the columns on insert and update.
- A unique index refuses a second row for the same key through
  `Index::holds_other_than`, which answers without copying the entry list.
- `FOREIGN KEY` is *declared* and reported by `pg_constraint`, but not enforced yet.

Indexes are kept consistent by `Table::insert`/`update`/`delete`: the old key is
removed and the new one added, so a row that moves between index entries is never
visible under both.

## Cluster, database, session

- `Engine` is the cluster: a map of databases and a map of roles, plus the OID
  source. It is shared as an `Arc` and safe for concurrent sessions.
- `Database` is one database: its `Catalog`, its `Storage` and its views, plus a
  count of the sessions attached to it. `DROP DATABASE` consults that count, and
  `Session`'s `Drop` implementation is what decrements it.
- `Session` is bound to one database for its lifetime. It holds the session
  parameters, the transaction state and the current role's identity. It is not
  shared: one session is one connection.

Concurrency is per table rather than global. A scan takes a snapshot of the live
`RowId`s and then reads rows one at a time, holding the read lock only while
copying a row. The background snapshotter is a reader too, so it never blocks a
query; a write is acknowledged from memory and only marked dirty so the snapshot
follows it.

## Catalog reads

`pg_catalog` relations are not stored. `catalog_scan.rs` materialises them from
live metadata at query time, which is what keeps them from drifting: a `CREATE
TABLE` is visible to the next `pg_class` query with no invalidation step. The
snapshot (`CatalogSnapshot`) is taken once per statement, so a query that reads
`pg_class` twice during a concurrent DDL statement still sees one consistent
cluster.

Consistency with the rest of the engine is a deliberate trade: materialising
costs a pass over the metadata per query, and it buys never having to reconcile a
cached catalog with the real one.
