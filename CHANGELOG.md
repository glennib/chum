# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
