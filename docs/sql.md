# SQL support

What the engine answers today, and where it deliberately disagrees with
PostgreSQL. Anything not listed here is refused with `feature not supported: …`
(`0A000`) rather than silently ignored.

## Types

`boolean`, `smallint`, `integer`, `bigint`, `real`, `double precision`,
`numeric(p, s)`, `text`, `character varying(n)`, `character(n)`, `bytea`, `date`,
`time`, `timestamp`, `timestamp with time zone`, `interval`, `uuid`, `json`,
`jsonb`, and arrays of these.

The standard pseudotypes `information_schema.character_data`, `sql_identifier`,
`yes_or_no`, `cardinal_number` and `time_stamp`, and PostgreSQL's object-identifier
family (`oid`, `xid`, `cid`, `regclass`, `regproc`, `regnamespace`, `regrole`,
`regtype`, …) map to ordinary types, so `CAST(x AS oid)` and `::regclass` work.

## DDL

| Statement | Notes |
|-----------|-------|
| `CREATE`/`DROP DATABASE` | `IF [NOT] EXISTS`; `DROP` is refused while a session is attached and inside a transaction block |
| `CREATE`/`DROP SCHEMA` | |
| `CREATE TABLE` | `IF NOT EXISTS`, `DEFAULT`, `NOT NULL`, `PRIMARY KEY`, `UNIQUE`, `CHECK`, `REFERENCES`, table-level and named constraints, `SERIAL` |
| `CREATE TABLE AS SELECT` | |
| `CREATE INDEX` | btree and hash, `UNIQUE` |
| `CREATE [OR REPLACE] VIEW`, `DROP VIEW` | Stored as the parsed query |
| `CREATE`/`DROP SEQUENCE` | `nextval`/`currval` defaults, `pg_get_serial_sequence` |
| `ALTER TABLE` | add/drop/rename column, change type, add/drop constraint, owner |
| `TRUNCATE`, `DROP TABLE` | |
| `CREATE ROLE`/`USER`, `ALTER ROLE`/`USER`, `DROP ROLE` | `PASSWORD`, `SUPERUSER`, `LOGIN`, `CREATEDB`, `CREATEROLE`, `IF [NOT] EXISTS` |

`SERIAL` owns a sequence named the way PostgreSQL names it (`t_id_seq`, reachable
through `pg_get_serial_sequence('t', 'id')`) and calls `nextval` on insert. An
explicit value bypasses the default without moving the sequence, exactly as in
PostgreSQL.

## DML

`INSERT` (multi-row `VALUES`, `INSERT … SELECT`, `DEFAULT VALUES`, `DEFAULT`),
`UPDATE`, `DELETE`, each with `RETURNING`. `NOT NULL`, `UNIQUE`, `PRIMARY KEY` and
`CHECK` are enforced; `FOREIGN KEY` is recorded in the catalog but not enforced.

## Queries

- `WHERE`, `ORDER BY` (`ASC`/`DESC`, `NULLS FIRST`/`LAST`, positional and alias
  references), `LIMIT`/`OFFSET`, `DISTINCT`, `DISTINCT ON`
- `GROUP BY`, `HAVING`, and `count`/`sum`/`avg`/`min`/`max`/`string_agg` with
  `DISTINCT` and `FILTER (WHERE …)`
- `INNER`, `LEFT`, `RIGHT`, `FULL` and `CROSS JOIN`, with `ON`, `USING` or `NATURAL`
- Derived tables, non-recursive CTEs, `UNION`/`INTERSECT`/`EXCEPT` with `ALL`
- Scalar, `EXISTS` and `IN` subqueries, correlated or not
- `CASE`, `BETWEEN`, `IN`, `LIKE`/`ILIKE`, `IS NULL`, `IS DISTINCT FROM`, `::` and
  `CAST`, `EXPLAIN` and `EXPLAIN ANALYZE`
- Views are inlined at each reference, so one appearing twice is evaluated twice

## Functions

String: `upper`, `lower`, `length`, `octet_length`, `replace`, `position`,
`substring`, `btrim`/`trim`, `ltrim`, `rtrim`, `concat`, `concat_ws`.

Numeric: `abs`, `sign`, `ceil`, `floor`, `trunc`, `round`, `sqrt`, `power`, `mod`.

Conditional: `coalesce`, `nullif`, `greatest`, `least`.

Date and time: `now`, `current_timestamp`, `transaction_timestamp`,
`statement_timestamp`, `clock_timestamp`, `current_date`, `current_time`,
`timeofday`, `extract`, `date_part`.

