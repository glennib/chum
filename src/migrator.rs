//! The migrator: validates the recorded state against the source set and
//! applies, reverts, or reports migrations.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use crate::backend;
use crate::error::ChumError;
use crate::error::Result;
use crate::migration::AppliedMigration;
use crate::migration::Direction;
use crate::migration::Migration;
use crate::split::split_statements;

/// A migration that was applied during a [`Migrator::run`] call.
#[derive(Debug)]
pub struct Applied {
    /// The version that was applied.
    pub version: i64,
    /// The migration's description.
    pub description: String,
    /// How long the migration's statements took to execute.
    pub elapsed: Duration,
}

/// The state of a single version, as reported by [`Migrator::info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Recorded as successfully applied.
    Applied,
    /// Present in the source but not yet applied.
    Pending,
    /// A previous apply failed partway; the version is dirty.
    Dirty,
}

/// A line of [`Migrator::info`] output.
#[derive(Debug)]
pub struct Status {
    /// The version.
    pub version: i64,
    /// The description (from the source, falling back to the recorded one).
    pub description: String,
    /// The version's state.
    pub state: State,
    /// When the version was applied, if it is recorded as applied. `None` for
    /// pending (and for dirty) versions.
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
    /// How long the migration's statements took, in milliseconds, if it is
    /// recorded as applied. `None` for pending (and dirty) versions.
    pub execution_ms: Option<u64>,
}

/// A migration that a [`Migrator::run`] would apply, as reported by
/// [`Migrator::plan`] without applying anything.
#[derive(Debug)]
pub struct Planned {
    /// The version that would be applied.
    pub version: i64,
    /// The migration's description.
    pub description: String,
}

/// A progress event emitted during [`Migrator::run_with`] /
/// [`Migrator::undo_with`], letting a caller report per-migration progress
/// (e.g. a CLI spinner). The plain [`Migrator::run`] / [`Migrator::undo`]
/// wrappers pass a no-op callback.
#[derive(Debug)]
pub enum Progress<'a> {
    /// A migration is about to be applied.
    ApplyStarted {
        /// The version being applied.
        version: i64,
        /// The migration's description.
        description: &'a str,
    },
    /// A migration finished applying.
    ApplyFinished {
        /// The version that was applied.
        version: i64,
        /// The migration's description.
        description: &'a str,
        /// How long its statements took.
        elapsed: Duration,
    },
    /// A version is about to be reverted.
    RevertStarted {
        /// The version being reverted.
        version: i64,
    },
    /// A version finished reverting.
    RevertFinished {
        /// The version that was reverted.
        version: i64,
    },
}

/// A resolved set of migrations ready to be applied to a database.
///
/// Holds [`Cow`]-borrowed migrations so it can be built from runtime-parsed
/// data (owned) or, in a future iteration, from `&'static` const data emitted
/// by a `migrate!` proc-macro (borrowed) — without changing this type.
#[derive(Debug)]
pub struct Migrator {
    migrations: Cow<'static, [Migration]>,
    ignore_missing: bool,
}

impl Migrator {
    /// Build a migrator from a resolved set of migrations.
    #[must_use]
    pub fn new(migrations: impl Into<Cow<'static, [Migration]>>) -> Self {
        Self {
            migrations: migrations.into(),
            ignore_missing: false,
        }
    }

    /// If set, applied migrations that are absent from the source set do not
    /// cause an error.
    #[must_use]
    pub fn ignore_missing(mut self, ignore: bool) -> Self {
        self.ignore_missing = ignore;
        self
    }

    /// Iterator over the up migrations, in ascending version order.
    fn ups(&self) -> impl Iterator<Item = &Migration> {
        self.migrations
            .iter()
            .filter(|m| m.direction == Direction::Up)
    }

    /// The down migration for a version, if present.
    fn down(&self, version: i64) -> Option<&Migration> {
        self.migrations
            .iter()
            .find(|m| m.version == version && m.direction == Direction::Down)
    }

    /// The up migration for a version, if present.
    fn up(&self, version: i64) -> Option<&Migration> {
        self.ups().find(|m| m.version == version)
    }

    /// Apply all pending migrations, optionally stopping at `target`.
    ///
    /// Validates first: a dirty database, a checksum drift on an
    /// already-applied migration, or (unless [`Migrator::ignore_missing`]) an
    /// applied version absent from the source all abort the run before any
    /// new migration is applied.
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn run(
        &self,
        client: &clickhouse::Client,
        table: &str,
        target: Option<i64>,
    ) -> Result<Vec<Applied>> {
        self.run_with(client, table, target, |_| {}).await
    }

