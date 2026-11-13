# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0](https://github.com/glennib/chum/compare/v0.5.0...v0.6.0) - 2026-11-13

### Added

- *(bookkeeping)* declare the bookkeeping format in the table comment
- *(bookkeeping)* refuse a bookkeeping table with missing or mistyped columns
- *(cli)* add `--missing-this unset` for databases built from scratch
- *(bookkeeping)* [**breaking**] record the `this.*` names a version was unset for
- *(vars)* [**breaking**] require every `this.*` name to be supplied or unset

### Fixed

- *(bookkeeping)* read names an older chum didn't record as unset
- *(deps)* update rust crate sqlparser to 0.64 ([#110](https://github.com/glennib/chum/pull/110))

### Other

- *(bookkeeping)* derive the table's DDL, upgrade and reads from one column list
- *(claude)* sync CLAUDE.md with templating and bookkeeping upgrades
- *(deps)* update dependency cargo-binstall to v1.27.0 ([#111](https://github.com/glennib/chum/pull/111))

## [0.5.0](https://github.com/glennib/chum/compare/v0.4.1...v0.5.0) - 2026-11-08

### Fixed

- *(diagnostic)* suggest `chum reset` before `chum force` for a dirty apply
- *(cli)* [**breaking**] offer each flag only on the subcommands that use it

## [0.4.1](https://github.com/glennib/chum/compare/v0.4.0...v0.4.1) - 2026-11-08

### Added

- check every branch of every template before rendering, and add `chum check`
- say whether a render failure is the template's or a value's fault
- *(template)* add sql_date, sql_datetime and sql_datetime64 filters

### Fixed

- *(error)* mention invalid values in the render error's help

### Other

- document --strict-consistency
- use underscore names for clippy lint overrides

## [0.4.0](https://github.com/glennib/chum/compare/v0.3.0...v0.4.0) - 2026-11-06

### Added

- *(add)* continue the directory's template spelling
- [**breaking**] accept `.j2.sql` as a template extension

## [0.3.0](https://github.com/glennib/chum/compare/v0.2.5...v0.3.0) - 2026-11-05

### Added

- name the failing statement when a migration fails
- expose templated migrations in the CLI
- [**breaking**] render .sql.j2 migrations with minijinja
- [**breaking**] add a vars column to the bookkeeping table
- [**breaking**] mark public data and error types non_exhaustive

### Fixed

- print each wrapped error once in reports
- mark a version dirty while it is being reverted

### Other

- drop the templated-migrations design document
- document templated migrations in the README
- propose templated migrations

## [0.2.5](https://github.com/glennib/chum/compare/v0.2.4...v0.2.5) - 2026-11-05

### Other

- bump dist to 0.34.0 so aarch64 macOS builds get a runner

## [0.2.4](https://github.com/glennib/chum/compare/v0.2.3...v0.2.4) - 2026-11-05

### Fixed

- send migration SQL to ClickHouse verbatim

### Other

- *(deps)* update dependency cargo:cargo-dist to v0.34.0 ([#101](https://github.com/glennib/chum/pull/101))
- format with stable rustfmt
- *(deps)* update dependency cargo:release-plz to v0.4.3 ([#100](https://github.com/glennib/chum/pull/100))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.151 ([#99](https://github.com/glennib/chum/pull/99))
- *(deps)* update docker.io/clickhouse/clickhouse-server docker tag to v26
- *(deps)* update dependency cargo-binstall to v1.26.2 ([#98](https://github.com/glennib/chum/pull/98))
- *(deps)* update dependency cargo:release-plz to v0.4.2 ([#97](https://github.com/glennib/chum/pull/97))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.150 ([#96](https://github.com/glennib/chum/pull/96))
- *(deps)* lock file maintenance ([#95](https://github.com/glennib/chum/pull/95))
- *(deps)* update dependency cargo:release-plz to v0.4.1 ([#94](https://github.com/glennib/chum/pull/94))
- *(deps)* update dependency cargo-binstall to v1.26.1 ([#93](https://github.com/glennib/chum/pull/93))
- *(deps)* update dependency cargo:release-plz to v0.4.0 ([#92](https://github.com/glennib/chum/pull/92))
- *(deps)* update dependency cargo:cargo-edit to v0.13.14 ([#90](https://github.com/glennib/chum/pull/90))
- *(deps)* update rust crate tokio to v1.54.1 ([#91](https://github.com/glennib/chum/pull/91))
- *(deps)* update dependency cargo:release-plz to v0.3.174 ([#89](https://github.com/glennib/chum/pull/89))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.149 ([#88](https://github.com/glennib/chum/pull/88))
- *(deps)* update dependency cargo:release-plz to v0.3.173 ([#86](https://github.com/glennib/chum/pull/86))
- *(deps)* update rust crate tokio to v1.54.0 ([#87](https://github.com/glennib/chum/pull/87))
- *(deps)* lock file maintenance ([#85](https://github.com/glennib/chum/pull/85))
- *(deps)* update dependency cargo-binstall to v1.26.0 ([#84](https://github.com/glennib/chum/pull/84))
- *(deps)* update rust crate tokio to v1.53.3 ([#83](https://github.com/glennib/chum/pull/83))
- *(deps)* update rust crate clickhouse to v0.15.3 ([#82](https://github.com/glennib/chum/pull/82))
- *(deps)* update rust crate sha2 to v0.11.1 ([#81](https://github.com/glennib/chum/pull/81))
- *(deps)* update dependency cargo:release-plz to v0.3.172 ([#80](https://github.com/glennib/chum/pull/80))
- *(deps)* lock file maintenance ([#79](https://github.com/glennib/chum/pull/79))
- *(deps)* update rust crate serde_json to v1.0.154 ([#78](https://github.com/glennib/chum/pull/78))
- *(deps)* update rust crate serde_json to v1.0.152 ([#77](https://github.com/glennib/chum/pull/77))
- *(deps)* update dependency cargo-binstall to v1.25.3 ([#76](https://github.com/glennib/chum/pull/76))
- *(deps)* update dependency cargo:release-plz to v0.3.171 ([#75](https://github.com/glennib/chum/pull/75))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.148 ([#74](https://github.com/glennib/chum/pull/74))
- *(deps)* update dependency cargo:release-plz to v0.3.170 ([#73](https://github.com/glennib/chum/pull/73))
- *(deps)* update dependency cargo-binstall to v1.25.2 ([#72](https://github.com/glennib/chum/pull/72))
- *(deps)* lock file maintenance ([#71](https://github.com/glennib/chum/pull/71))
- *(deps)* update rust crate tokio to v1.53.2 ([#70](https://github.com/glennib/chum/pull/70))
- *(deps)* update dependency cargo-binstall to v1.25.1 ([#69](https://github.com/glennib/chum/pull/69))
- *(deps)* update dependency cargo-binstall to v1.25.0 ([#68](https://github.com/glennib/chum/pull/68))
- *(deps)* lock file maintenance ([#67](https://github.com/glennib/chum/pull/67))
- *(deps)* update dependency cargo-binstall to v1.24.0 ([#66](https://github.com/glennib/chum/pull/66))
- *(deps)* update rust crate thiserror to v2.0.21 ([#65](https://github.com/glennib/chum/pull/65))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.146 ([#64](https://github.com/glennib/chum/pull/64))
- *(deps)* lock file maintenance ([#63](https://github.com/glennib/chum/pull/63))
- *(deps)* update dependency cargo:release-plz to v0.3.169 ([#61](https://github.com/glennib/chum/pull/61))

## [0.2.3](https://github.com/glennib/chum/compare/v0.2.2...v0.2.3) - 2026-09-17

### Fixed

- *(deps)* update rust crate sqlparser to 0.63 ([#56](https://github.com/glennib/chum/pull/56))
- *(deps)* update rust crate tabled to 0.22 ([#46](https://github.com/glennib/chum/pull/46))

### Other

- *(deps)* update dependency cargo:release-plz to v0.3.168 ([#60](https://github.com/glennib/chum/pull/60))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.145 ([#59](https://github.com/glennib/chum/pull/59))
- *(deps)* update rust crate clap to v4.6.7 ([#58](https://github.com/glennib/chum/pull/58))
- *(deps)* lock file maintenance ([#57](https://github.com/glennib/chum/pull/57))
- *(deps)* update dependency cargo:release-plz to v0.3.167 ([#55](https://github.com/glennib/chum/pull/55))
- *(deps)* update dependency cargo:cargo-nextest to v0.9.144 ([#54](https://github.com/glennib/chum/pull/54))
- *(deps)* update dependency cargo:release-plz to v0.3.165 ([#53](https://github.com/glennib/chum/pull/53))
- *(deps)* update rust crate console to v0.16.6 ([#52](https://github.com/glennib/chum/pull/52))
- *(deps)* update dependency cargo:release-plz to v0.3.164 ([#51](https://github.com/glennib/chum/pull/51))
- *(deps)* update dependency cargo:release-plz to v0.3.163 ([#50](https://github.com/glennib/chum/pull/50))
- *(deps)* lock file maintenance ([#49](https://github.com/glennib/chum/pull/49))
- *(deps)* update dependency cargo:release-plz to v0.3.162 ([#47](https://github.com/glennib/chum/pull/47))
- *(deps)* update dependency cargo-binstall to v1.23.0 ([#48](https://github.com/glennib/chum/pull/48))
- *(deps)* update dependency cargo:release-plz to v0.3.161 ([#45](https://github.com/glennib/chum/pull/45))
- *(deps)* lock file maintenance ([#44](https://github.com/glennib/chum/pull/44))
- *(deps)* update rust crate clickhouse to v0.15.2 ([#43](https://github.com/glennib/chum/pull/43))
- *(deps)* lock file maintenance ([#42](https://github.com/glennib/chum/pull/42))
- *(deps)* update dependency cargo-binstall to v1.22.0 ([#41](https://github.com/glennib/chum/pull/41))

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
