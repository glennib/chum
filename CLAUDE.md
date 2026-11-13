# CLAUDE.md — chum

## Project Overview

**chum** is a general-purpose **ClickHouse** schema migration tool — both a
Rust **library** and a **CLI** (`chum`). It is modeled on `sqlx-cli` /
`golang-migrate` but built for ClickHouse's quirks: client-side statement
splitting, compile-time or runtime migration sources, and append-only
bookkeeping with per-migration SHA-384 checksums. See `README.md` for the user
manual and the rationale.

## Testing

- **Unit/integration tests**: `mise run test` (or `cargo nextest run
  --all-targets`). These need no infrastructure.
- **End-to-end tests**: `#[ignore]`d by default. Start ClickHouse with `mise run
  db:up` (compose publishes it on host port **8124**, not 8123, to avoid
  colliding with another project's ClickHouse), then `cargo nextest run
  --run-ignored all` (the tests default `CLICKHOUSE_URL` to
  `http://localhost:8124`). They embed the fixture directories under `tests/`
  and cover: the full apply → re-run → info → revert cycle with bookkeeping in
  a dedicated database; a migration that creates its **own** database with the
  app db not pre-existing; the `--bookkeeping-database` override; `?` / `??` in
  migration SQL reaching ClickHouse verbatim; templated migrations recording
  their variables and unsets and reverting from them; dirty versions after a
  failed statement; and upgrading a bookkeeping table from every schema an
  older chum release created.
- **Final pre-commit check**: `mise run ci` — runs `fmt:check`, `clippy`,
  `clippy:lib`, `clippy:lib:template`, `test`, `build:debug`, `build:lib`, and
  `build:lib:template`. Run it after implementing changes and before
  committing; a green `mise run ci` is the bar for trusting any change. It
  skips the end-to-end tests, so run those too when a change touches
  bookkeeping or anything else that talks to ClickHouse.

## Library / CLI split

The crate is split by the `cli` feature (on by default):

- **Library** — the `Migrator`, sources, splitter, and backend. It takes a
  ready-made `clickhouse::Client` and knows nothing about connection URLs.
  Errors are concrete `ChumError` variants (`thiserror`). The library must
  compile with **no default features** (only `chrono`, `clickhouse`,
  `include_dir`, `serde`, `sha2`, `sqlparser`, `thiserror`). `mise run
  build:lib` / `clippy:lib` guard this advertised contract — do not let
  CLI-only deps leak into the library. The opt-in `diagnostic` feature adds a
  `miette::Diagnostic` derive (error codes + `help` text) on `ChumError`,
  pulling in `miette` (no `fancy`); it is **off** in the no-default-features
  build, so the contract above is unchanged. The opt-in `template` feature
  adds `minijinja` to render `.sql.j2` / `.j2.sql` migrations; without it a
  source containing a template fails to load. `mise run build:lib:template` /
  `clippy:lib:template` guard the library with only that feature.
- **CLI** (`src/bin/chum.rs`, `required-features = ["cli"]`) — owns the
  connection story (URL/DSN → `clickhouse::Client`), presentation, and the
  interactive prompts. The `cli` feature enables `template`. The binary uses
  `miette` for error reporting (the `cli` feature enables `diagnostic` +
  `miette/fancy`, so the library's per-variant codes and `help` survive `?` and
  render in the graphical report); the library itself never reports, only
  returns `ChumError`.

## CLI flags and env vars travel together

Every user-facing flag is a clap field with `#[arg(long, env = "…",
default_value = …)]` in `src/bin/chum.rs`. When adding, renaming, or changing a
flag's default, update **both** touchpoints in the same change:

1. the clap field (and its `env`/`default_value`), and
2. `README.md` — the flag/env table, including its "Accepted by" column (and
   any per-subcommand usage block).

A flag is offered only by the subcommands that use it, so each `--help` lists
only what applies. A flag used by several subcommands lives in its own
`#[derive(clap::Args)]` struct (`SourceArgs`, `ConnectionArgs`,
`BookkeepingArgs`, `VarArgs`, `YesArgs`) flattened into each of them. Only
flags every subcommand uses (`OutputArgs`) are `global = true`, so they go
before or after the subcommand; all others go after it.

There is no separate config file: clap is the single source of truth for
defaults and env-var mapping. The exception is the prefixed variables
`CHUM_VAR_<key>` and `CHUM_UNSET_<key>`, which clap can't express: `VarArgs`
reads them from the environment by hand (`env_vars`), and the README's flag
table lists them beside `--var` / `--unset`.

## Architecture

```
src/
├── lib.rs        # Public API surface and crate docs
├── migrator.rs   # Migrator: plan/apply/undo/info, State/Status/Progress
├── migration.rs  # Migration, AppliedMigration, Direction, checksums
├── source.rs     # Migration sources: from_dir (embedded) / from_path (runtime), filename parsing
├── split.rs      # Statement splitter (sqlparser tokenizer → boundaries, not a full AST)
├── template.rs   # minijinja rendering and checking of templated migrations (`template` feature)
├── vars.rs       # Vars: `env.*` / `this.*` values and unsets, their origins, MissingThis
├── backend.rs    # ClickHouse bookkeeping: the `Bookkeeping` target (dedicated db + table), schema upgrades, append-only inserts, argMax reads
├── error.rs      # ChumError (thiserror) + Result alias
└── bin/chum.rs   # CLI: URL/DSN → client, subcommands, table/TSV/JSON output
tests/
├── integration.rs                     # e2e tests (#[ignore]d) + filename-scan unit test
├── migrations/                        # generic fixture schema with tricky ClickHouse DDL
├── bootstrap_migrations{,_override}/  # migrations that create their own database
├── question_mark_migrations/          # `?` / `??` that must reach ClickHouse verbatim
├── template_migrations/               # templated fixtures using `env.*` and `this.*`
└── upgrade_migrations/                # run against bookkeeping tables in older schemas
```

## Conventions

### Rust

- Edition 2024.
- Use `cargo nextest run` for testing, not `cargo test`.
- Prefer `cargo run --bin chum` and `cargo build --bin chum`.
- **Never use `#[allow(...)]`**. Use `#[expect(..., reason = "...")]` instead —
  it requires a reason and warns when the expectation becomes unnecessary.
- Clippy runs with `pedantic` warnings denied in CI (`-D warnings`).

### Formatting

- Formatting uses stable rustfmt; `rustfmt.toml` holds only stable options.
- Run `mise run fmt` (or `cargo fmt --all`) before committing; CI checks it
  with `mise run fmt:check`.

### Error handling

- Library: concrete `ChumError` variants via `thiserror`, each carrying a
  `miette` `code` + `help` under the `diagnostic` feature; return
  `chum::Result<T>`.
- CLI binary: `miette` for human-facing reporting — `.into_diagnostic()` to
  adopt foreign (std/`url`/`serde_json`) errors, then `.with_context(...)`;
  `ChumError` flows in directly via `?` and keeps its diagnostics.

## ClickHouse design notes

These are load-bearing decisions documented at length in `README.md`; the short
version for code changes:

- **Bookkeeping lives in its own dedicated database** (`Bookkeeping { database,
  table }`, default `_chum._chum_migrations`, overridable via
  `--bookkeeping-database` / `CHUM_BOOKKEEPING_DATABASE` and `--table` /
  `CHUM_TABLE`). chum runs `CREATE DATABASE IF NOT EXISTS <bookkeeping-database>`
  (`backend::ensure_database`, called before `ensure_table`) and
  **fully-qualifies every bookkeeping statement** as `<database>.<table>`.
  Both identifiers are validated by `backend::validate_ident` (interpolated, not
  bound). This decoupling is what lets a migration create its **own** database:
  bookkeeping never depends on any app database existing.
- **The session default database is never pinned to a possibly-missing app db.**
  `build_client` calls `with_database` only when `--database` (or the URL
  path/`database` query param) is explicitly given; otherwise the client stays
  on the `clickhouse` crate's built-in `default` database (which always exists).
  ClickHouse rejects any request whose session default database is absent
  (verified against 25.12), so `--database`, if set, must already exist — it is
  the session default for *unqualified* names in migration SQL, not where
  bookkeeping goes. The intended model is that migrations fully-qualify their own
  object names and leave `--database` unset.
- **chum reads NO `CLICKHOUSE_*` env vars** — none of `--url`, `--user`,
  `--password`, `--database` carries a clap `env` binding, so no connection
  config is inherited from the environment. The connection target is supplied by
  an explicit `--url` (a full DSN can carry user/password/database via
  userinfo/path/query) plus the explicit override flags; `--url` keeps its
  `http://localhost:8123` default, so a bare `chum` targets localhost, never an
  ambient cluster. Rationale: a migrator must not silently connect to whatever
  target the environment points at (that's how a stray invocation hits the wrong
  or prod cluster), and it matters especially for the database — chum commonly
  shares an `.env` with the app it migrates, which sets
  `CLICKHOUSE_DATABASE=<appdb>`; inheriting it would pin the session to a
  possibly-missing app db and break the create-your-own-db flow. The database
  resolution in `build_client` is `cli.database.or(path_database).or(database_q)`
  — `cli.database` (and `cli.user`/`cli.password`/`cli.url`) are populated only
  from the CLI. **Only `CHUM_*` tool config stays env-readable** (`CHUM_TABLE`,
  `CHUM_BOOKKEEPING_DATABASE`, `CHUM_SOURCE`, `CHUM_STRICT_CONSISTENCY`,
  `CHUM_MISSING_THIS`, `CHUM_VAR_*`, `CHUM_UNSET_*`).
- **Bookkeeping engine is `MergeTree`** so one DDL works on both single-node and
  `Replicated` ClickHouse (auto-promoted to `ReplicatedMergeTree`).
- **Append-only state** — each apply writes a `success = false` marker, runs the
  statements, then a `success = true` marker. Latest state per version is read
  with `argMax(col, seq)`; a stuck `false` reads as **dirty**. Each marker
  also records the template variables the version used (`vars`) and the
  `this.*` names it was applied without (`unset`), which `revert` and `force`
  render from.
- **The bookkeeping schema only ever gains columns, and every older schema
  must keep working.** `ensure_table` creates the current schema, but `CREATE
  TABLE IF NOT EXISTS` leaves an older table as it was. A new column is
  appended to `backend::ADDED_COLUMNS` (and to `AppliedRow`, at the same
  position); the `CREATE TABLE`, the idempotent `ADD COLUMN IF NOT EXISTS`
  upgrade that only writing commands run, and the value read-only commands
  read for a missing column without altering the table all come from that
  list, as does the check every command runs on the table's columns (spell a
  type exactly as `system.columns` reports it, or the check rejects the
  table chum just created). Give the column a `DEFAULT` that is correct for
  rows an older chum writes, since it keeps writing to the upgraded table and
  names only its own columns. That is why `records_unsets` exists: it is
  `false` for those rows, which record values but not unsets. Add the schema
  of each release to `OLD_TABLES` in `tests/integration.rs` and to the table
  in `backend.rs`'s module docs.
