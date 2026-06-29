//! Error type for the migrator.

/// Errors produced while resolving, validating, or applying migrations.
#[derive(Debug, thiserror::Error)]
pub enum ChumError {
    /// A migration source (directory or embedded tree) could not be read or
    /// a filename did not match `<version>_<name>.{up,down}.sql`.
    #[error("migration source error: {0}")]
    Source(String),

    /// The SQL of a migration could not be tokenized into statements.
    #[error("failed to split migration {version} into statements: {message}")]
    Split {
        /// The version whose SQL failed to tokenize.
        version: i64,
        /// The underlying tokenizer message.
        message: String,
    },

    /// A migration that was previously applied is missing from the current
    /// source set (and `ignore_missing` is not enabled).
    #[error("migration {0} is applied to the database but missing from the source")]
    VersionMissing(i64),

    /// A migration's checksum no longer matches the one recorded at apply
    /// time — its SQL was edited after being applied.
    #[error("migration {0} was altered after being applied (checksum mismatch)")]
    ChecksumMismatch(i64),

    /// The database is dirty: a previous apply failed partway and the version
    /// must be resolved (e.g. with `chum force`) before continuing.
    #[error(
        "database is dirty at version {0}: a previous migration failed partway; resolve the \
         schema and run `chum force {0}`"
    )]
    Dirty(i64),

    /// A ClickHouse query failed.
    #[error("clickhouse error: {0}")]
    ClickHouse(#[from] clickhouse::error::Error),

    /// An I/O error while reading migrations or scaffolding a new one.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, ChumError>;
