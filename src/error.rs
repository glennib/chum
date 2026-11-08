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
///
/// A variant that wraps another error returns it from
/// [`source`](std::error::Error::source) and leaves it out of its own message,
/// so a reporter that walks the chain (miette, anyhow's `{:#}`) prints it once.
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
                 `<version>_<name>.{{up,down}}.sql`, or `.sql.j2` / `.j2.sql` for a template. \
                 A version can't have two files for one direction."
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

    /// The database is dirty because reverting a version failed partway: its
    /// down migration ran only in part.
    #[error(
        "database is dirty at version {0}: reverting it failed partway; finish the revert and \
         run `chum reset {0}`, or restore the version and run `chum force {0}`"
    )]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::dirty_revert),
            help(
                "Both commands only update chum's bookkeeping. `chum reset <version>` records the \
                 version as reverted, so the next `chum run` applies it again; `chum force \
                 <version>` records it as applied. Make the schema match before running either."
            )
        )
    )]
    DirtyRevert(i64),

    /// A statement of a migration failed. Statements before it ran and are not
    /// rolled back, so the version is left dirty.
    #[error(
        "migration {version} ({}) failed at statement {index} of {total}, `{}`",
        .direction.as_str(),
        excerpt(.statement)
    )]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::statement_failed),
            help("{}", statement_help(*version, *direction, *index))
        )
    )]
    StatementFailed {
        /// The version whose migration failed.
        version: i64,
        /// Whether it was being applied (`Up`) or reverted (`Down`).
        direction: Direction,
        /// The 1-based position of the failing statement.
        index: usize,
        /// How many statements the migration has.
        total: usize,
        /// The failing statement, as sent to ClickHouse.
        statement: String,
        /// The ClickHouse error.
        source: clickhouse::error::Error,
    },

    /// A ClickHouse query failed.
    #[error("ClickHouse request failed")]
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
    #[error("failed to upgrade the bookkeeping table")]
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
    #[error("I/O error")]
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
    #[error("{}", failures_message(&render_headline(.0), .0))]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(code(chum::render), help("{}", render_help(.0)))
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

    /// The source contains a `.sql.j2` or `.j2.sql` template, but the library was built
    /// without the `template` feature.
    #[error("{0} is a template, but chum was built without the `template` feature")]
    #[cfg_attr(
        feature = "diagnostic",
        diagnostic(
            code(chum::templating_disabled),
            help("Enable chum's `template` cargo feature to load template migrations.")
        )
    )]
    TemplatingDisabled(String),
}

/// Why one migration failed to render, as part of [`ChumError::Render`].
///
/// With the `diagnostic` feature it is a [`miette::Diagnostic`] whose source
/// code is the template, labelled at [`RenderFailure::location`], so a
/// reporter can show the failing line.
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
    /// Whether the values or the template are at fault.
    pub kind: RenderFailureKind,
    /// The template and the part of it that failed, when the engine reports
    /// one.
    pub location: Option<Box<TemplateLocation>>,
}

/// What a [`RenderFailure`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RenderFailureKind {
    /// A variable the template needs has no value.
    MissingValue,
    /// A filter rejected a supplied value, e.g. `sql_datetime` given a time
    /// without an offset.
    InvalidValue,
    /// The template itself is wrong, whatever the values: a syntax error, an
    /// unknown filter or test, or a filter called with the wrong arguments.
    Template,
}

/// A span of a template's text.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TemplateLocation {
    /// The template's file name.
    pub file_name: String,
    /// The whole template text.
    pub text: String,
    /// The byte range within [`TemplateLocation::text`].
    pub span: std::ops::Range<usize>,
}

impl TemplateLocation {
    /// Build a location.
    #[must_use]
    pub fn new(file_name: String, text: String, span: std::ops::Range<usize>) -> Self {
        Self {
            file_name,
            text,
            span,
        }
    }
}

impl RenderFailure {
    /// Build a failure record: a [`RenderFailureKind::MissingValue`] when
    /// `missing` names anything, else a [`RenderFailureKind::Template`], with
    /// no location.
    #[must_use]
    pub fn new(version: i64, direction: Direction, message: String, missing: Vec<String>) -> Self {
        let kind = if missing.is_empty() {
            RenderFailureKind::Template
        } else {
            RenderFailureKind::MissingValue
        };
        Self {
            version,
            direction,
            message,
            missing,
            kind,
            location: None,
        }
    }

    /// Set what the failure is about.
    #[must_use]
    pub fn with_kind(mut self, kind: RenderFailureKind) -> Self {
        self.kind = kind;
        self
    }

    /// Set where in the template it failed.
    #[must_use]
    pub fn with_location(mut self, location: Option<TemplateLocation>) -> Self {
        self.location = location.map(Box::new);
        self
    }
}

impl std::error::Error for RenderFailure {}

#[cfg(feature = "diagnostic")]
impl miette::Diagnostic for RenderFailure {
    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.location
            .as_deref()
            .map(|l| l as &dyn miette::SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        let location = self.location.as_ref()?;
        Some(Box::new(std::iter::once(miette::LabeledSpan::underline(
            location.span.clone(),
        ))))
    }
}

