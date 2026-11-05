//! Error type for the migrator.

use std::fmt;

use crate::migration::Direction;
use crate::migrator::State;

/// Errors produced while resolving, validating, or applying migrations.
///
/// With the `diagnostic` feature (on by default via `cli`), this derives
/// [`miette::Diagnostic`], attaching a stable error `code` and an actionable
/// `help` line to every variant. The library build with no default features
/// stays free of `miette` — the derive and its attributes vanish entirely.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "diagnostic", derive(miette::Diagnostic))]
#[non_exhaustive]
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
                 `<version>_<name>.{{up,down}}.sql`, or `.sql.j2` for a template. A version \
                 can't have both a plain and a template file for one direction."
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
                 forcing. If the failed attempt changed nothing (e.g. a guard rejected its \
                 inputs), `chum reset <version>` makes the version pending again."
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

    /// Adding the `vars` column to an existing bookkeeping table failed,
    /// usually because chum's user lacks `ALTER` on it.
    #[error("failed to upgrade the bookkeeping table: {source}")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::bookkeeping_upgrade),
            help(
                "chum adds this column to bookkeeping tables created by older versions. Grant \
                 chum's user ALTER on the bookkeeping table, or have an administrator run: \
                 {statement}"
            )
        )
    )]
    BookkeepingUpgrade {
        /// The `ALTER TABLE` statement that was refused.
        statement: String,
        /// The ClickHouse error.
        source: clickhouse::error::Error,
    },

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

    /// A `--var` or `CHUM_VAR_*` key could not be turned into a variable.
    #[error("invalid variable {key}: {reason}")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::invalid_var_key),
            help(
                "A plain key (`tier`) sets `env.tier` for every migration; a version-prefixed \
                 key (`20261106120000_cutoff`) sets `this.cutoff` for that version only. Names \
                 match `[A-Za-z_][A-Za-z0-9_]*`, and each key may be given once per source."
            )
        )
    )]
    InvalidVarKey {
        /// The offending key as it was supplied, e.g. `--var 0005_` or
        /// `CHUM_VAR_0005_`.
        key: String,
        /// Why it was rejected.
        reason: String,
    },

    /// One or more templated migrations failed to render. Nothing was
    /// executed.
    #[error("{}", render_message(.0))]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::render),
            help(
                "Supply each missing value with `--var <key>=<value>` or \
                 `CHUM_VAR_<key>=<value>`: a plain key (`tier`) for `env.tier`, a version-prefixed \
                 key (`<version>_cutoff`) for `this.cutoff`. `chum render` shows what a \
                 migration renders to without connecting to ClickHouse."
            )
        )
    )]
    Render(Vec<RenderFailure>),

    /// A supplied `this.*` value differs from the one recorded when the
    /// version was applied. In `revert` and `force` the recorded value
    /// describes what actually ran, so it wins, and a different supplied
    /// value is a mistake rather than an override.
    #[error(
        "{key} supplies {name} = {supplied:?} for version {version}, but it was applied with \
         {recorded:?}"
    )]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::var_conflict),
            help(
                "Recorded `this.*` values win when reverting or forcing a version. Drop the \
                 supplied value, or pass the recorded one."
            )
        )
    )]
    VarConflict {
        /// The version being reverted or forced.
        version: i64,
        /// The namespaced name, e.g. `this.cutoff`.
        name: String,
        /// Where the conflicting value was supplied, e.g.
        /// `--var 20261106120000_cutoff`.
        key: String,
        /// The supplied value.
        supplied: String,
        /// The recorded value.
        recorded: String,
    },

    /// `reset` was asked to clear a version that isn't dirty.
    #[error("version {version} is {}, not dirty", .state.as_str())]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::not_dirty),
            help(
                "`chum reset` only clears a failed attempt. Revert an applied version with `chum \
                 revert`; a pending version has nothing to reset."
            )
        )
    )]
    NotDirty {
        /// The version.
        version: i64,
        /// Its actual state.
        state: State,
    },

    /// The source contains a `.sql.j2` template, but the library was built
    /// without the `template` feature.
    #[error("{0} is a template, but chum was built without the `template` feature")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::templating_disabled),
            help("Enable chum's `template` cargo feature to load `.sql.j2` migrations.")
        )
    )]
    TemplatingDisabled(String),
}

/// Why one migration failed to render, as part of [`ChumError::Render`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RenderFailure {
    /// The version whose template failed.
    pub version: i64,
    /// Which of its files failed.
    pub direction: Direction,
    /// The template engine's message, naming the file and line.
    pub message: String,
    /// The namespaced names (`env.tier`) the failing line looked up that had
    /// no value.
    pub missing: Vec<String>,
}

impl RenderFailure {
    /// Build a failure record.
    #[must_use]
    pub fn new(version: i64, direction: Direction, message: String, missing: Vec<String>) -> Self {
        Self {
            version,
            direction,
            message,
            missing,
        }
    }
}

impl fmt::Display for RenderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.version,
            self.direction.as_str(),
            self.message
        )?;
        if !self.missing.is_empty() {
            let names: Vec<String> = self.missing.iter().map(|m| format!("`{m}`")).collect();
            write!(f, "; no value was supplied for {}", names.join(", "))?;
        }
        Ok(())
    }
}

fn render_message(failures: &[RenderFailure]) -> String {
    let mut out = match failures.len() {
        1 => "failed to render 1 migration:".to_owned(),
        n => format!("failed to render {n} migrations:"),
    };
    for failure in failures {
        out.push_str("\n  - ");
        out.push_str(&failure.to_string());
    }
    out
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, ChumError>;
