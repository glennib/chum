# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.2](https://github.com/glennib/chum/compare/v0.2.1...v0.2.2) - 2026-08-04

### Other

- *(deps)* update dependency cargo:cargo-nextest to v0.9.143 ([#35](https://github.com/glennib/chum/pull/35))
- *(deps)* lock file maintenance ([#34](https://github.com/glennib/chum/pull/34))
- *(deps)* update rust crate clap to v4.6.5 ([#33](https://github.com/glennib/chum/pull/33))
- *(deps)* update dependency cargo:release-plz to v0.3.160 ([#32](https://github.com/glennib/chum/pull/32))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.140 ([#30](https://github.com/glennib/chum/pull/30))
- *(deps)* update dependency cargo:cargo-edit to v0.13.13 ([#29](https://github.com/glennib/chum/pull/29))

## [0.2.1](https://github.com/glennib/chum/compare/v0.2.0...v0.2.1) - 2026-07-27

### Other

- *(deps)* lock file maintenance ([#28](https://github.com/glennib/chum/pull/28))
- *(deps)* update dependency cargo-binstall to v1.21.1 ([#27](https://github.com/glennib/chum/pull/27))
- *(deps)* update rust crate tokio to v1.53.1 ([#26](https://github.com/glennib/chum/pull/26))
- *(deps)* update rust crate serde_json to v1.0.151 ([#25](https://github.com/glennib/chum/pull/25))
- *(deps)* update rust crate clap to v4.6.3 ([#24](https://github.com/glennib/chum/pull/24))
- *(deps)* update rust crate thiserror to v2.0.19 ([#23](https://github.com/glennib/chum/pull/23))
- *(deps)* update rust crate serde to v1.0.229 ([#22](https://github.com/glennib/chum/pull/22))
- *(deps)* update rust crate tokio to v1.53.0 ([#21](https://github.com/glennib/chum/pull/21))
- *(deps)* update rust crate tokio to v1.52.4 ([#20](https://github.com/glennib/chum/pull/20))
- *(deps)* update rust crate clap to v4.6.2 ([#19](https://github.com/glennib/chum/pull/19))
- *(deps)* update dependency cargo-binstall to v1.21.0 ([#18](https://github.com/glennib/chum/pull/18))

## [0.2.0](https://github.com/glennib/chum/compare/v0.1.1...v0.2.0) - 2026-07-01

### Added

- [**breaking**] stop reading connection config from CLICKHOUSE_* env vars
- [**breaking**] store migration bookkeeping in a dedicated _chum database

### Other

- drop clap-parse-only tests that restate library behavior
- drop env-mutating test, remove all unsafe from the crate
- *(deps)* update actions/checkout action to v7
- *(deps)* update actions/cache action to v6

## [0.1.1](https://github.com/glennib/chum/compare/v0.1.0...v0.1.1) - 2026-06-29

### Added

- *(cli)* give chum its own terminal color theme

### Other

- *(deps)* update clickhouse/clickhouse-server docker tag to v25.12

## [0.1.0](https://github.com/glennib/chum/releases/tag/v0.1.0) - 2026-06-29

### Added

- Initial release of `chum`, a general-purpose ClickHouse schema migration
  tool usable both as a Rust library and as a CLI.
- Client-side multi-statement splitting using `sqlparser`'s tokenizer to find
  true statement boundaries, supporting ClickHouse DDL (`CREATE DICTIONARY`,
  the `ENGINE … ORDER BY` tail, `AggregateFunction`, `JSON`) that a full AST
  parse cannot model.
- Migration sources gathered at compile time via `include_dir` or at runtime
  from a directory.
- Append-only bookkeeping table (`MergeTree`, auto-promoted to
  `ReplicatedMergeTree`) with per-migration SHA-384 checksums, drift detection,
  and a `dirty` marker for partial failures.
- Cross-replica consistency via a client-wide bundle of session settings
  (quorum + sequential consistency, synchronous lightweight deletes, no async
  insert).
- CLI commands: `run` (with `--target` / `--dry-run`), `info`, `revert` (with
  `--steps` / `--target` / `--dry-run` / `--yes`), `add` (timestamp or
  `--sequential` versioning), `convert` (between sequential and timestamp
  schemes, with `--dry-run`), and `force`.
- Connection configuration via flags or environment variables, accepting a bare
  HTTP endpoint or a full DSN.
- Output as a colored table, TSV (auto-selected when piped), or JSON.
- CLI error reporting with `miette`, backed by `code` + `help` diagnostics on
  `ChumError` under the opt-in `diagnostic` feature.
