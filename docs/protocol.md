# Wire protocol

`vsql-protocol` implements PostgreSQL wire protocol v3 well enough that `psql`,
`lib/pq` and `node-postgres` connect without special-casing. The interesting parts
are the ones a driver checks: what arrives in what order, and where a statement is
allowed to fail.

## Startup

An `SSLRequest` is answered with a single `'N'` — a legal refusal, and the reason
`psql` falls back to plaintext without complaining. `GSSENCRequest` and
`CancelRequest` are recognised; there is no TLS and no query cancellation yet,
which the server says so in its log rather than pretending otherwise.

The startup packet's `user` and `database` decide the session. The order matters:

1. Authenticate the role. A failure is `FATAL 28P01`, sent before any
   `ParameterStatus`.
2. Then check that the database exists. An unknown one is `FATAL 3D000`, also
   before any `ParameterStatus`.

Both close the connection immediately. A client distinguishes "could not connect"
from "the statement failed" by exactly this: a `FATAL` before the status parameters
is a login problem, an `ErrorResponse` after them is a statement problem. Getting
this order wrong is how a driver ends up reporting a bad password for a missing
database.

## Simple query

`Query` takes one or more statements, separated by `;`. Each one is executed in
turn and answered as it finishes: `RowDescription` (text format), `DataRow` per row,
`CommandComplete` with the PostgreSQL tag (`SELECT 3`, `INSERT 0 1`, `CREATE TABLE`).
An empty or comment-only string gets `EmptyQueryResponse`. A `SET` that changes a
cached parameter emits `ParameterStatus` before `ReadyForQuery`, which is where
libpq expects it while updating its cache.

## Extended query

`Parse` plans the statement and caches it under a name (the unnamed statement is a
valid name). Planning happens here, so a syntactically broken or unplannable query
fails at `Parse`, not at `Execute` — which is where a tool's error dialog gets its
message from.

`Describe` on a *statement* starts with `ParameterDescription`, whose length is the
number of `$n` the statement uses. The count is what drivers validate against the
arguments they were handed; lib/pq refuses to run anything else, reporting
`got 3 parameters but the statement requires 0`. The type OIDs are sent as `0`
("unspecified"), PostgreSQL's answer for a parameter it cannot pin down at parse
time, and clients then send text.

`Describe` returns `RowDescription` whenever the statement *can* return rows,
including `INSERT … UPDATE … RETURNING`; only statements with no result set get
`NoData`. `Execute` then returns `DataRow` messages and `CommandComplete` and
nothing else — repeating `RowDescription` there is a protocol error that makes
lib/pq abort with `unexpected message during extended query execution: "(T)
RowDescription"`.

`Close` drops a statement or a portal; `Sync` ends the round trip; `Flush` pushes
whatever is buffered.

### Parameters

Parameters arrive in text format. A binary parameter is refused with
`feature not supported: binary parameter format` rather than being misread as text.

A text parameter has no type of its own — the context gives it one, the way
PostgreSQL resolves an `unknown` literal. `int_col = $1` plans a cast of the
parameter to `int`, so the comparison and any index probe use the column's type.
The value's meaning follows the target type: `{"a":1}` bound to a `jsonb` column is
JSON, while `{1,2}` is an array only where an array is expected (`int[]` column,
`id = ANY($1)`, `unnest($1)`). Guessing "array" from the shape at `Bind` time is
what turns JSON into `text[]`.

Binding the wrong number of parameters is refused at `Bind`, in PostgreSQL's words
and with its SQLSTATE (`08P01`), rather than failing later with an unbound `$n`.

### Result formats

`Bind` chooses a format per result column, and both are honoured. `lib/pq` asks for
binary on the types it decodes natively, and a server that answered in text would
hand it bytes it reads as garbage.

| Binary results | Value in the protocol |
|---|---|
| `bool`, `int2`, `int4`, `int8` | network byte order |
| `float4`, `float8` | IEEE 754 |
| `numeric` | PostgreSQL's base-10000 digits |
| `date`, `time`, `timestamp`, `timestamptz`, `interval` | microseconds since the PostgreSQL epoch |
| `uuid`, `text`, `bytea`, `json`, `jsonb` | the value's bytes |

A type without a binary encoder — an array, so far — is reported as unsupported
instead of being sent in the wrong format.

## Error recovery

In the extended protocol one failure does not end the connection. The server
answers with `ErrorResponse`, sets `sync_required`, and discards every message
until `Sync` arrives, then reports `ReadyForQuery`. That is what keeps a driver's
connection usable after a bad statement.

A failure inside a transaction block moves the session into the aborted state, and
`ReadyForQuery` reports `E` until `ROLLBACK`. Outside a transaction it reports `I`,
and after `BEGIN` it reports `T`.

## Authentication

`SCRAM-SHA-256` (the default) challenges every login, including for a role that
does not exist: the decoy challenge is indistinguishable from the real one, so the
handshake cannot be used to enumerate accounts. A failure is always the same
`28P01` message. The client's proof is verified against the role's stored verifier,
and the server's signature is sent so the client can prove the server holds it too.

`--auth trust` skips this and accepts any existing role, which the server warns
about at startup. `SCRAM-SHA-256-PLUS` (channel binding) needs TLS and is not
offered.

The startup sequence never sends an `Authentication` message after
`AuthenticationOk`. libpq treats a second `R` as a protocol violation and reports
`unexpected response from server; first received character was "R"`; a wire test
pins that down.

## Statement logging

Every statement is logged at `info`, the level the server shows by default, with
the SQL, the bound parameters and `elapsed=`. Failures go to `warn` with the same
fields. Both are written when the statement finishes, since that is when the
duration is known; `Parse` is what the log calls a planned statement.

A password in `CREATE`/`ALTER ROLE` is masked to `'***'` before the line is
written. The wire carries role passwords in clear text — SCRAM protects the login,
not the DDL — so a verbatim log would put them on disk. Nothing else is rewritten,
so `SELECT password FROM secrets` is logged as written.

`RUST_LOG=vsql_protocol=warn` keeps the failures and drops the statements;
`RUST_LOG=debug` adds connection-level detail.

## Deliberate limits

- No TLS: `SSLRequest` is refused with `'N'`. The password never crosses the wire,
  but the rest of the session does.
- No binary parameters, as above.
- `Execute` with a row limit is refused: a portal holds the complete result set, so
  a partial fetch could not be honoured faithfully.
- No query cancellation: `CancelRequest` is logged and ignored.