- **Statement splitting uses sqlparser's tokenizer, not a full parse** —
  ClickHouse DDL (`CREATE DICTIONARY`, `ENGINE … ORDER BY`, `AggregateFunction`,
  `JSON`) cannot be modeled as a validated AST, but statement boundaries can be
  found robustly. The splitter slices the original SQL verbatim between
  boundaries. `tests/migrations/` deliberately includes this tricky DDL.
- **Cross-replica consistency** comes from a client-wide bundle of session
  settings (quorum + sequential consistency, sync deletes, no async insert), not
  table-DDL or cluster config. It is **opt-in** (`--strict-consistency` /
  `CHUM_STRICT_CONSISTENCY`) because its `insert_quorum` settings hung every
  bookkeeping `INSERT` against an Aiven cluster.

## Distribution

- Versioning/changelog/publishing is handled by **release-plz** (see
  `release-plz.toml` and `.github/workflows/release-plz.yml`) — it opens release
  PRs and, on merge, publishes to crates.io via trusted publishing.
- Prebuilt binaries are built by **cargo-dist** (`dist-workspace.toml` +
  `.github/workflows/release.yml`). **The release workflow is generated by
  `dist` — never hand-edit `release.yml`; run `dist init` / `dist generate`
  and commit the result.**
- Toolchain versions are pinned in `mise.toml`.
