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
- **End-to-end test**: `#[ignore]`d by default. Start ClickHouse with `mise run
  db:up`, then `cargo nextest run --run-ignored all -E 'test(full_lifecycle)'`.
  It embeds `tests/migrations/` and runs the full apply → re-run → info →
  revert cycle against a throwaway database it creates and drops.
- **Final pre-commit check**: `mise run ci` — runs `fmt:check`, `clippy`,
  `clippy:lib`, `test`, `build:debug`, and `build:lib`. Run it after
  implementing changes and before committing; a green `mise run ci` is the bar
  for trusting any change.

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
  build, so the contract above is unchanged.
- **CLI** (`src/bin/chum.rs`, `required-features = ["cli"]`) — owns the
  connection story (URL/DSN → `clickhouse::Client`), presentation, and the
  interactive prompts. The binary uses `miette` for error reporting (the `cli`
  feature enables `diagnostic` + `miette/fancy`, so the library's per-variant
  codes and `help` survive `?` and render in the graphical report); the library
  itself never reports, only returns `ChumError`.

## CLI flags and env vars travel together

Every user-facing flag is a clap field with `#[arg(long, env = "…",
default_value = …)]` in `src/bin/chum.rs`. When adding, renaming, or changing a
flag's default, update **both** touchpoints in the same change:

1. the clap field (and its `env`/`default_value`), and
2. `README.md` — the flag/env table (and any per-subcommand usage block).

There is no separate config file: clap is the single source of truth for
defaults and env-var mapping.

## Architecture

```
src/
├── lib.rs        # Public API surface and crate docs
├── migrator.rs   # Migrator: plan/apply/undo/info, State/Status/Progress
├── migration.rs  # Migration, AppliedMigration, Direction, checksums
├── source.rs     # Migration sources: from_dir (embedded) / from_path (runtime), filename parsing
├── split.rs      # Statement splitter (sqlparser tokenizer → boundaries, not a full AST)
├── backend.rs    # ClickHouse bookkeeping table: append-only inserts, argMax reads
├── error.rs      # ChumError (thiserror) + Result alias
└── bin/chum.rs   # CLI: URL/DSN → client, subcommands, table/TSV/JSON output
tests/
├── integration.rs   # e2e lifecycle test (#[ignore]d) + filename-scan unit test
└── migrations/      # generic fixture schema embedded by the e2e test
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

- `rustfmt.toml` uses nightly-only options (`format_strings`, `group_imports`,
  `imports_granularity`, `wrap_comments`, `doc_comment_code_block_width`).
- Run `mise run fmt:nightly` (or `cargo +nightly fmt --all`) for full
  formatting locally. **Always run it before committing.**

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

- **Bookkeeping engine is `MergeTree`** so one DDL works on both single-node and
  `Replicated` ClickHouse (auto-promoted to `ReplicatedMergeTree`).
- **Append-only state** — each apply writes a `success = false` marker, runs the
  statements, then a `success = true` marker. Latest state per version is read
  with `argMax(col, seq)`; a stuck `false` reads as **dirty**.
- **Statement splitting uses sqlparser's tokenizer, not a full parse** —
  ClickHouse DDL (`CREATE DICTIONARY`, `ENGINE … ORDER BY`, `AggregateFunction`,
  `JSON`) cannot be modeled as a validated AST, but statement boundaries can be
  found robustly. The splitter slices the original SQL verbatim between
  boundaries. `tests/migrations/` deliberately includes this tricky DDL.
- **Cross-replica consistency** is achieved with a client-wide bundle of session
  settings (quorum + sequential consistency, sync deletes, no async insert), not
  table-DDL or cluster config.

## Distribution

- Versioning/changelog/publishing is handled by **release-plz** (see
  `release-plz.toml` and `.github/workflows/release-plz.yml`) — it opens release
  PRs and, on merge, publishes to crates.io via trusted publishing.
- Prebuilt binaries are built by **cargo-dist** (`dist-workspace.toml` +
  `.github/workflows/release.yml`). **The release workflow is generated by
  `dist` — never hand-edit `release.yml`; run `dist init` / `dist generate`
  and commit the result.**
- Toolchain versions are pinned in `mise.toml`.
