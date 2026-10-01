# Persistence

Durability is on by default and it is a snapshot, not a write-ahead log. The
distinction matters more than the mechanism, so start with what it buys.

Writes are applied in memory and acknowledged from memory. A background task
serialises the cluster to one file, so the answer to "what happens on restart" is
"you lose whatever was still in flight", not "you lose everything". There is no
transaction atomicity and no recovery to a crash point; that is milestone M2.

## The file

`data/velocitysql.snapshot`, relative to the server's working directory, encoded
with `bincode`: a binary format, not JSON. It is written to a temporary file,
`fsync`ed, and renamed into place, so a reader sees either the previous snapshot or
the new one and never a half-written file.

The file starts with a format version (currently 5). On a version the engine does
not understand, the snapshot is **not** overwritten: it is renamed to
`velocitysql.snapshot.bak` and the server starts with an empty cluster, which is
recoverable by hand. Silently truncating it would not be.

## When it is written

| Trigger | When |
|---------|------|
| A write | Promptly after every statement that changed something. The session marks the trigger dirty *after* the statement has been applied and its result handed back, so persistence never delays a query |
| A timer | At least once every 5 seconds (`DEFAULT_SNAPSHOT_INTERVAL`), as a safety net |
| Shutdown | Once more, on `Ctrl+C` |

The task only reads: it takes each table's read lock long enough to copy the rows,
so it competes with queries for CPU and memory, not for write access.

## What a snapshot contains

- Every role: name, OID, attributes, and the SCRAM verifier exactly as stored
- Every database: name, OID, owner
- Per database: user schemas, sequences, tables — columns, defaults, keys, indexes,
  checks, foreign keys — and the rows those tables hold
- Views, stored as their parsed query rather than as SQL text, so a definition with
  joins, aggregates or `ORDER BY` round-trips without depending on how faithfully a
  statement can be rendered back to text

`numeric` and JSON values are stored as text. `bincode` is not self-describing, and
the `Deserialize` implementations of `rust_decimal` and `serde_json` need
`deserialize_any`, which such a format cannot provide; deriving them directly
produces values that write and cannot be read back.
`a_snapshot_round_trips_every_value_the_engine_has` walks every value type to keep
that from returning.

## Restore

Restore runs before the server accepts clients, and it is best-effort: an object
that cannot be rebuilt — a referenced table that vanished, a type the engine no
longer understands — is skipped rather than aborting the whole file.

- **An object that already exists wins.** `--init` and `--demo` run first, so their
  tables are left alone and only their rows are replayed. The same rule keeps
  `--superuser-password` authoritative: the bootstrap role already exists, so the
  password in the snapshot is ignored instead of resurrecting an old one.
- **A missing object is installed under its original OID.** A database created at
  runtime is not in a freshly built cluster, so it is restored rather than skipped.
- **The OID source is advanced past everything restored.** Otherwise the next
  `CREATE` would hand out an OID that is already on disk.

Restoring is therefore idempotent: running it twice over the same file leaves the
same cluster.

## What it is not

- Not a write-ahead log. A statement is visible to `SELECT` before it is durable,
  and a hard kill loses writes that had already been acknowledged.
- Not transactional. Half of a multi-statement unit of work can survive.
- Not incremental. Every snapshot rewrites the whole cluster, so the cost is
  proportional to the data, not to what changed. A cluster with a large working set
  pays both the memory to serialise it and the I/O to write it.

M2 replaces this with a WAL plus per-database files, at which point the snapshot
becomes a checkpoint that the log replays on top of.
