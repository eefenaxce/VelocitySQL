# Development

## Commands

```bash
cargo build --workspace
cargo test --workspace                                   # 240 tests + 4 doc-tests
cargo test -p vsql-executor --test m1_end_to_end          # one suite
cargo clippy --workspace --all-targets -- -D warnings     # clean
cargo fmt --all -- --check                                # clean
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## Continuous integration

`.github/workflows/ci.yml` runs five jobs on every push and pull request, and they
are the same commands as above:

| Job | Command |
|-----|---------|
| `rustfmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| `test` | `cargo test --workspace --locked`, on `ubuntu-latest` and `windows-latest` |
| `rustdoc` | `cargo doc --workspace --no-deps --locked`, with `RUSTDOCFLAGS=-D warnings` |
| `coverage` | `cargo llvm-cov --workspace --lcov --output-path lcov.info` |

A push to a branch cancels the run it supersedes.

## Coverage

The `coverage` job instruments the suite with `cargo-llvm-cov`, prints the table
into the run's summary (visible as "coverage" on the Actions page, no third-party
account needed), uploads `lcov.info` as an artifact, and uploads to Codecov when
the run is a push rather than a pull request.

To get the same numbers locally:

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked

cargo llvm-cov --workspace --lcov --output-path lcov.info   # run the suite
cargo llvm-cov report --summary-only                        # the table
cargo llvm-cov --workspace --html                           # browsable report
```

Coverage is a guide, not a gate: nothing fails because a line is uncovered. The
suites that carry the weight are the end-to-end ones, and a test that pins a
PostgreSQL behaviour is worth more than a percentage of the lines it touches.

The baseline is 68.8% of lines and 71.2% of regions. The widest single hole is
`vsql-server/src/main.rs` (0%, because it is a binary and no test run executes it);
past that the uncovered code is spread over error paths and branches a healthy run
never reaches, with `vsql-types/src/data_type.rs` (36.6%) and
`vsql-parser/src/lower.rs` (64.0%) the two largest files among them.


## Test layout

Tests live next to what they test, and there is no mocking layer: everything runs
against the real engine.

| Target | Tests | Covers |
|--------|-------|--------|
| `vsql-types` | 14 | Comparison and NULL semantics, implicit casts, ordering, date/time and interval rendering, arrays |
| `vsql-parser` | 14 | Identifier folding, literal types, error messages, the `CREATE USER` rewrite, role DDL |
| `vsql-catalog` | 13 | Namespaces, OID allocation, default schemas, constraint and index lookup, SCRAM (RFC 7677 vectors) |
| `vsql-storage` | 10 | Insert/update/delete with index maintenance, unique and `NOT NULL` enforcement, range scans, truncate |
| `vsql-executor` | 19 | Three-valued logic, type promotion, `LIKE`, scopes, the database registry, snapshot round-trips |
| `vsql-executor::m1_end_to_end` | 28 | The full stack: DDL, DML, joins, aggregates, subqueries, CTEs, `RETURNING`, `EXPLAIN`, views |
| `vsql-executor::alter_table` | 24 | Add/drop/rename column, change type, constraints, ownership |
| `vsql-executor::pg_catalog` | 22 | The materialised `pg_catalog` relations, their column shapes, the rendering functions |
| `vsql-executor::databases` | 13 | Isolation, `3D000`/`42P04`/`55006`/`25001`/`42939`, `\c` semantics |
| `vsql-executor::roles` | 12 | Password storage and verification, `28P01`, permission refusals, `DROP ROLE` rules |
| `vsql-executor::information_schema` | 8 | The `information_schema` views and their column types |
| `vsql-executor::session_parameters` | 8 | `SET`/`SHOW`, `ParameterStatus` reporting, read-only parameters |
| `vsql-executor::array_subscript` | 7 | Array indexing and slicing |
| `vsql-executor::table_constraints` | 5 | `SERIAL` sequences, foreign keys as reported by the catalog, `CHECK` |
| `vsql-executor::testdata` | 4 | Loads `testdata/*.sql` and asserts the queries the samples exist for |
| `vsql-protocol` | 8 | Text encoding, `typlen`, parameter rendering |
| `vsql-protocol::wire` | 28 | Real TCP: startup, simple and extended query, binary results, `Describe`, error recovery, SCRAM, refusal cases |
| `vsql-cli` | 3 | Table rendering and the row-count footer |

`vsql-server` has no tests of its own; its behaviour is the protocol suite plus the
binaries' argument parsing.

## How a test is written

A test states the PostgreSQL behaviour it pins down, and asserts on that behaviour
rather than on the current implementation. Suites that go through SQL share the
same three helpers:

```rust
fn session() -> Session { Session::new(Arc::new(Engine::new())) }

let rows = text_rows(&mut session, "SELECT count(*) FROM orders");
assert_eq!(rows, ["3"]);
```

`text_rows` renders each row as `a|b` with `NULL` spelled out, which keeps the
assertion readable and independent of the value representation.

The protocol suite is deliberately different: it speaks to a real socket with a
hand-written client, because framing, message order and SQLSTATEs are exactly the
things an in-process test cannot check.

## Sample data

`testdata/` holds the SQL that the documentation tells a reader to run, and
`crates/vsql-executor/tests/testdata.rs` loads it. Adding to a sample file is
therefore safe — if a statement stops working, the suite says so before a reader
does. See [../testdata/README.md](../testdata/README.md).

## Adding SQL

The path a new construct takes: `vsql-parser/src/lower.rs` (if `sqlparser-rs` needs
rewriting or the construct is refused), `planner.rs` (if it changes what a name
resolves to), `exec.rs` (if it is a new operator), `functions.rs` (for a scalar
function). Unsupported constructs are refused in `lower.rs` with a specific
message; there is no `todo!()` anywhere in the workspace, on purpose.

Tests belong in the suite that matches the change: `m1_end_to_end` for observable
SQL behaviour, the `pg_catalog`/`information_schema` suites when a catalog view is
affected, and the wire suite when the change is visible to a driver.

## Adding a catalog relation

An empty relation needs a name and PostgreSQL's exact column list inside
`EMPTY_RELATIONS` in `crates/vsql-executor/src/catalog_scan.rs` — the column list is
the point, since tools `SELECT` columns from relations they know are empty.
Something with real rows needs a `CatalogSource` variant and a materialiser. Both
belong in `pg_catalog.rs` with an assertion on the shape, because that is what a
tool will do first.

## Releasing

`.github/workflows/release.yml` runs on a `v*` tag and on demand. It needs no
secrets: the GitHub Release and the image both authenticate with the workflow's
own `GITHUB_TOKEN`.

```bash
# 1. the version lives in one place, the workspace manifest
$EDITOR Cargo.toml        # [workspace.package] version = "0.2.0"
git commit -am "Release 0.2.0"
git push

# 2. the tag names the release, and has to agree with the manifest
git tag v0.2.0
git push origin v0.2.0
```

The workflow then:

1. **verifies** — refuses the tag when it disagrees with `Cargo.toml`, then runs
   the suite with `--locked`, so a release is never built from a red commit;
2. **packages** — builds the release profile for `x86_64-unknown-linux-gnu` and
   `x86_64-pc-windows-msvc` (the Windows build links the CRT statically, so the
   archive runs on a machine without the MSVC redistributable);
3. **publishes a GitHub Release** with both archives, a `SHA256SUMS` and notes
   generated from the commits since the last tag;
4. **pushes the image** to `ghcr.io/<owner>/<repo>`: the version, the minor series
   and `latest` for a tag, `edge` for a manual run against a branch.

Two things that bite once:

- The image name is lowercased by the workflow, because an image reference may
  not contain capitals and `github.repository` keeps them.
- A GHCR package starts out **private** even when the repository is public. To let
  people pull anonymously, set its visibility in the repository's package
  settings after the first push.

The image is `linux/amd64`. An arm64 image means building under emulation; add
QEMU and `platforms: linux/amd64,linux/arm64` to the build step if that is wanted.

### The Windows installer

`installer/velocitysql.iss` builds the wizard the release carries. The Windows job
compiles it from the binaries it just built, which is also how to do it by hand:

```powershell
choco install innosetup -y
& 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' `
    '/DAppVersion=0.1.0' '/DSourceDir=..\target\release' 'installer\velocitysql.iss'
```

It writes `dist/velocitysql-<version>-x86_64-pc-windows-msvc-setup.exe`.

`installer/register-task.ps1` is what the installer runs to register the boot-time
task, and what an administrator re-runs to move an existing installation to
another port or binding. It uses `Register-ScheduledTask` rather than
`schtasks /TR`, because the former takes the executable and its arguments as two
separate values while the latter needs a quoted command line — and a quoted
command line containing a path with spaces is where installers usually break.

## Comments

Comments explain why a thing is the way it is, especially when the code looks odd
until you know the driver behaviour or the PostgreSQL rule behind it. Restating what
the next line does is noise. Doc comments on public items are a rustdoc obligation
and are checked by the doctests in `vsql-catalog`, `vsql-executor`, `vsql-parser`
and `vsql-storage`.
