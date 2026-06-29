# chum

A general-purpose **ClickHouse** schema migration tool — a library and a CLI,
modeled on `sqlx-cli` / `golang-migrate` but built for ClickHouse's quirks.

## Installation

With [`mise`](https://mise.jdx.dev) (preferred) — installs the prebuilt binary
straight from GitHub releases:

```bash
mise use -g github:glennib/chum@latest --pin
```

With [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```bash
cargo binstall chum
```

Or download the archive for your platform from a
[GitHub release](https://github.com/glennib/chum/releases) and put `chum` on
your `PATH`, or build from source:

```bash
cargo install chum
```

To use only the library (no CLI), depend on it with default features off — see
[Library](#library) below.

## Why not just use golang-migrate?

`chum` exists to replace golang-migrate with a Rust-native tool that:

- **Splits multi-statement migrations correctly.** ClickHouse's HTTP and native
  protocols execute one statement per request, so multi-statement files must be
  split client-side. Naive `;`-splitting breaks on semicolons inside strings and
  comments. `chum` uses [`sqlparser`]'s *tokenizer* to find true statement
  boundaries, then slices the original SQL verbatim. (A *full* AST parse is not
  viable — sqlparser's ClickHouse dialect cannot model `CREATE DICTIONARY`, the
  `ENGINE … ORDER BY` tail, `AggregateFunction`, the `JSON` type, and more — but
  we only need boundaries, not a validated AST.)
- **Gathers migrations at compile time** via `include_dir`, or at runtime from a
  directory.
- **Detects drift and partial failures** with per-migration SHA-384 checksums
  and a `dirty` marker.

## Migration files

Files use the `<version>_<name>.{up,down}.sql` convention (identical to sqlx and
golang-migrate). `<version>` is an integer — either a UTC timestamp like
`20260626111407` (the default) or a zero-padded sequential counter like `0001`.
A single file may contain multiple statements separated by `;`.

```
migrations/
  20260626111407_initial_schema.up.sql
  20260626111407_initial_schema.down.sql
```

`chum add` auto-detects which scheme a directory already uses and continues it
(`0001` → `0002`, or a fresh timestamp), so the choice is only made once. Pass
`--sequential` to start a fresh directory with sequential numbers instead of a
timestamp:

```
db/migrations/
  0001_initial_schema.up.sql
  0001_initial_schema.down.sql
  0002_add_users_table.up.sql
  0002_add_users_table.down.sql
```

The two schemes share one numeric ordering space, so they cannot be mixed in a
single directory: `--sequential` refuses to graft a counter onto a directory
that already uses timestamps (it would sort *before* every existing migration).

`chum convert <sequential|timestamp>` switches an existing directory between the
two schemes by renaming files, preserving their order. Use `--dry-run` to
preview the renames first.

- **timestamp → sequential** simply renumbers `0001`, `0002`, … in order.
- **sequential → timestamp** synthesizes `YYYYMMDDHHMMSS` versions one second
  apart, ending one second before "now" — derived from the filename order, not
  from file metadata. (No filesystem timestamp is portable *and* survives a
  checkout/copy intact, so none can be trusted to reflect migration order.)

`convert` is **filesystem-only** — it never touches the database. Because the
bookkeeping table is keyed by the old versions, a database that already applied
these migrations must afterward be reset (drop and recreate the database, then
re-run `chum run`) or have each new version `chum force`d. Convert before a
migration set is widely deployed.

## CLI

```bash
chum run                       # apply all pending migrations
chum run --target 20260626     # apply up to (and including) a version
chum run --dry-run             # show what would be applied, without applying
chum info                      # show applied / pending / dirty state + timing
chum revert                    # revert the most recently applied migration (asks first)
chum revert --steps 2          # revert the two most recent migrations
chum revert --target 20240101  # revert everything newer than a version
chum revert --dry-run          # show what would be reverted, without reverting
chum revert --steps 2 --yes    # revert without the confirmation prompt
chum add add_users_table       # scaffold an up/down pair (continues the dir's scheme; timestamp by default)
chum add --sequential initial  # start a fresh dir with sequential numbers (0001, 0002, …)
chum convert sequential        # renumber every migration to 0001, 0002, … (renames files)
chum convert timestamp         # renumber every migration to UTC timestamps
chum convert sequential --dry-run   # show the renames without performing them
chum force 20260626111407      # mark applied without running (clears dirty)
```

### Output format

By default `chum` prints a colored table on a terminal and switches to
tab-separated values (TSV, with a header row) when stdout is redirected — so a
piped invocation is machine-readable without any flag. `--json` (or
`--format json`) emits JSON instead, and `--format pretty` / `--format tsv`
force a format regardless of the terminal.

```bash
chum info                      # table on a terminal, TSV when piped
chum --json info | jq '.[]'    # JSON
chum --format pretty info      # force the table even when redirected
```

`revert` is the one destructive command: interactively it lists what it will
revert and asks for confirmation. Non-interactively (piped, or `--json` /
`--format tsv`) it refuses unless `--yes` is given, rather than blocking on a
prompt.

Connection, source, and presentation are configured by flags or environment
variables:

| Flag | Env | Default | Notes |
|------|-----|---------|-------|
| `--url` | `CLICKHOUSE_URL` | `http://localhost:8123` | Bare endpoint or full DSN (see below) |
| `--database` | `CLICKHOUSE_DATABASE` | — | Overrides the database in the URL |
| `--user` | `CLICKHOUSE_USER` | — | Overrides the user in the URL |
| `--password` | `CLICKHOUSE_PASSWORD` | — | Overrides the password in the URL |
| `--table` | `CHUM_TABLE` | `_chum_migrations` | |
| `--source` | `CHUM_SOURCE` | `migrations` | Migration directory |
| `--format` | — | `auto` | `auto` \| `pretty` \| `json` \| `tsv` (`auto` = table on a tty, TSV when piped) |
| `--json` | — | — | Shorthand for `--format json` |
| `--color` | — | `auto` | `auto` \| `always` \| `never` (honors `NO_COLOR`) |

`--url` accepts either a bare HTTP endpoint or a full DSN, e.g.
`clickhouse+https://user:pass@host:8123/mydb?secure=true`. The CLI maps it to
the HTTP-only `clickhouse` client:

- userinfo, or `user` / `username` / `password` query params → credentials;
- the first path segment, or a `database` / `db` query param → database;
- a scheme containing `https`, or a truthy `secure` query param → TLS;
- `x-*` query params (golang-migrate driver params) are ignored;
- any other query param becomes a ClickHouse setting.

`--user` / `--password` / `--database` override whatever the URL provides. Set
`RUST_LOG` to control log verbosity (default `info`).

## Library

The library has no notion of connection URLs — it takes a ready-made
`clickhouse::Client`. Building a client from a URL is the CLI's job. A consumer
that only needs the library can switch off the CLI dependencies (clap, url,
tokio, …) with `default-features = false`:

```toml
chum = { version = "0.0.1", default-features = false }
```

Embed migrations at compile time and drive the `Migrator`:

```rust,no_run
use chum::{Migrator, source};
use include_dir::{include_dir, Dir};

static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

# async fn run() -> chum::Result<()> {
let migrator = Migrator::new(source::from_dir(&MIGRATIONS)?);
let client = clickhouse::Client::default()
    .with_url("http://localhost:8123")
    .with_database("mydb");
migrator.run(&client, chum::DEFAULT_TABLE, None).await?;
# Ok(())
# }
```

## How state is tracked

`chum` records applied migrations in a bookkeeping table (default
`_chum_migrations`), engine **`MergeTree`**. The engine is chosen so one DDL
works everywhere: on a managed/`Replicated` ClickHouse, `MergeTree` is
auto-promoted to `ReplicatedMergeTree`, giving consistent bookkeeping across
replicas — whereas the log-family engines a tiny table would suggest (e.g.
golang-migrate's `TinyLog`) are non-replicated and rejected by a `Replicated`
database.

Because ClickHouse has no cheap row updates and no transactional DDL, the table
is **append-only**: each apply writes a `success = false` marker, runs the
statements, then writes a `success = true` marker. The latest state per version
is read with `argMax(col, seq)`, where `seq` is a server-assigned nanosecond
counter (this also leaves a full audit trail of every attempt). If an apply
fails partway, the latest marker stays `false` and the version reads as
**dirty** until resolved (fix the schema by hand, then `chum force <version>`).

`chum` does **not** adopt databases previously migrated by another tool — an
already-migrated database must be handled manually (e.g. `chum force` each
version, or start from a fresh database).

### Cross-replica consistency

The append-only + `argMax` scheme is correct regardless of merge timing *within
a node*. To also get read-after-write *across replicas* — so a later command,
even routed to a different replica behind a load-balanced endpoint, reliably
sees what an earlier one wrote — `chum` applies a bundle of session settings
client-wide on every connection:

| Setting | Purpose |
| --- | --- |
| `insert_quorum=auto`, `insert_quorum_parallel=0`, `select_sequential_consistency=1` | INSERT waits for a majority of replicas; SELECT refuses to read behind it. The three are one bundle — sequential consistency is inert without non-parallel quorum. |
| `lightweight_deletes_sync=2` | the revert `DELETE` waits for all replicas. |
| `async_insert=0`, `wait_end_of_query=1` | the bookkeeping insert is never server-buffered, and the HTTP response waits until it is committed. |

These need no table-DDL or cluster-config change — replication itself is
supplied by the `Replicated` database engine. On a single node they are inert
no-ops. The cross-replica guarantees are documented ClickHouse behavior but are
**not yet exercised against the Aiven cluster**. Any of them can be overridden
with a `?setting=value` query param on `--url` / `CLICKHOUSE_URL`.

## Tests

Unit tests (statement splitter, filename parsing) run with no infrastructure —
`mise run test` (or `cargo nextest run`). The end-to-end test against a live
ClickHouse is `#[ignore]`d by default:

```bash
mise run db:up                                                # start ClickHouse (podman, falls back to docker)
cargo nextest run --run-ignored all -E 'test(full_lifecycle)' # run it
mise run db:down                                              # stop it
```

[`sqlparser`]: https://crates.io/crates/sqlparser
