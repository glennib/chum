//! The compile-time / source representation of a migration, and the runtime
//! representation of a migration that has already been applied to a database.

use std::borrow::Cow;

use sha2::Digest;
use sha2::Sha384;

/// The direction of a migration file: `*.up.sql` or `*.down.sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A forward migration (`<version>_<name>.up.sql`).
    Up,
    /// A reverse migration (`<version>_<name>.down.sql`).
    Down,
}

impl Direction {
    /// The filename suffix this direction is recorded with.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Direction::Up => ".up.sql",
            Direction::Down => ".down.sql",
        }
    }
}

/// A migration resolved from a source (a directory at runtime, or embedded
/// bytes at compile time).
///
/// The fields use [`Cow`] so the same type can be produced both by runtime
/// parsing (yielding owned data) and, in a future iteration, by a
/// `migrate!` proc-macro emitting `&'static` const data.
#[derive(Debug, Clone)]
pub struct Migration {
    /// The numeric version parsed from the filename prefix.
    pub version: i64,
    /// Human-readable description (the filename minus version and suffix,
    /// with `_` turned into spaces).
    pub description: Cow<'static, str>,
    /// Whether this is an up or down migration.
    pub direction: Direction,
    /// The raw SQL contents of the migration file.
    pub sql: Cow<'static, str>,
    /// Hex-encoded SHA-384 digest of [`Migration::sql`], used to detect
    /// changes to a migration that has already been applied. Computed over a
    /// lightly normalized form of the SQL (see [`checksum`]) so that line
    /// endings and trailing whitespace don't trip false drift.
    pub checksum: Cow<'static, str>,
}

impl Migration {
    /// Construct a migration, computing its checksum from the SQL.
    #[must_use]
    pub fn new(
        version: i64,
        description: impl Into<Cow<'static, str>>,
        direction: Direction,
        sql: impl Into<Cow<'static, str>>,
    ) -> Self {
        let sql = sql.into();
        let checksum = Cow::Owned(checksum(&sql));
        Self {
            version,
            description: description.into(),
            direction,
            sql,
            checksum,
        }
    }
}

/// A migration that has been recorded in the database bookkeeping table.
///
/// Reflects the latest marker recorded for the version (see
/// [`crate::backend`]).
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    /// The version that was applied.
    pub version: i64,
    /// The description recorded at apply time.
    pub description: String,
    /// Hex-encoded SHA-384 checksum recorded at apply time.
    pub checksum: String,
    /// Wall-clock time the migration's statements took to run, in
    /// milliseconds. Zero for a dirty (failed / in-progress) version.
    pub execution_ms: u64,
    /// When the latest marker for this version was recorded.
    pub applied_at: chrono::DateTime<chrono::Utc>,
    /// Whether the migration completed successfully. A `false` value marks
    /// the version as *dirty* (a partial / failed apply).
    pub success: bool,
}

/// Compute the hex-encoded SHA-384 checksum of a migration's SQL.
///
/// The SQL is first [normalized](normalize): line endings are unified to `\n`
/// and trailing whitespace is stripped. This keeps the integrity check strict
/// about content while ignoring the cosmetic differences (CRLF vs LF, a
/// stray trailing newline, editor whitespace) that would otherwise report
/// spurious drift on an already-applied migration. The normalization is
/// deliberately parser-independent so the checksum stays stable across
/// dependency upgrades.
#[must_use]
pub fn checksum(sql: &str) -> String {
    let digest = Sha384::digest(normalize(sql).as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Normalize SQL for checksumming: unify line endings to `\n`, strip trailing
/// whitespace from each line, and drop trailing blank lines.
fn normalize(sql: &str) -> String {
    let unified = sql.replace("\r\n", "\n").replace('\r', "\n");
    let mut normalized = unified
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    normalized.truncate(normalized.trim_end().len());
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_do_not_affect_checksum() {
        assert_eq!(
            checksum("CREATE TABLE t (a UInt8)\nENGINE = Memory"),
            checksum("CREATE TABLE t (a UInt8)\r\nENGINE = Memory"),
        );
    }

    #[test]
    fn trailing_whitespace_and_newline_do_not_affect_checksum() {
        assert_eq!(
            checksum("CREATE TABLE t (a UInt8) ENGINE = Memory"),
            checksum("CREATE TABLE t (a UInt8) ENGINE = Memory   \n\n"),
        );
    }

    #[test]
    fn content_changes_do_affect_checksum() {
        assert_ne!(
            checksum("CREATE TABLE t (a UInt8) ENGINE = Memory"),
            checksum("CREATE TABLE t (a UInt16) ENGINE = Memory"),
        );
    }

    #[test]
    fn interior_whitespace_is_significant() {
        // Only *trailing* whitespace is stripped; interior spacing is content.
        assert_ne!(
            checksum("CREATE TABLE t (a UInt8)"),
            checksum("CREATE TABLE t (a  UInt8)"),
        );
    }
}