    /// Like [`Migrator::run`], but reports progress through `on_progress`
    /// ([`Progress::ApplyStarted`] before each migration,
    /// [`Progress::ApplyFinished`] after). Used to drive a CLI spinner;
    /// [`Migrator::run`] is the no-op wrapper.
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn run_with<F>(
        &self,
        client: &clickhouse::Client,
        table: &str,
        target: Option<i64>,
        mut on_progress: F,
    ) -> Result<Vec<Applied>>
    where
        F: FnMut(Progress<'_>),
    {
        backend::ensure_table(client, table).await?;
        let applied = self.validated_applied(client, table).await?;

        let mut report = Vec::new();
        for migration in self.ups() {
            if target.is_some_and(|t| migration.version > t) {
                break;
            }
            if let Some(rec) = applied.get(&migration.version) {
                if rec.checksum != migration.checksum.as_ref() {
                    return Err(ChumError::ChecksumMismatch(migration.version));
                }
            } else {
                on_progress(Progress::ApplyStarted {
                    version: migration.version,
                    description: migration.description.as_ref(),
                });
                let elapsed = self.apply(client, table, migration).await?;
                on_progress(Progress::ApplyFinished {
                    version: migration.version,
                    description: migration.description.as_ref(),
                    elapsed,
                });
                report.push(Applied {
                    version: migration.version,
                    description: migration.description.to_string(),
                    elapsed,
                });
            }
        }
        Ok(report)
    }

    /// Compute the migrations a [`Migrator::run`] would apply, without applying
    /// any. Performs the *same* validation as `run` — a dirty database, a
    /// checksum drift, or (unless [`Migrator::ignore_missing`]) a missing
    /// applied version aborts with the same error — so a dry-run reflects what
    /// the real run would do rather than an optimistic guess.
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn plan(
        &self,
        client: &clickhouse::Client,
        table: &str,
        target: Option<i64>,
    ) -> Result<Vec<Planned>> {
        backend::ensure_table(client, table).await?;
        let applied = self.validated_applied(client, table).await?;

        let mut planned = Vec::new();
        for migration in self.ups() {
            if target.is_some_and(|t| migration.version > t) {
                break;
            }
            if let Some(rec) = applied.get(&migration.version) {
                if rec.checksum != migration.checksum.as_ref() {
                    return Err(ChumError::ChecksumMismatch(migration.version));
                }
            } else {
                planned.push(Planned {
                    version: migration.version,
                    description: migration.description.to_string(),
                });
            }
        }
        Ok(planned)
    }

    /// Revert applied migrations whose version is greater than `target`, in
    /// descending order.
    ///
    /// # Errors
    ///
    /// See [`ChumError`]. Also errors if a version to revert has no
    /// corresponding `down` migration in the source.
    pub async fn undo(
        &self,
        client: &clickhouse::Client,
        table: &str,
        target: i64,
    ) -> Result<Vec<i64>> {
        self.undo_with(client, table, target, |_| {}).await
    }

    /// Like [`Migrator::undo`], but reports progress through `on_progress`
    /// ([`Progress::RevertStarted`] before each revert,
    /// [`Progress::RevertFinished`] after). [`Migrator::undo`] is the no-op
    /// wrapper.
    ///
    /// # Errors
    ///
    /// See [`ChumError`]. Also errors if a version to revert has no
    /// corresponding `down` migration in the source.
    pub async fn undo_with<F>(
        &self,
        client: &clickhouse::Client,
        table: &str,
        target: i64,
        mut on_progress: F,
    ) -> Result<Vec<i64>>
    where
        F: FnMut(Progress<'_>),
    {
        backend::ensure_table(client, table).await?;
        let applied = self.validated_applied(client, table).await?;

        let mut versions: Vec<i64> = applied
            .values()
            .filter(|m| m.success && m.version > target)
            .map(|m| m.version)
            .collect();
        versions.sort_unstable();
        versions.reverse();

        let mut reverted = Vec::new();
        for version in versions {
            let down = self.down(version).ok_or_else(|| {
                ChumError::Source(format!("no down migration for applied version {version}"))
            })?;
            let statements = split_statements(&down.sql)
                .map_err(|message| ChumError::Split { version, message })?;
            on_progress(Progress::RevertStarted { version });
            for stmt in &statements {
                client.query(stmt).execute().await?;
            }
            backend::delete_version(client, table, version).await?;
            on_progress(Progress::RevertFinished { version });
            reverted.push(version);
        }
        Ok(reverted)
    }

    /// Compute the versions a [`Migrator::undo`] would revert (descending),
    /// without reverting any. Validates the same conditions (dirty / missing)
    /// and that each version has a `down` migration in the source, so a revert
    /// that would fail surfaces the error here rather than after a
    /// confirmation.
    ///
    /// # Errors
    ///
    /// See [`ChumError`]. Also errors if a version to revert has no
    /// corresponding `down` migration in the source.
    pub async fn revert_plan(
        &self,
        client: &clickhouse::Client,
        table: &str,
        target: i64,
    ) -> Result<Vec<i64>> {
        backend::ensure_table(client, table).await?;
        let applied = self.validated_applied(client, table).await?;

        let mut versions: Vec<i64> = applied
            .values()
            .filter(|m| m.success && m.version > target)
            .map(|m| m.version)
            .collect();
        versions.sort_unstable();
        versions.reverse();

        for version in &versions {
            if self.down(*version).is_none() {
                return Err(ChumError::Source(format!(
                    "no down migration for applied version {version}"
                )));
            }
        }
        Ok(versions)
    }

    /// Record a version as successfully applied **without running its SQL**,
    /// clearing a dirty state. Used after manually resolving a partial apply.
    ///
    /// # Errors
    ///
    /// Errors if the version is not present in the source set.
    pub async fn force(
        &self,
        client: &clickhouse::Client,
        table: &str,
        version: i64,
    ) -> Result<()> {
        backend::ensure_table(client, table).await?;
        let migration = self
            .up(version)
            .ok_or_else(|| ChumError::Source(format!("unknown migration version {version}")))?;
        backend::record(
            client,
            table,
            version,
            &migration.description,
            &migration.checksum,
            0,
            true,
        )
        .await?;
        Ok(())
    }

    /// Report the state of every known version (source ∪ recorded).
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn info(&self, client: &clickhouse::Client, table: &str) -> Result<Vec<Status>> {
        backend::ensure_table(client, table).await?;
        let applied: HashMap<i64, AppliedMigration> = backend::list_applied(client, table)
            .await?
            .into_iter()
            .map(|m| (m.version, m))
            .collect();

        let mut statuses = Vec::new();
        for migration in self.ups() {
            let rec = applied.get(&migration.version);
            let state = match rec {
                Some(rec) if rec.success => State::Applied,
                Some(_) => State::Dirty,
                None => State::Pending,
            };
            // Timing / applied-at are only meaningful for a successfully
            // applied version; a dirty marker records a zero duration.
            let (applied_at, execution_ms) = match rec {
                Some(rec) if rec.success => (Some(rec.applied_at), Some(rec.execution_ms)),
                _ => (None, None),
            };
            statuses.push(Status {
                version: migration.version,
                description: migration.description.to_string(),
                state,
                applied_at,
                execution_ms,
            });
        }
        statuses.sort_by_key(|s| s.version);
        Ok(statuses)
    }

    /// Fetch the applied set and validate it against the source (dirty +
    /// missing checks). Checksum drift is checked per-migration by the caller.
    async fn validated_applied(
        &self,
        client: &clickhouse::Client,
        table: &str,
    ) -> Result<HashMap<i64, AppliedMigration>> {
        let applied = backend::list_applied(client, table).await?;

        if let Some(dirty) = applied
            .iter()
            .filter(|m| !m.success)
            .map(|m| m.version)
            .min()
        {
            return Err(ChumError::Dirty(dirty));
        }

        if !self.ignore_missing {
            for rec in &applied {
                if self.up(rec.version).is_none() {
                    return Err(ChumError::VersionMissing(rec.version));
                }
            }
        }

        Ok(applied.into_iter().map(|m| (m.version, m)).collect())
    }

    /// Apply one migration: mark dirty, run each statement, mark success.
    async fn apply(
        &self,
        client: &clickhouse::Client,
        table: &str,
        migration: &Migration,
    ) -> Result<Duration> {
        let version = migration.version;
        let statements = split_statements(&migration.sql)
            .map_err(|message| ChumError::Split { version, message })?;

        backend::record(
            client,
            table,
            version,
            &migration.description,
            &migration.checksum,
            0,
            false,
        )
        .await?;

        let start = Instant::now();
        for stmt in &statements {
            client.query(stmt).execute().await?;
        }
        let elapsed = start.elapsed();

        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        backend::record(
            client,
            table,
            version,
            &migration.description,
            &migration.checksum,
            ms,
            true,
        )
        .await?;

        Ok(elapsed)
    }
}
