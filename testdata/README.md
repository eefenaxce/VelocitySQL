# Sample data

Three SQL files that build something worth querying. They use only SQL this
engine already answers, and `crates/vsql-executor/tests/testdata.rs` loads all
three on every `cargo test`, so they cannot drift away from the engine.

| File | Builds | Goes into |
|------|--------|-----------|
| `seed.sql` | The `shop` schema: customers, products, orders, order lines, three indexes and the `order_totals` view. 8 customers, 12 products, 10 orders, 20 order lines. | the default database |
| `cluster.sql` | Login role `app` and database `analytics`. | cluster level |
| `analytics.sql` | `events` (16 rows), `daily_rollup` keyed on `(day, source)`, and the `events_by_source` view. | the `analytics` database |

## Loading

`--init` takes SQL text, not a path, so hand it the file's contents:

```bash
# Unix shell
cargo run -p vsql-server -- --init "$(cat testdata/seed.sql)"
```

```powershell
# PowerShell
cargo run -p vsql-server -- --init (Get-Content testdata/seed.sql -Raw)
```

The console reads SQL from stdin when it is not a terminal, which is easier for
a file this size:

```bash
cargo run -p vsql-cli < testdata/seed.sql
```

With `--password` the console authenticates through the same SCRAM path a driver
uses, which is how the `app-pass` role above is checked.

A database is a separate catalog and a session is bound to one database, so the
warehouse takes three commands:

```bash
cargo run -p vsql-server -- --superuser-password 'TopSecret!' \
    --init "$(cat testdata/cluster.sql)"
psql -h 127.0.0.1 -p 5210 -U app -d analytics -f testdata/analytics.sql
psql -h 127.0.0.1 -p 5210 -U app -d analytics
```

`app` logs in with `app-pass`. It is an ordinary role: it reads and writes
tables, and it is refused when it tries to create a role or a database.

## Queries the data was built for

```sql
SELECT order_id, total FROM shop.order_totals ORDER BY total DESC;

SELECT c.full_name, sum(t.total) AS spent
  FROM shop.customers c
  JOIN shop.order_totals t ON t.customer_id = c.id
 GROUP BY c.full_name
 ORDER BY spent DESC;

SELECT category, count(*) FROM shop.products GROUP BY category ORDER BY category;
SELECT count(*) FROM shop.products WHERE in_stock = 0;

SELECT day, source, events FROM events_by_source WHERE source = 'partner' ORDER BY day;
```

The expected results are asserted in `crates/vsql-executor/tests/testdata.rs`, so
if a number below ever disagrees with the engine, that test fails first.

| Query | Result |
|-------|--------|
| `SELECT count(*) FROM shop.customers` | 8 |
| `SELECT count(*) FROM shop.order_lines` | 20 |
| `SELECT total FROM shop.order_totals WHERE order_id = 1` | 114.90 |
| `SELECT count(*) FROM shop.products WHERE in_stock = 0` | 3 |
| `SELECT count(*) FROM events` (in `analytics`) | 16 |