#[cfg(feature = "diagnostic")]
impl miette::SourceCode for TemplateLocation {
    fn read_span<'a>(
        &'a self,
        span: &miette::SourceSpan,
        context_lines_before: usize,
        context_lines_after: usize,
    ) -> std::result::Result<Box<dyn miette::SpanContents<'a> + 'a>, miette::MietteError> {
        let inner = self
            .text
            .read_span(span, context_lines_before, context_lines_after)?;
        Ok(Box::new(miette::MietteSpanContents::new_named(
            self.file_name.clone(),
            inner.data(),
            *inner.span(),
            inner.line(),
            inner.column(),
            inner.line_count(),
        )))
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

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Rendering stops at a template's first error, so there is one failure per
/// migration.
fn render_headline(failures: &[RenderFailure]) -> String {
    format!("failed to render {}", plural(failures.len(), "migration"))
}

fn failures_message(headline: &str, failures: &[RenderFailure]) -> String {
    let mut out = format!("{headline}:");
    for failure in failures {
        out.push_str("\n  - ");
        out.push_str(&failure.to_string());
    }
    out
}

/// One sentence per kind of failure present, in a fixed order.
#[cfg_attr(
    not(feature = "diagnostic"),
    expect(dead_code, reason = "only the diagnostic help uses it")
)]
fn render_help(failures: &[RenderFailure]) -> String {
    let has = |kind| failures.iter().any(|f| f.kind == kind);
    let mut sentences = Vec::new();
    if has(RenderFailureKind::MissingValue) {
        sentences.push(
            "Supply each missing value with `--var <key>=<value>` or `CHUM_VAR_<key>=<value>`: a \
             plain key (`tier`) for `env.tier`, a version-prefixed key (`<version>_cutoff`) for \
             `this.cutoff`.",
        );
    }
    if has(RenderFailureKind::InvalidValue) {
        sentences.push(
            "Correct each value a filter rejected, where it was supplied (`--var` or \
             `CHUM_VAR_*`).",
        );
    }
    if has(RenderFailureKind::Template) {
        sentences.push("Fix the template file: the mistake doesn't depend on the supplied values.");
    }
    sentences
        .push("`chum render` shows what a migration renders to without connecting to ClickHouse.");
    sentences.join(" ")
}

/// The first line of `statement` that isn't blank or a `--` comment, cut to a
/// length that fits in an error message.
fn excerpt(statement: &str) -> String {
    const MAX_CHARS: usize = 60;
    let line = statement
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or_default();
    match line.char_indices().nth(MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

#[cfg_attr(
    not(feature = "diagnostic"),
    expect(dead_code, reason = "only the diagnostic help uses it")
)]
fn statement_help(version: i64, direction: Direction, index: usize) -> String {
    let ran = match index {
        1 => String::new(),
        2 => "statement 1, which ran, or ".to_owned(),
        n => format!("statements 1 to {}, which ran, or ", n - 1),
    };
    let resolve = match direction {
        Direction::Up => format!(
            "finish the migration by hand and run `chum force {version}`, or undo what it did and \
             run `chum reset {version}` to apply it again"
        ),
        Direction::Down => format!(
            "finish the revert by hand and run `chum reset {version}`, or restore what it removed \
             and run `chum force {version}`"
        ),
    };
    format!(
        "ClickHouse doesn't roll back {ran}what the failing statement did, so version {version} \
         is now dirty. Fix the cause, then either {resolve}."
    )
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, ChumError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "diagnostic")]
    #[test]
    fn render_help_covers_only_the_kinds_present() {
        let failure =
            |kind| RenderFailure::new(1, Direction::Up, String::new(), Vec::new()).with_kind(kind);
        let invalid = render_help(&[failure(RenderFailureKind::InvalidValue)]);
        assert!(invalid.starts_with("Correct each value a filter rejected"));
        assert!(!invalid.contains("missing") && !invalid.contains("template file"));

        let both = render_help(&[
            failure(RenderFailureKind::Template),
            failure(RenderFailureKind::MissingValue),
        ]);
        assert!(both.starts_with("Supply each missing value"));
        assert!(both.contains("Fix the template file"));
    }

    #[test]
    fn excerpt_skips_comments_and_truncates_long_lines() {
        assert_eq!(
            excerpt("-- add t\n\n  CREATE TABLE t\n(a UInt8)"),
            "CREATE TABLE t"
        );
        let long = format!("SELECT '{}'", "é".repeat(80));
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), 61, "{cut}");
        assert!(cut.ends_with('…'), "{cut}");
    }

    #[test]
    fn statement_failed_names_the_statement_and_its_position() {
        let err = ChumError::StatementFailed {
            version: 7,
            direction: Direction::Down,
            index: 2,
            total: 3,
            statement: "-- c\nDROP TABLE app.t\nSYNC".to_owned(),
            source: clickhouse::error::Error::Custom("boom".to_owned()),
        };
        assert_eq!(
            err.to_string(),
            "migration 7 (down) failed at statement 2 of 3, `DROP TABLE app.t`"
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn statement_help_names_what_ran_and_how_to_resolve() {
        let first = statement_help(7, Direction::Up, 1);
        assert!(
            first.starts_with("ClickHouse doesn't roll back what the failing"),
            "{first}"
        );
        let third = statement_help(7, Direction::Up, 3);
        assert!(third.contains("statements 1 to 2, which ran"), "{third}");
        assert!(third.contains("`chum force 7`"), "{third}");
        let down = statement_help(7, Direction::Down, 2);
        assert!(down.contains("statement 1, which ran"), "{down}");
        assert!(
            down.contains("finish the revert by hand and run `chum reset 7`"),
            "{down}"
        );
    }
}
