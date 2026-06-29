//! Error type for the migrator.

/// Errors produced while resolving, validating, or applying migrations.
///
/// With the `diagnostic` feature (on by default via `cli`), this derives
/// [`miette::Diagnostic`], attaching a stable error `code` and an actionable
/// `help` line to every variant. The library build with no default features
/// stays free of `miette` — the derive and its attributes vanish entirely.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "diagnostic", derive(miette::Diagnostic))]
pub enum ChumError {
    /// A migration source (directory or embedded tree) could not be read or
    /// a filename did not match `<version>_<name>.{up,down}.sql`.
    #[error("migration source error: {0}")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::source),
            help(
                "Check that the migration directory exists and that every file is named \
                 `<version>_<name>.{{up,down}}.sql`."
            )
        )
    )]
    Source(String),

    /// The SQL of a migration could not be tokenized into statements.
    #[error("failed to split migration {version} into statements: {message}")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::split),
            help(
                "chum splits a file into statements with a ClickHouse-aware tokenizer; an \
                 unterminated string literal or block comment is the usual cause. Check this \
                 migration's SQL."
            )
        )
    )]
    Split {
        /// The version whose SQL failed to tokenize.
        version: i64,
        /// The underlying tokenizer message.
        message: String,
    },

    /// A migration that was previously applied is missing from the current
    /// source set (and `ignore_missing` is not enabled).
    #[error("migration {0} is applied to the database but missing from the source")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::version_missing),
            help(
                "Restore the deleted migration file to the source directory. If it is \
                 intentionally gone, reset the database (drop the schema and re-migrate from \
                 scratch) — or, for library callers, skip the check with \
                 `Migrator::ignore_missing`."
            )
        )
    )]
    VersionMissing(i64),

    /// A migration's checksum no longer matches the one recorded at apply
    /// time — its SQL was edited after being applied.
    #[error("migration {0} was altered after being applied (checksum mismatch)")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::checksum_mismatch),
            help(
                "A migration must never be edited after it has been applied. Restore its original \
                 SQL. If the change is intentional, reset the database (drop the schema and \
                 re-migrate)."
            )
        )
    )]
    ChecksumMismatch(i64),

    /// The database is dirty: a previous apply failed partway and the version
    /// must be resolved (e.g. with `chum force`) before continuing.
    #[error(
        "database is dirty at version {0}: a previous migration failed partway; resolve the \
         schema and run `chum force {0}`"
    )]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::dirty),
            help(
                "`chum force <version>` only updates chum's bookkeeping — it does not roll back \
                 the partial change. Inspect the schema and finish or undo it by hand before \
                 forcing."
            )
        )
    )]
    Dirty(i64),

    /// A ClickHouse query failed.
    #[error("clickhouse error: {0}")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::clickhouse),
            help(
                "The ClickHouse server rejected the query or was unreachable. Check the \
                 connection URL, credentials, and that the target database exists."
            )
        )
    )]
    ClickHouse(#[from] clickhouse::error::Error),

    /// An I/O error while reading migrations or scaffolding a new one.
    #[error("io error: {0}")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::io),
            help(
                "Check that the path is correct and that you have permission to read or write it."
            )
        )
    )]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, ChumError>;
