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
migration set is widely deployed. `convert` keeps the `.j2` suffix of
[templated migrations](#templated-migrations), and reminds you to rename any
version-prefixed `--var` / `CHUM_VAR_*` keys for the renumbered versions.

## Templated migrations

A migration file ending in `.sql.j2` is a [minijinja](https://docs.rs/minijinja)
(Jinja2) template, rendered before it runs. Use it for SQL that differs between
environments (a `GRANT` only in production, a retention period) or that needs a
value only known at apply time (a backfill cutoff). Plain `.sql` files behave
exactly as before. `chum add --template <name>` scaffolds a `.sql.j2` pair, and
a template up with a plain down (or the reverse) is fine; a plain and a
template file for the same version and direction is an error.

```sql
-- 20261106090000_events.up.sql.j2
CREATE TABLE app.events (ts DateTime, user_id UInt64)
ENGINE = MergeTree ORDER BY (user_id, ts);

{% if env.tier == "prod" %}
GRANT SELECT ON app.events TO analytics_reader;
{% endif %}
```

Rendering depends only on the template and the variables you supply: no clock,
no process environment, no database queries. Undefined variables are errors
(strict mode), so a template that uses a value nobody passed fails before
anything runs, and the error names the missing variable. Output is not escaped
and whitespace is kept as written.

### Variables

Templates read variables from two namespaces, and the form of the key you
supply picks the namespace:

| Key | Example | Template sees | Meaning |
| --- | --- | --- | --- |
| `<name>` | `--var tier=prod` | `env.tier` | Describes the target environment; available to every migration. |
| `<version>_<name>` | `--var 20261106120000_cutoff=2026-11-07` | `this.cutoff` | For that one version only. |

Each key can be given as `--var <key>=<value>` (repeatable, before or after the
subcommand) or as a `CHUM_VAR_<key>=<value>` environment variable, which suits a
container deploy where per-environment values come from a ConfigMap. `--var`
wins over `CHUM_VAR_<key>`. The version in a key is compared numerically
(`0005_x` and `5_x` are the same key), names must match `[A-Za-z_][A-Za-z0-9_]*`,
and giving the same key twice from one source is an error.

`env.cutoff` and `this.cutoff` are different variables: a plain `cutoff` never
reaches `this.cutoff`. Both namespaces always exist, so `is defined`,
`| default(…)` and `env.get("x")` work on a missing key. The template author
decides each variable's kind; to let an operator override an environment value
for one migration, write `{{ this.tier | default(env.tier) }}`.

Values are strings. Two filters quote them for SQL:

- `sql_string` renders a single-quoted string literal with quotes and
  backslashes escaped (`'it\'s'`), for values that end up as strings.
- `sql_ident` renders a backquoted identifier (`` `my db` ``), for database,
  table and column names.

An unfiltered `{{ env.x }}` inserts the value verbatim, which is right for
numbers and SQL fragments. Compare values (`{% if env.tier == "prod" %}`)
rather than testing their truthiness: every non-empty string is true, `"false"`
included. Prefer a required, compared value over `is defined` wherever a
missing value is dangerous, since `is defined` turns a misspelt key into a
silently skipped branch.

Jinja processes the whole file, so `-- {{ env.x }}` still needs `env.x`; use
`{# … #}` for comments about the template. SQL that contains Jinja syntax must
be wrapped in `{% raw %}…{% endraw %}`. `include`, `import` and `extends` are
not available: every template is self-contained.

chum warns about a supplied key that no template in the source refers to (a
typo such as `CHUM_VAR_teir`), and about a `--var` that no migration rendered
by the command used.

### When templates render, and which values they get

| Command | Renders | Variables from |
| --- | --- | --- |
| `render` (offline) | the requested migrations | `--var`, `CHUM_VAR_*` |
| `run`, `run --dry-run` | every pending migration up to `--target` | `--var`, `CHUM_VAR_*` |
| `revert`, `revert --dry-run` | the down of each version reverted | supplied and recorded |
| `force <version>` | the up, without executing it | supplied and recorded |
| `info`, `add`, `convert`, `reset` | nothing | none |

`run` renders and splits every pending migration before the first one runs, and
`revert` does the same with its downs, so a missing variable or template error
anywhere aborts the command before any migration statement runs, listing every
migration that failed. `run --dry-run` lists each value it would use and where
it came from (`--var`, environment, recorded or default).

Each bookkeeping marker records the values the migration used, as namespaced
keys (`env.tier`, `this.cutoff`): the values the up looked up while rendering,
plus every supplied value its down refers to. Values from `| default(…)` aren't
recorded. `revert` and `force` use them:

- **`this.*`: recorded values win.** A supplied value that differs from the
  recorded one is an error if the migration being rendered reads that name, and
  is ignored otherwise, so a stale value left in a deploy spec can't block a
  migration that doesn't use it. A supplied value for a name that wasn't
  recorded fills the gap.
- **`env.*`: supplied values win**, since the environment may have changed since
  the apply; recorded values fill gaps, and chum warns when a supplied value
  differs from the recorded one.

`chum info` shows the recorded values in a `vars` column (a `vars` object in
JSON). For pending templates, JSON output also lists `template_vars`, the
namespaced names the up template refers to, so you can see what to pass. Read
recorded values only from an **applied** version, since a dirty marker records a
failed attempt:

```bash
chum --json info \
  | jq -r '.[] | select(.version == 20261106120000 and .state == "applied") | .vars["this.cutoff"] // empty'
```

The checksum covers the template text, not the rendered SQL, so environments
that ran different SQL record the same checksum, and editing an applied
template is drift as usual.

### Previewing: `chum render`

`chum render` prints the SQL migrations render to and never connects to
ClickHouse, so it works in CI or code review and doubles as a pre-flight check:
it fails exactly where `run` would.

```text
chum render [VERSION]... [--from VERSION] [--down] [--var key=value]...
```

With versions it renders those; with `--from`, every migration from that
version on; with neither, every template in the source. `--down` renders the
downs instead, newest first. Plain migrations are printed as they are. Each
migration is preceded by a `-- <file>` comment and the output is valid SQL.
`render` can't see recorded values, so `render --down` matches what `revert`
would run only when given the values the up used.

### Guards and `chum reset`

chum doesn't type or validate variables. Check them in SQL with `throwIf`, next
to where they are used; it runs on the server before the statements after it:

```sql
SELECT throwIf({{ env.tier | sql_string }} NOT IN ('local', 'staging', 'prod'), 'unknown tier');
```

A failing guard leaves the version dirty with nothing applied. Fix the input,
run `chum reset <version>` to make the version pending again, and run again.
`reset` deletes the version's bookkeeping rows and touches no database objects,
so use it only when the failed attempt left nothing behind (or you cleaned up by
hand). It refuses a version that isn't dirty.

Every migration also runs in every fresh database (a laptop, CI, a rebuild), so
guard on the condition that makes a value necessary, such as whether the
affected table has data, rather than on a particular environment. `chum render
--from <version>` with a local `env.*` and no `this.*` shows whether a fresh
database can migrate without per-apply values.

### Example: a materialized view with a future cutoff

A materialized view only processes rows inserted after it is created, so older
rows come from a backfill that runs outside chum. To split the two without gaps
or overlaps, the view filters on a cutoff in the future, and once the cutoff has
passed the backfill inserts everything before it. Each environment applies the
migration on its own day, with its own cutoff:

```sql
-- 20261106120000_events_hourly_mv.up.sql.j2
{% if this.cutoff is defined %}
SELECT throwIf(
    toDateTime({{ this.cutoff | sql_string }}, 'UTC') <= now()
        AND (SELECT count() FROM app.events) > 0,
    'cutoff must be in the future while app.events has data'
);
{% else %}
SELECT throwIf(
    (SELECT count() FROM app.events) > 0,
    'app.events has data: pass 20261106120000_cutoff'
);
{% endif %}

CREATE MATERIALIZED VIEW app.events_hourly_mv
TO app.events_hourly AS
SELECT toStartOfHour(ts) AS hour, count() AS events
FROM app.events
{% if this.cutoff is defined %}
WHERE ts >= toDateTime({{ this.cutoff | sql_string }}, 'UTC')
{% endif %}
GROUP BY hour;
```

The down needs no variables, so it can be a plain `.down.sql` file containing
`DROP VIEW app.events_hourly_mv;`.

The guards decide whether a cutoff is needed from the data, not from the
environment's name. Once `app.events` has data (staging, production), a cutoff
is required and must be in the future. While it is empty (a laptop, CI, a new or
rebuilt database), the view covers everything from the start, there is nothing
to backfill, and any cutoff or none is accepted.

```bash
chum --url "$STAGING_URL" run --var 20261106120000_cutoff='2026-11-06 14:00:00'
```

Where chum runs as a Kubernetes Job, the release that ships the migration adds
`CHUM_VAR_20261106120000_cutoff` to the Job's environment. `run` ignores it once
the version is applied, but `revert`, `force` and the guard in a rebuilt
database still see it, so remove it once every environment has applied the
migration. After the cutoff has passed, the backfill reads that environment's
cutoff with the `chum info` query above and inserts `WHERE ts < cutoff`; no
recorded cutoff means the table was empty and there is nothing to backfill.

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
chum add --template backfill   # scaffold a .sql.j2 template pair instead of plain .sql
chum render --var tier=prod    # print the SQL every template renders to (offline)
chum render --from 20260626 --down  # print the down SQL from a version on, newest first
chum convert sequential        # renumber every migration to 0001, 0002, … (renames files)
chum convert timestamp         # renumber every migration to UTC timestamps
chum convert sequential --dry-run   # show the renames without performing them
chum force 20260626111407      # mark applied without running (clears dirty)
chum reset 20260626111407      # make a dirty version pending again (asks first)
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
| `--url` | *(none)* | `http://localhost:8123` | Bare endpoint or full DSN (see below). Explicit flag only — **not** read from the environment. |
| `--database` | *(none)* | — | Session default database for **unqualified** names in your migration SQL — *not* where bookkeeping lives (see below). Explicit flag or DSN only — **not** read from the environment. If set, it must already exist. |
| `--user` | *(none)* | — | Overrides the user in the URL. Explicit flag or DSN only — **not** read from the environment. |
| `--password` | *(none)* | — | Overrides the password in the URL. Explicit flag or DSN only — **not** read from the environment. |
| `--table` | `CHUM_TABLE` | `_chum_migrations` | Bookkeeping table name |
| `--bookkeeping-database` | `CHUM_BOOKKEEPING_DATABASE` | `_chum` | Dedicated database holding the bookkeeping table; chum creates it |
| `--source` | `CHUM_SOURCE` | `migrations` | Migration directory |
| `--format` | — | `auto` | `auto` \| `pretty` \| `json` \| `tsv` (`auto` = table on a tty, TSV when piped) |
| `--json` | — | — | Shorthand for `--format json` |
| `--color` | — | `auto` | `auto` \| `always` \| `never` (honors `NO_COLOR`) |
| `--var <key>=<value>` | `CHUM_VAR_<key>` | — | A variable for [templated migrations](#templated-migrations). Repeatable. A plain key sets `env.<key>`; a version-prefixed key (`<version>_<name>`) sets `this.<name>` for that version only. `--var` wins over `CHUM_VAR_<key>`. Accepted by `run`, `revert`, `render` and `force`, before or after the subcommand. |

`--url` accepts either a bare HTTP endpoint or a full DSN, e.g.
`clickhouse+https://user:pass@host:8123/mydb?secure=true`. The CLI maps it to
the HTTP-only `clickhouse` client:

- userinfo, or `user` / `username` / `password` query params → credentials;
- the first path segment, or a `database` / `db` query param → the **session
  default database** (see below);
- a scheme containing `https`, or a truthy `secure` query param → TLS;
- `x-*` query params (golang-migrate driver params) are ignored;
- any other query param becomes a ClickHouse setting.

`--user` / `--password` / `--database` override whatever the URL provides. Set
`RUST_LOG` to control log verbosity (default `info`).

### chum reads no `CLICKHOUSE_*` environment variables

chum reads **no `CLICKHOUSE_*` environment variables at all** — not
`CLICKHOUSE_URL`, `CLICKHOUSE_USER`, `CLICKHOUSE_PASSWORD`, nor
`CLICKHOUSE_DATABASE`. The connection is supplied entirely by an explicit
`--url` (which accepts a full DSN carrying user / password / database via the
DSN userinfo, path, and `database`/`db` query param) plus the explicit `--user`
/ `--password` / `--database` override flags. When `--url` is omitted it
defaults to `http://localhost:8123`, so a bare `chum` targets localhost — never
an ambient cluster.

This is deliberate. A migrator that silently connected to whatever target the
environment happened to point at — or assembled one from scattered ambient
`CLICKHOUSE_*` partials — is how a stray invocation ends up on the wrong (or
production) cluster. Requiring an explicit `--url` (plus explicit override
flags) prevents that. It also matters for the database specifically: chum
commonly shares an `.env` with the application it migrates, which legitimately
sets `CLICKHOUSE_DATABASE=<appdb>`; inheriting it would pin chum's session to an
app database that may not exist yet, breaking the create-your-own-database flow
(below) even though bookkeeping lives in `--bookkeeping-database`.

Only `CHUM_TABLE`, `CHUM_BOOKKEEPING_DATABASE`, `CHUM_SOURCE` and the
`CHUM_VAR_<key>` template variables are env-readable — those are `CHUM_*` tool
config, not part of the connection target. `CHUM_VAR_<key>` is the one family
clap can't declare, because the prefix is dynamic: chum reads every environment
variable starting with `CHUM_VAR_` and nothing else, so an unrelated `PATH` or
`CLICKHOUSE_URL` never reaches a template.

### `--database` vs `--bookkeeping-database`

These are two different things, and keeping them separate is what lets a
migration bootstrap its own database:

- **`--bookkeeping-database`** (default `_chum`) is chum's own — it holds the
  bookkeeping table. chum runs `CREATE DATABASE IF NOT EXISTS` for it and
  fully-qualifies every bookkeeping statement as
  `<bookkeeping-database>.<table>`, so bookkeeping **never depends on any app
  database existing**.
- **`--database`** is the *session default* for **unqualified** object names in
  your migration SQL. It is **not** where bookkeeping goes. By default it is
  left unset, and chum connects on ClickHouse's built-in `default` database
  (which always exists) rather than pinning a possibly-missing app database.
  This is deliberate: it lets a migration create and fully-qualify its own
  database, e.g.

  ```sql
  CREATE DATABASE app;
  CREATE TABLE app.events (…) ENGINE = MergeTree ORDER BY …;
  ```

  with no `--database` and no database pre-created. Set `--database` only if
  your migrations rely on unqualified names against an existing session default
  — in which case that database **must already exist**, because ClickHouse
  rejects any request whose session default database is absent. `--database`
  must be passed explicitly (or via the URL); it is never inherited from
  `CLICKHOUSE_DATABASE` in the environment.

## Library

The library has no notion of connection URLs — it takes a ready-made
`clickhouse::Client`. Building a client from a URL is the CLI's job. A consumer
that only needs the library can switch off the CLI dependencies (clap, url,
tokio, …) with `default-features = false`:

```toml
chum = { version = "0.1.0", default-features = false }
```

Embed migrations at compile time and drive the `Migrator`:

```rust,no_run
use chum::{Bookkeeping, Migrator, source};
use include_dir::{include_dir, Dir};

static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

# async fn run() -> chum::Result<()> {
let migrator = Migrator::new(source::from_dir(&MIGRATIONS)?);
// No app database pinned as the session default, so migrations can create and
// fully-qualify their own databases.
let client = clickhouse::Client::default()
    .with_url("http://localhost:8123");
// Bookkeeping lives in its own database (default `_chum`), fully-qualified as
// `_chum._chum_migrations`.
let bookkeeping = Bookkeeping::new(chum::DEFAULT_BOOKKEEPING_DATABASE, chum::DEFAULT_TABLE)?;
migrator.run(&client, &bookkeeping, None).await?;
# Ok(())
# }
```

Templated migrations need the `template` feature (enabled by `cli`), which
adds the `minijinja` dependency. Pass variables with `Migrator::vars`, e.g.
`Vars::new().with_env("tier", "prod")`, or build them from `--var`-style keys
with `Vars::from_keys`. Without the feature, a source containing a `.sql.j2`
file fails to load with `TemplatingDisabled`.

The migrator takes a [`Bookkeeping`] target (database + table). It runs
`CREATE DATABASE IF NOT EXISTS` for the bookkeeping database and fully-qualifies
all bookkeeping SQL, so it never needs an app database to exist.

## How state is tracked

`chum` records applied migrations in a bookkeeping table (default
`_chum_migrations`) that lives in its **own dedicated database** (default
`_chum`, override with `--bookkeeping-database`). chum creates that database
(`CREATE DATABASE IF NOT EXISTS`) and fully-qualifies every bookkeeping
statement as `<bookkeeping-database>.<table>`. Keeping bookkeeping in its own
database — rather than in the connection's default database — means chum never
depends on any app database existing, so a migration is free to create its own
database (`CREATE DATABASE app; CREATE TABLE app.t …`) as its first step. See
[`--database` vs `--bookkeeping-database`](#--database-vs---bookkeeping-database)
above.

The bookkeeping table's engine is **`MergeTree`**. The engine is chosen so one
DDL works everywhere: on a managed/`Replicated` ClickHouse, `MergeTree` is
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
**dirty** until resolved (fix the schema by hand, then `chum force <version>`;
or, if the attempt changed nothing, `chum reset <version>` to make it pending
again). A revert likewise writes a `success = false` marker before running the
down migration and deletes the version's rows once it finishes, so a revert that
fails partway also leaves the version dirty. Finish the revert by hand and run
`chum reset <version>`, or restore the version and run `chum force <version>`.
The error names the failing statement and its position in the file.

Each marker also records, in a `vars Map(String, String)` column, the template
variables the migration used (see
[Templated migrations](#when-templates-render-and-which-values-they-get)); plain
migrations record an empty map. A table created by an older chum lacks the
column: commands that write markers (`run`, `revert`, `force`) add it with
`ALTER TABLE … ADD COLUMN IF NOT EXISTS`, and read-only commands treat it as
empty without altering the table. An older chum keeps working against the
upgraded table, since it names its columns.

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
with a `?setting=value` query param on `--url`.

## Tests

Unit tests (statement splitter, filename parsing) run with no infrastructure —
`mise run test` (or `cargo nextest run`). The end-to-end test against a live
ClickHouse is `#[ignore]`d by default:

The compose file publishes ClickHouse on host port **8124** (not the default
8123) so it never collides with another project's ClickHouse. The e2e tests
default `CLICKHOUSE_URL` to `http://localhost:8124`.

```bash
mise run db:up                                                # start ClickHouse on :8124 (podman, falls back to docker)
cargo nextest run --run-ignored all                           # run every e2e test
cargo nextest run --run-ignored all -E 'test(full_lifecycle)' # or just one
mise run db:down                                              # stop it
```

The e2e suite proves the dedicated-bookkeeping-database model end to end: chum
creates `_chum`, applies a migration that itself runs
`CREATE DATABASE …; CREATE TABLE ….t …` without the app database pre-existing,
records bookkeeping in `_chum._chum_migrations`, re-runs idempotently, honors a
`--bookkeeping-database` override, and reverts cleanly.

[`sqlparser`]: https://crates.io/crates/sqlparser