Session: `version`, `current_schema`, `current_schemas`, `current_setting`,
`current_user`, `session_user`, `user`, `current_database`, `pg_backend_pid`,
`pg_typeof`.

Catalog helpers: `pg_get_userbyid`, `pg_encoding_to_char`, `pg_get_expr`,
`format_type`, `pg_get_viewdef`, `pg_get_indexdef`, `pg_get_constraintdef`,
`pg_get_serial_sequence`, `pg_table_is_visible`, `pg_is_in_recovery`,
`shobj_description`, `obj_description`, `col_description`, `nextval`.

## Catalog compatibility

`pg_catalog` and `information_schema` are not stored. They are materialised from
live metadata when a query asks for them, so they cannot go stale.

Materialised, with real rows:

```
pg_database  pg_roles     pg_user        pg_namespace  pg_class     pg_attribute
pg_attrdef   pg_type      pg_settings    pg_tables     pg_views     pg_indexes
pg_tablespace pg_constraint pg_am        pg_index      pg_sequence  pg_sequences
```

`information_schema`: `schemata`, `tables`, `columns`, `views`,
`table_constraints`, `key_column_usage`.

Present with PostgreSQL's columns but always empty: 23 more `pg_catalog` relations
(`pg_matviews`, `pg_proc`, `pg_trigger`, `pg_collation`, `pg_operator`,
`pg_extension`, `pg_description`, `pg_depend`, `pg_inherits`, `pg_enum`,
`pg_aggregate`, `pg_opclass`, `pg_rewrite`, `pg_shdescription`, `pg_statistic`,
`pg_policy`, `pg_publication`, `pg_subscription`, `pg_partitioned_table`, and the
foreign-data and user-mapping relations) and `information_schema`'s
`referential_constraints`, `check_constraints`, `routines`, `triggers`,
`sequences`, `parameters`. A tool that joins them gets an empty result instead of
`relation does not exist`, which is what most of them need in order to open at all.

The shape is what matters here, not the emptiness. `SELECT v.tablespace FROM
pg_matviews v` — Navicat issues it while opening a query window — fails the whole
statement if the column is missing, so every one of these relations carries
PostgreSQL's column list in PostgreSQL's order.

Two gaps remain for `psql`'s own `\d` and `\l`: they need a few functions this
engine does not have (`array_to_string` among them) and name-to-OID resolution for
`regclass` (`'t'::regclass` does not consult the catalog). Use the
[console](cli.md) for that.

Views appear in `pg_class` under a synthetic OID derived from their name, and their
columns are not in `pg_attribute` yet.

## Sessions

`SET` and `SHOW` cover the 13 parameters PostgreSQL reports through
`ParameterStatus` (among them `client_encoding`, `DateStyle`, `TimeZone`,
`standard_conforming_strings`, `application_name`), sent at startup and again after
a change, so libpq-based clients cache the right values. A read-only parameter
cannot be `SET` (`55P02`). Unknown parameter names are accepted and stored, because
tools set GUCs this engine does not model yet; known ones do not change execution
behaviour — `datestyle` and `timezone` are recorded and reported, nothing more.

## Where this differs from PostgreSQL, on purpose

- **A double-quoted name that is not a column is read as a string.** In
  `WHERE "type" = "allowRegister"`, if no *unqualified* column is named
  `allowRegister`, that name is the text `'allowRegister'`. MySQL-style clients
  write quotes that way. PostgreSQL would report `42703`. A qualified name
  (`"settings"."type"`) is always a column, and an unquoted one is always a column
  too, so a misspelt column is never silently accepted.
- **`SET` accepts parameters it does not know**, as described above.
- **Names longer than 63 bytes are refused** rather than silently truncated to
  PostgreSQL's `NAMEDATALEN - 1`. A name you cannot use again is worse than an
  error.
- **`ILIKE` folds with `to_lowercase`**, the approximation PostgreSQL makes for
  non-`C` collations in an in-memory engine.
- **`varchar(n)` does not enforce its length** on write.
- **`FOREIGN KEY` is not enforced**; `NOT NULL`, `UNIQUE`, `PRIMARY KEY` and
  `CHECK` are.
- **`ROLLBACK` does not undo anything.** `BEGIN`/`COMMIT`/`ROLLBACK` track state
  only, and `ROLLBACK TO SAVEPOINT` is refused outright.
- **There are no relation-level privileges**, so any role that can log in can read
  and write every table. See the README's limits section.
