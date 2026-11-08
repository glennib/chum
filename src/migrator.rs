//! The migrator: validates the recorded state against the source set and
//! applies, reverts, or reports migrations.
//!
//! Every command that executes migrations checks, renders and splits all of
//! them before running the first statement, so a template mistake, a missing
//! variable or a split error anywhere aborts the command with nothing
//! executed and no marker written. The check covers every branch of a
//! template, including those the supplied values don't take.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use crate::backend;
use crate::backend::Bookkeeping;
use crate::error::ChumError;
use crate::error::RenderFailure;
use crate::error::Result;
#[cfg(feature = "template")]
use crate::error::TemplateLocation;
use crate::migration::AppliedMigration;
use crate::migration::Direction;
use crate::migration::Migration;
use crate::split::split_statements;
use crate::vars::Origin;
use crate::vars::References;
use crate::vars::Supplied;
use crate::vars::VarKey;
use crate::vars::Vars;

/// A migration that was applied during a [`Migrator::run`] call.
#[derive(Debug)]
#[non_exhaustive]
pub struct Applied {
    /// The version that was applied.
    pub version: i64,
    /// The migration's description.
    pub description: String,
    /// How long the migration's statements took to execute.
    pub elapsed: Duration,
    /// The variables recorded for the migration, keyed by namespaced name.
    pub vars: BTreeMap<String, String>,
}

/// The state of a single version, as reported by [`Migrator::info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Recorded as successfully applied.
    Applied,
    /// Present in the source but not yet applied.
    Pending,
    /// A previous apply or revert failed partway; the version is dirty.
    Dirty,
}

impl State {
    /// `"applied"`, `"pending"` or `"dirty"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Applied => "applied",
            State::Pending => "pending",
            State::Dirty => "dirty",
        }
    }
}

/// A line of [`Migrator::info`] output.
#[derive(Debug)]
#[non_exhaustive]
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
    /// The variables the latest marker recorded, keyed by namespaced name
    /// (`env.tier`). Empty for pending versions.
    pub vars: BTreeMap<String, String>,
    /// For a pending version whose up migration is a template, the namespaced
    /// names it refers to (`env.tier`, `this.cutoff`), so an operator can see
    /// what to pass. `env.*` or `this.*` means the template reads the whole
    /// namespace. Empty otherwise.
    pub template_vars: BTreeSet<String>,
}

/// A migration that a [`Migrator::run`] would apply, as reported by
/// [`Migrator::plan`] without applying anything.
#[derive(Debug)]
#[non_exhaustive]
pub struct Planned {
    /// The version that would be applied.
    pub version: i64,
    /// The migration's description.
    pub description: String,
    /// Every variable the migration would use or record, and where its value
    /// comes from.
    pub vars: Vec<UsedVar>,
}

/// A variable a migration uses, as reported by [`Migrator::plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UsedVar {
    /// The namespaced name, e.g. `env.tier`.
    pub name: String,
    /// The value, or `None` when nothing was supplied and the template's
    /// `| default(…)` or `is defined` covered it.
    pub value: Option<String>,
    /// Where the value came from; [`Origin::Default`] when `value` is `None`.
    pub origin: Origin,
}

/// Something about the supplied variables worth warning about. Collected by
/// [`Migrator::var_notices`]; none of these is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VarNotice {
    /// No template in the source refers to this variable, most likely a typo.
    Unreferenced {
        /// The variable.
        key: VarKey,
        /// Its supplied value and source.
        supplied: Supplied,
    },
    /// Templates refer to this variable, but none rendered by this migrator
    /// used it.
    Unused {
        /// The variable.
        key: VarKey,
        /// Its supplied value and source.
        supplied: Supplied,
    },
    /// A supplied `env.*` value differs from the one recorded when the
    /// version was applied; the supplied value was used.
    EnvChanged {
        /// The version being reverted or forced.
        version: i64,
        /// The namespaced name, e.g. `env.tier`.
        name: String,
        /// The supplied value, which was used.
        supplied: String,
        /// The recorded value.
        recorded: String,
    },
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

/// A supplied `this.*` value that differs from the one recorded for the
/// version, with the namespaced name (`this.cutoff`) and the supplying key.
struct Conflict {
    name: String,
    key: String,
    supplied: String,
    recorded: String,
}

/// A migration rendered and split, ready to execute.
struct Prepared<'a> {
    migration: &'a Migration,
    statements: Vec<String>,
    /// The variables to record with the migration's markers.
    record: BTreeMap<String, String>,
    /// What the migration uses, for [`Planned::vars`].
    used: Vec<UsedVar>,
}

/// What rendering in this migrator has used, for [`Migrator::var_notices`].
#[derive(Debug, Default)]
struct Usage {
    used: BTreeSet<VarKey>,
    env_changed: BTreeSet<(i64, String, String, String)>,
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
    vars: Vars,
    usage: Mutex<Usage>,
}

impl Migrator {
    /// Build a migrator from a resolved set of migrations.
    #[must_use]
    pub fn new(migrations: impl Into<Cow<'static, [Migration]>>) -> Self {
        Self {
            migrations: migrations.into(),
            ignore_missing: false,
            vars: Vars::new(),
            usage: Mutex::new(Usage::default()),
        }
    }

    /// If set, applied migrations that are absent from the source set do not
    /// cause an error.
    #[must_use]
    pub fn ignore_missing(mut self, ignore: bool) -> Self {
        self.ignore_missing = ignore;
        self
    }

    /// The variables templated migrations render with.
    #[must_use]
    pub fn vars(mut self, vars: Vars) -> Self {
        self.vars = vars;
        self
    }

    /// The migrations this migrator was built from, sorted by version with
    /// `up` before `down`.
    #[must_use]
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Check every template for mistakes that don't depend on the supplied
    /// values, without rendering it: a syntax error, or, in every branch, an
    /// unknown filter or test or one of chum's filters called with the wrong
    /// number of arguments. Plain migrations always pass.
    ///
    /// Needs no variables and no client, so a test can call it on embedded
    /// migrations to catch a mistake in a branch that only some environment
    /// reaches.
    ///
    /// # Errors
    ///
    /// [`ChumError::Check`] listing every mistake, or
    /// [`ChumError::TemplatingDisabled`] for a template when chum is built
    /// without the `template` feature.
    pub fn check(&self) -> Result<()> {
        self.check_matching(|_| true)
    }

    /// Like [`Migrator::check`], for the migrations `keep` returns true for,
    /// e.g. to skip templates that are already applied and can't be changed.
    ///
    /// # Errors
    ///
    /// As for [`Migrator::check`].
    pub fn check_matching(&self, keep: impl Fn(&Migration) -> bool) -> Result<()> {
        check_all(self.migrations.iter().filter(|m| keep(m)))
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

    fn find(&self, version: i64, direction: Direction) -> Result<&Migration> {
        match direction {
            Direction::Up => self
                .up(version)
                .ok_or_else(|| ChumError::Source(format!("unknown migration version {version}"))),
            Direction::Down => self.down(version).ok_or_else(|| {
                ChumError::Source(format!("no down migration for version {version}"))
            }),
        }
    }

    /// Apply all pending migrations, optionally stopping at `target`.
    ///
    /// Validates first: a dirty database, a checksum drift on an
    /// already-applied migration, or (unless [`Migrator::ignore_missing`]) an
    /// applied version absent from the source all abort the run before any
    /// new migration is applied. Every pending migration is then rendered
    /// and split before the first one runs.
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn run(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        target: Option<i64>,
    ) -> Result<Vec<Applied>> {
        self.run_with(client, bookkeeping, target, |_| {}).await
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
        bookkeeping: &Bookkeeping,
        target: Option<i64>,
        mut on_progress: F,
    ) -> Result<Vec<Applied>>
    where
        F: FnMut(Progress<'_>),
    {
        ensure_writable(client, bookkeeping).await?;
        let applied = self.validated_applied(client, bookkeeping).await?;
        let pending = self.pending(&applied, target)?;
        let prepared = self.prepare(pending.into_iter().map(|m| (m, None)))?;

        let mut report = Vec::new();
        for p in prepared {
            let migration = p.migration;
            on_progress(Progress::ApplyStarted {
                version: migration.version,
                description: migration.description.as_ref(),
            });
            let elapsed = self.apply(client, bookkeeping, &p).await?;
            on_progress(Progress::ApplyFinished {
                version: migration.version,
                description: migration.description.as_ref(),
                elapsed,
            });
            report.push(Applied {
                version: migration.version,
                description: migration.description.to_string(),
                elapsed,
                vars: p.record,
            });
        }
        Ok(report)
    }

    /// Compute the migrations a [`Migrator::run`] would apply, without applying
    /// any. Performs the *same* validation as `run` — a dirty database, a
    /// checksum drift, (unless [`Migrator::ignore_missing`]) a missing applied
    /// version, or a migration that fails to render aborts with the same error
    /// — so a dry-run reflects what the real run would do rather than an
    /// optimistic guess. Never alters the bookkeeping table's columns.
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn plan(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        target: Option<i64>,
    ) -> Result<Vec<Planned>> {
        backend::ensure_database(client, bookkeeping).await?;
        backend::ensure_table(client, bookkeeping).await?;
        let applied = self.validated_applied(client, bookkeeping).await?;
        let pending = self.pending(&applied, target)?;
        let prepared = self.prepare(pending.into_iter().map(|m| (m, None)))?;
        Ok(prepared
            .into_iter()
            .map(|p| Planned {
                version: p.migration.version,
                description: p.migration.description.to_string(),
                vars: p.used,
            })
            .collect())
    }

    /// The pending up migrations up to `target`, in order. Errors on checksum
    /// drift of any applied migration in that range.
    fn pending(
        &self,
        applied: &HashMap<i64, AppliedMigration>,
        target: Option<i64>,
    ) -> Result<Vec<&Migration>> {
        let mut pending = Vec::new();
        for migration in self.ups() {
            if target.is_some_and(|t| migration.version > t) {
                break;
            }
            if let Some(rec) = applied.get(&migration.version) {
                if rec.checksum != migration.checksum.as_ref() {
                    return Err(ChumError::ChecksumMismatch(migration.version));
                }
            } else {
                pending.push(migration);
            }
        }
        Ok(pending)
    }

    /// Revert applied migrations whose version is greater than `target`, in
    /// descending order.
    ///
    /// Every down migration is rendered and split before the first one runs.
    /// A down template renders with the values recorded when its version was
    /// applied: a recorded `this.*` value wins (a different supplied value is
    /// [`ChumError::VarConflict`]), and a supplied `env.*` value wins over a
    /// recorded one.
    ///
    /// # Errors
    ///
    /// See [`ChumError`]. Also errors if a version to revert has no
    /// corresponding `down` migration in the source.
    pub async fn undo(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        target: i64,
    ) -> Result<Vec<i64>> {
        self.undo_with(client, bookkeeping, target, |_| {}).await
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
        bookkeeping: &Bookkeeping,
        target: i64,
        mut on_progress: F,
    ) -> Result<Vec<i64>>
    where
        F: FnMut(Progress<'_>),
    {
        ensure_writable(client, bookkeeping).await?;
        let applied = self.validated_applied(client, bookkeeping).await?;
        let prepared = self.prepare_reverts(&applied, target)?;

        let mut reverted = Vec::new();
        for (rec, p) in prepared {
            let version = p.migration.version;
            on_progress(Progress::RevertStarted { version });
            backend::record_revert_started(client, bookkeeping, rec).await?;
            execute(client, &p).await?;
            backend::delete_version(client, bookkeeping, version).await?;
            on_progress(Progress::RevertFinished { version });
            reverted.push(version);
        }
        Ok(reverted)
    }

    /// Compute the versions a [`Migrator::undo`] would revert (descending),
    /// without reverting any. Validates the same conditions (dirty / missing),
    /// that each version has a `down` migration in the source, and that every
    /// down migration renders, so a revert that would fail surfaces the error
    /// here rather than after a confirmation.
    ///
    /// # Errors
    ///
    /// See [`ChumError`]. Also errors if a version to revert has no
    /// corresponding `down` migration in the source.
    pub async fn revert_plan(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        target: i64,
    ) -> Result<Vec<i64>> {
        backend::ensure_database(client, bookkeeping).await?;
        backend::ensure_table(client, bookkeeping).await?;
        let applied = self.validated_applied(client, bookkeeping).await?;
        Ok(self
            .prepare_reverts(&applied, target)?
            .into_iter()
            .map(|(_, p)| p.migration.version)
            .collect())
    }

    /// Render and split the down migration of every applied version above
    /// `target`, newest first, each paired with its recorded state.
    fn prepare_reverts<'a>(
        &'a self,
        applied: &'a HashMap<i64, AppliedMigration>,
        target: i64,
    ) -> Result<Vec<(&'a AppliedMigration, Prepared<'a>)>> {
        let mut versions: Vec<&AppliedMigration> = applied
            .values()
            .filter(|m| m.success && m.version > target)
            .collect();
        versions.sort_unstable_by_key(|m| std::cmp::Reverse(m.version));

        let mut downs = Vec::with_capacity(versions.len());
        for rec in &versions {
            let down = self.down(rec.version).ok_or_else(|| {
                ChumError::Source(format!(
                    "no down migration for applied version {}",
                    rec.version
                ))
            })?;
            downs.push((down, Some(&rec.vars)));
        }
        Ok(versions.into_iter().zip(self.prepare(downs)?).collect())
    }

    /// Record a version as successfully applied **without running its SQL**,
    /// clearing a dirty state. Used after manually resolving a partial apply.
    ///
    /// The up migration is still rendered, without executing it, so the
    /// variables it uses are recorded. Values recorded by earlier attempts
    /// take part as in [`Migrator::undo`]; a version with no rows needs every
    /// value supplied.
    ///
    /// # Errors
    ///
    /// Errors if the version is not present in the source set, or its up
    /// migration fails to render.
    pub async fn force(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        version: i64,
    ) -> Result<()> {
        ensure_writable(client, bookkeeping).await?;
        let migration = self.find(version, Direction::Up)?;
        let recorded = backend::list_applied(client, bookkeeping)
            .await?
            .into_iter()
            .find(|m| m.version == version)
            .map(|m| m.vars);
        let prepared = self.prepare([(migration, recorded.as_ref())])?;
        backend::record(client, bookkeeping, migration, 0, true, &prepared[0].record).await?;
        Ok(())
    }

    /// Delete the bookkeeping rows of a **dirty** version, so it reads as
    /// pending again and the next [`Migrator::run`] applies it afresh.
    ///
    /// The database objects are not touched: the caller asserts that the
    /// version's objects are gone, because a failed apply left nothing behind,
    /// or a failed revert was finished by hand.
    ///
    /// # Errors
    ///
    /// Returns [`ChumError::NotDirty`] for an applied or pending version.
    pub async fn reset(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        version: i64,
    ) -> Result<()> {
        backend::ensure_database(client, bookkeeping).await?;
        backend::ensure_table(client, bookkeeping).await?;
        let rec = backend::list_applied(client, bookkeeping)
            .await?
            .into_iter()
            .find(|m| m.version == version);
        match rec {
            Some(rec) if !rec.success => {
                backend::delete_version(client, bookkeeping, version).await
            }
            Some(_) => Err(ChumError::NotDirty {
                version,
                state: State::Applied,
            }),
            None => Err(ChumError::NotDirty {
                version,
                state: State::Pending,
            }),
        }
    }

    /// Render one migration with the supplied variables and split it into
    /// statements, without a client. A plain migration is only split.
    ///
    /// # Errors
    ///
    /// Returns [`ChumError::Source`] if the migration doesn't exist,
    /// [`ChumError::Render`] if it fails to render, or [`ChumError::Split`].
    pub fn render(&self, version: i64, direction: Direction) -> Result<Vec<String>> {
        Ok(self
            .render_many(&[version], direction)?
            .pop()
            .unwrap_or_default())
    }

    /// Like [`Migrator::render`] for several versions, in the order given.
    /// Every one is rendered before any error is returned, and a
    /// [`ChumError::Render`] lists every failure.
    ///
    /// # Errors
    ///
    /// As for [`Migrator::render`].
    pub fn render_many(&self, versions: &[i64], direction: Direction) -> Result<Vec<Vec<String>>> {
        let migrations = versions
            .iter()
            .map(|v| Ok((self.find(*v, direction)?, None)))
            .collect::<Result<Vec<_>>>()?;
        Ok(self
            .prepare(migrations)?
            .into_iter()
            .map(|p| p.statements)
            .collect())
    }

    /// Report the state of every known version (source ∪ recorded).
    ///
    /// # Errors
    ///
    /// See [`ChumError`].
    pub async fn info(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
    ) -> Result<Vec<Status>> {
        backend::ensure_database(client, bookkeeping).await?;
        backend::ensure_table(client, bookkeeping).await?;
        let applied: HashMap<i64, AppliedMigration> = backend::list_applied(client, bookkeeping)
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
            let template_vars = if state == State::Pending {
                template_vars(migration)
            } else {
                BTreeSet::new()
            };
            statuses.push(Status {
                version: migration.version,
                description: migration.description.to_string(),
                state,
                applied_at,
                execution_ms,
                vars: rec.map(|rec| rec.vars.clone()).unwrap_or_default(),
                template_vars,
            });
        }
        statuses.sort_by_key(|s| s.version);
        Ok(statuses)
    }

    /// Warnings about the supplied variables: those no template in the source
    /// refers to, those no migration rendered by this migrator so far used,
    /// and `env.*` values that differ from the recorded ones during a revert
    /// or force. A variable that is unreferenced is not also reported as
    /// unused.
    #[must_use]
    pub fn var_notices(&self) -> Vec<VarNotice> {
        let usage = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        let mut notices = Vec::new();
        for (key, supplied) in self.vars.iter() {
            if !self.is_referenced(&key) {
                notices.push(VarNotice::Unreferenced {
                    key,
                    supplied: supplied.clone(),
                });
            } else if !usage.used.contains(&key) {
                notices.push(VarNotice::Unused {
                    key,
                    supplied: supplied.clone(),
                });
            }
        }
        for (version, name, supplied, recorded) in &usage.env_changed {
            notices.push(VarNotice::EnvChanged {
                version: *version,
                name: name.clone(),
                supplied: supplied.clone(),
                recorded: recorded.clone(),
            });
        }
        notices
    }

    /// Whether any template in the source may read `key`.
    fn is_referenced(&self, key: &VarKey) -> bool {
        self.migrations.iter().any(|m| {
            let refs = references(m);
            match key {
                VarKey::Env(name) => refs.env.contains(name),
                VarKey::This { version, name } => m.version == *version && refs.this.contains(name),
            }
        })
    }

    /// Fetch the applied set and validate it against the source (dirty +
    /// missing checks). Checksum drift is checked per-migration by the caller.
    async fn validated_applied(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
    ) -> Result<HashMap<i64, AppliedMigration>> {
        let applied = backend::list_applied(client, bookkeeping).await?;

        if let Some(dirty) = applied
            .iter()
            .filter(|m| !m.success)
            .min_by_key(|m| m.version)
        {
            return Err(if dirty.reverting {
                ChumError::DirtyRevert(dirty.version)
            } else {
                ChumError::Dirty(dirty.version)
            });
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

    /// Check, render and split every migration, each with the values recorded
    /// for its version (if any). Check failures across all of them are
    /// reported first, then rendering failures; split errors are checked only
    /// once everything rendered.
    fn prepare<'a, I>(&'a self, migrations: I) -> Result<Vec<Prepared<'a>>>
    where
        I: IntoIterator<Item = (&'a Migration, Option<&'a BTreeMap<String, String>>)>,
    {
        let migrations: Vec<_> = migrations.into_iter().collect();
        check_all(migrations.iter().map(|(m, _)| *m))?;

        let mut rendered = Vec::new();
        let mut failures = Vec::new();
        for (migration, recorded) in migrations {
            if migration.template && !cfg!(feature = "template") {
                return Err(ChumError::TemplatingDisabled(
                    migration.file_name.to_string(),
                ));
            }
            let (env, this, conflicts) = self.effective(migration.version, recorded);
            match render(migration, &env, &this) {
                Ok(r) => {
                    if let Some(c) = conflicts.into_iter().find(|c| r.used.contains_key(&c.name)) {
                        return Err(ChumError::VarConflict {
                            version: migration.version,
                            name: c.name,
                            key: c.key,
                            supplied: c.supplied,
                            recorded: c.recorded,
                        });
                    }
                    rendered.push((migration, env, this, r));
                }
                Err(failure) => failures.push(failure),
            }
        }
        if !failures.is_empty() {
            return Err(ChumError::Render(failures));
        }

        let mut prepared = Vec::with_capacity(rendered.len());
        for (migration, env, this, r) in rendered {
            let version = migration.version;
            let statements = split_statements(&r.sql)
                .map_err(|message| ChumError::Split { version, message })?;
            prepared.push(self.finish(migration, statements, &env, &this, r));
        }
        Ok(prepared)
    }

    /// Work out what a rendered migration records and uses, and log the
    /// supplied variables it used.
    fn finish<'a>(
        &self,
        migration: &'a Migration,
        statements: Vec<String>,
        env: &BTreeMap<String, Supplied>,
        this: &BTreeMap<String, Supplied>,
        r: Render,
    ) -> Prepared<'a> {
        let version = migration.version;
        let lookup = |namespaced: &str| -> Option<(VarKey, &Supplied)> {
            if let Some(name) = namespaced.strip_prefix("env.") {
                Some((VarKey::Env(name.to_owned()), env.get(name)?))
            } else {
                let name = namespaced.strip_prefix("this.")?;
                let key = VarKey::This {
                    version,
                    name: name.to_owned(),
                };
                Some((key, this.get(name)?))
            }
        };

        let mut record = r.used;
        // The down migration isn't rendered at apply time, but a revert falls
        // back on recorded values for the names only it uses.
        if migration.direction == Direction::Up
            && let Some(down) = self.down(version)
        {
            let refs = references(down);
            for (name, s) in env {
                if refs.env.contains(name) {
                    record.insert(format!("env.{name}"), s.value.clone());
                }
            }
            for (name, s) in this {
                if refs.this.contains(name) {
                    record.insert(format!("this.{name}"), s.value.clone());
                }
            }
        }

        let mut used = Vec::new();
        let mut usage = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        for name in record.keys() {
            if let Some((key, s)) = lookup(name) {
                used.push(UsedVar {
                    name: name.clone(),
                    value: Some(s.value.clone()),
                    origin: s.origin,
                });
                usage.used.insert(key);
            }
        }
        for name in &r.missing {
            used.push(UsedVar {
                name: name.clone(),
                value: None,
                origin: Origin::Default,
            });
        }
        used.sort_by(|a, b| a.name.cmp(&b.name));

        Prepared {
            migration,
            statements,
            record,
            used,
        }
    }

    /// The `env` and `this` namespaces for `version`: the supplied values,
    /// combined with the values recorded for the version, if any.
    ///
    /// A recorded `this.*` value describes what was applied and wins. A
    /// different supplied value is returned as a [`Conflict`]; the caller
    /// raises it only if the render reads that name, so a stale value for a
    /// name the migration doesn't use can't block it. A supplied `env.*` value
    /// describes the environment as it is now and wins, with a notice when it
    /// differs from the recorded one. Recorded values fill names nothing was
    /// supplied for.
    fn effective(
        &self,
        version: i64,
        recorded: Option<&BTreeMap<String, String>>,
    ) -> (
        BTreeMap<String, Supplied>,
        BTreeMap<String, Supplied>,
        Vec<Conflict>,
    ) {
        let mut env = self.vars.env().clone();
        let mut this = self.vars.this(version).cloned().unwrap_or_default();
        let mut conflicts = Vec::new();
        for (namespaced, value) in recorded.into_iter().flatten() {
            let from_record = || Supplied::new(value.clone(), Origin::Recorded, namespaced.clone());
            if let Some(name) = namespaced.strip_prefix("this.") {
                if let Some(s) = this.get(name)
                    && s.value != *value
                {
                    conflicts.push(Conflict {
                        name: namespaced.clone(),
                        key: s.origin.describe(&s.key),
                        supplied: s.value.clone(),
                        recorded: value.clone(),
                    });
                }
                this.insert(name.to_owned(), from_record());
            } else if let Some(name) = namespaced.strip_prefix("env.") {
                match env.get(name) {
                    Some(s) if s.value != *value => {
                        self.usage
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .env_changed
                            .insert((version, namespaced.clone(), s.value.clone(), value.clone()));
                    }
                    Some(_) => {}
                    None => {
                        env.insert(name.to_owned(), from_record());
                    }
                }
            }
        }
        (env, this, conflicts)
    }

    /// Apply one prepared migration: mark dirty, run each statement, mark
    /// success.
    async fn apply(
        &self,
        client: &clickhouse::Client,
        bookkeeping: &Bookkeeping,
        prepared: &Prepared<'_>,
    ) -> Result<Duration> {
        let migration = prepared.migration;
        let vars = &prepared.record;
        backend::record(client, bookkeeping, migration, 0, false, vars).await?;

        let start = Instant::now();
        execute(client, prepared).await?;
        let elapsed = start.elapsed();

        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        backend::record(client, bookkeeping, migration, ms, true, vars).await?;

        Ok(elapsed)
    }
}

/// Prepare the bookkeeping table for a command that writes markers: create the
/// database and table if missing, and add the `vars` column to a table created
/// by an older chum.
async fn ensure_writable(client: &clickhouse::Client, bookkeeping: &Bookkeeping) -> Result<()> {
    backend::ensure_database(client, bookkeeping).await?;
    backend::ensure_table(client, bookkeeping).await?;
    backend::upgrade_table(client, bookkeeping).await?;
    Ok(())
}

/// Run a prepared migration's statements in order, stopping at the first
/// failure.
async fn execute(client: &clickhouse::Client, prepared: &Prepared<'_>) -> Result<()> {
    let migration = prepared.migration;
    let total = prepared.statements.len();
    for (i, stmt) in prepared.statements.iter().enumerate() {
        // `query` would treat any `?` as a bind placeholder.
        client
            .query_raw(stmt)
            .execute()
            .await
            .map_err(|source| ChumError::StatementFailed {
                version: migration.version,
                direction: migration.direction,
                index: i + 1,
                total,
                statement: stmt.clone(),
                source,
            })?;
    }
    Ok(())
}

/// The output of rendering one migration.
struct Render {
    sql: String,
    used: BTreeMap<String, String>,
    missing: BTreeSet<String>,
}

/// Render a migration; a plain migration renders to its own SQL.
fn render(
    migration: &Migration,
    env: &BTreeMap<String, Supplied>,
    this: &BTreeMap<String, Supplied>,
) -> std::result::Result<Render, RenderFailure> {
    if !migration.template {
        return Ok(Render {
            sql: migration.sql.to_string(),
            used: BTreeMap::new(),
            missing: BTreeSet::new(),
        });
    }
    render_template(migration, env, this)
}

#[cfg(feature = "template")]
fn render_template(
    migration: &Migration,
    env: &BTreeMap<String, Supplied>,
    this: &BTreeMap<String, Supplied>,
) -> std::result::Result<Render, RenderFailure> {
    let values = |m: &BTreeMap<String, Supplied>| -> BTreeMap<String, String> {
        m.iter()
            .map(|(k, s)| (k.clone(), s.value.clone()))
            .collect()
    };
    crate::template::render(
        &migration.file_name,
        &migration.sql,
        &values(env),
        &values(this),
    )
    .map(|r| Render {
        sql: r.sql,
        used: r.used,
        missing: r.missing,
    })
    .map_err(|f| failure(migration, f))
}

/// Statically check every template among `migrations`.
fn check_all<'a>(migrations: impl IntoIterator<Item = &'a Migration>) -> Result<()> {
    let mut failures = Vec::new();
    for migration in migrations {
        if !migration.template {
            continue;
        }
        if !cfg!(feature = "template") {
            return Err(ChumError::TemplatingDisabled(
                migration.file_name.to_string(),
            ));
        }
        failures.extend(check_template(migration));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ChumError::Check(failures))
    }
}

#[cfg(feature = "template")]
fn check_template(migration: &Migration) -> Vec<RenderFailure> {
    crate::template::check(&migration.file_name, &migration.sql)
        .into_iter()
        .map(|f| failure(migration, f))
        .collect()
}

#[cfg(not(feature = "template"))]
fn check_template(_migration: &Migration) -> Vec<RenderFailure> {
    Vec::new()
}

#[cfg(feature = "template")]
fn failure(migration: &Migration, f: crate::template::Failure) -> RenderFailure {
    let location = f.span.map(|span| {
        TemplateLocation::new(
            migration.file_name.to_string(),
            migration.sql.to_string(),
            span,
        )
    });
    RenderFailure::new(migration.version, migration.direction, f.message, f.missing)
        .with_kind(f.kind)
        .with_location(location)
}

#[cfg(not(feature = "template"))]
fn render_template(
    migration: &Migration,
    _env: &BTreeMap<String, Supplied>,
    _this: &BTreeMap<String, Supplied>,
) -> std::result::Result<Render, RenderFailure> {
    Err(RenderFailure::new(
        migration.version,
        migration.direction,
        format!(
            "{} is a template, but chum was built without the `template` feature",
            migration.file_name
        ),
        Vec::new(),
    ))
}

/// The names a migration may read from each namespace; nothing for a plain
/// migration, or without the `template` feature.
fn references(migration: &Migration) -> References {
    #[cfg(feature = "template")]
    if migration.template {
        return crate::template::references(&migration.file_name, &migration.sql);
    }
    #[cfg(not(feature = "template"))]
    let _ = migration;
    References::default()
}

/// The namespaced names an up template refers to, for [`Status::template_vars`].
fn template_vars(migration: &Migration) -> BTreeSet<String> {
    let refs = references(migration);
    let mut out = BTreeSet::new();
    for (prefix, refs) in [("env", &refs.env), ("this", &refs.this)] {
        if refs.all {
            out.insert(format!("{prefix}.*"));
        }
        out.extend(refs.names.iter().map(|n| format!("{prefix}.{n}")));
    }
    out
}

#[cfg(all(test, feature = "template"))]
mod tests {
    use super::*;
    use crate::error::RenderFailureKind;

    fn up(version: i64, sql: &'static str) -> Migration {
        Migration::new(version, "m", Direction::Up, sql).with_template(true)
    }

    fn down(version: i64, sql: &'static str) -> Migration {
        Migration::new(version, "m", Direction::Down, sql).with_template(true)
    }

    fn recorded(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn every_render_failure_is_reported() {
        let migrator = Migrator::new(vec![
            up(1, "SELECT {{ env.a }}"),
            up(2, "SELECT 2"),
            up(3, "SELECT {{ this.b }}"),
        ]);
        let err = migrator.render_many(&[1, 2, 3], Direction::Up).unwrap_err();
        let ChumError::Render(failures) = err else {
            panic!("expected Render, got {err}");
        };
        let versions: Vec<i64> = failures.iter().map(|f| f.version).collect();
        assert_eq!(versions, [1, 3]);
        assert_eq!(failures[0].missing, ["env.a"]);
        assert_eq!(failures[1].missing, ["this.b"]);
        let message = ChumError::Render(failures).to_string();
        assert!(
            message.contains("failed to render 2 migrations"),
            "{message}"
        );
        assert!(message.contains("`this.b`"), "{message}");
    }

    #[test]
    fn mistakes_in_untaken_branches_fail_before_rendering() {
        let migrator = Migrator::new(vec![
            up(
                1,
                "{% if this.cutoff is defined %}{{ this.cutoff | sql_datetime64 }}{% endif %}",
            ),
            up(2, "SELECT {{ env.missing }}"),
            up(3, "{% if false %}{{ 1 | nope }}{{ 2 | nope }}{% endif %}"),
        ]);
        // Version 2's missing value isn't reported: the check runs first.
        let err = migrator.render_many(&[1, 2, 3], Direction::Up).unwrap_err();
        let ChumError::Check(failures) = err else {
            panic!("expected Check, got {err}");
        };
        let versions: Vec<i64> = failures.iter().map(|f| f.version).collect();
        assert_eq!(versions, [1, 3, 3]);
        assert!(
            failures
                .iter()
                .all(|f| f.kind == RenderFailureKind::Template && f.location.is_some())
        );
        let message = ChumError::Check(failures).to_string();
        assert!(
            message.starts_with("3 template mistakes in 2 migrations:"),
            "{message}"
        );
        assert!(message.contains("filter sql_datetime64 takes one argument"));

        assert!(matches!(migrator.check(), Err(ChumError::Check(_))));
        migrator.check_matching(|m| m.version == 2).unwrap();
    }

    #[test]
    fn render_failures_say_whether_the_value_or_the_template_is_wrong() {
        let migrator = Migrator::new(vec![
            up(1, "SELECT {{ this.cutoff | sql_datetime }}"),
            up(2, "SELECT {{ this.cutoff }}"),
            up(3, "SELECT {{ this.cutoff | sql_datetime64(10) }}"),
        ])
        .vars(
            Vars::from_keys(
                [
                    ("1_cutoff".to_owned(), "2026-11-10".to_owned()),
                    ("3_cutoff".to_owned(), "2026-11-10T09:00:00Z".to_owned()),
                ],
                [],
            )
            .unwrap(),
        );
        let err = migrator.render_many(&[1, 2, 3], Direction::Up).unwrap_err();
        let ChumError::Render(failures) = err else {
            panic!("expected Render, got {err}");
        };
        let kinds: Vec<RenderFailureKind> = failures.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                RenderFailureKind::InvalidValue,
                RenderFailureKind::MissingValue,
                RenderFailureKind::Template,
            ]
        );
        let location = failures[0].location.as_ref().unwrap();
        assert_eq!(location.file_name, "1_m.up.sql.j2");
        assert_eq!(&location.text[location.span.clone()], "sql_datetime");
    }

    #[test]
    fn plain_migrations_are_split_but_not_rendered() {
        let migrator = Migrator::new(vec![Migration::new(
            1,
            "m",
            Direction::Up,
            "SELECT '{{ x }}'; SELECT 2",
        )]);
        assert_eq!(
            migrator.render(1, Direction::Up).unwrap(),
            ["SELECT '{{ x }}'", "SELECT 2"]
        );
    }

    #[test]
    fn up_records_what_it_used_and_what_its_down_refers_to() {
        let migrator = Migrator::new(vec![
            up(5, "SELECT {{ env.tier }} {{ this.cutoff | default('x') }}"),
            down(5, "SELECT {{ this.drop_after }} {{ env.cluster }}"),
        ])
        .vars(
            Vars::new()
                .with_env("tier", "prod")
                .with_env("cluster", "c1")
                .with_env("unrelated", "u")
                .with_this(5, "drop_after", "d"),
        );
        let p = migrator.prepare([(migrator.up(5).unwrap(), None)]).unwrap();
        assert_eq!(
            p[0].record,
            recorded(&[
                ("env.cluster", "c1"),
                ("env.tier", "prod"),
                ("this.drop_after", "d")
            ])
        );
        let defaulted = p[0].used.iter().find(|u| u.name == "this.cutoff").unwrap();
        assert_eq!(defaulted.value, None);
        assert_eq!(defaulted.origin, Origin::Default);
    }

    #[test]
    fn recorded_this_wins_and_conflicts_are_errors() {
        let migrator = Migrator::new(vec![down(5, "{{ this.cutoff }}/{{ this.other }}")])
            .vars(Vars::new().with_this(5, "other", "supplied"));
        let rec = recorded(&[("this.cutoff", "T")]);
        let p = migrator
            .prepare([(migrator.down(5).unwrap(), Some(&rec))])
            .unwrap();
        // The recorded value fills `cutoff`; the supplied one fills a gap.
        assert_eq!(p[0].statements, ["T/supplied"]);

        // The same value supplied again is fine.
        let migrator = migrator.vars(
            Vars::new()
                .with_this(5, "cutoff", "T")
                .with_this(5, "other", "o"),
        );
        assert!(
            migrator
                .prepare([(migrator.down(5).unwrap(), Some(&rec))])
                .is_ok()
        );

        // A different one is a conflict.
        let migrator = migrator.vars(
            Vars::new()
                .with_this(5, "cutoff", "U")
                .with_this(5, "other", "o"),
        );
        let err = migrator
            .prepare([(migrator.down(5).unwrap(), Some(&rec))])
            .err()
            .expect("conflict");
        assert!(
            matches!(
                &err,
                ChumError::VarConflict { version: 5, name, supplied, recorded, .. }
                    if name == "this.cutoff" && supplied == "U" && recorded == "T"
            ),
            "{err}"
        );
    }

    #[test]
    fn conflicts_on_names_the_template_does_not_read_are_ignored() {
        // A stale `<version>_cutoff` left in a deploy spec must not block a
        // migration that never reads `this.cutoff`.
        let migrator = Migrator::new(vec![down(5, "DROP {{ this.other }}")]).vars(
            Vars::new()
                .with_this(5, "cutoff", "U")
                .with_this(5, "other", "o"),
        );
        let rec = recorded(&[("this.cutoff", "T")]);
        let p = migrator
            .prepare([(migrator.down(5).unwrap(), Some(&rec))])
            .expect("no conflict for an unread name");
        assert_eq!(p[0].statements, ["DROP o"]);
        assert!(!p[0].record.contains_key("this.cutoff"));
    }

    #[test]
    fn supplied_env_wins_over_recorded_with_a_notice() {
        let migrator = Migrator::new(vec![down(5, "{{ env.tier }}/{{ env.region }}")])
            .vars(Vars::new().with_env("tier", "staging"));
        let rec = recorded(&[("env.tier", "prod"), ("env.region", "eu")]);
        let p = migrator
            .prepare([(migrator.down(5).unwrap(), Some(&rec))])
            .unwrap();
        assert_eq!(p[0].statements, ["staging/eu"]);
        assert!(migrator.var_notices().contains(&VarNotice::EnvChanged {
            version: 5,
            name: "env.tier".into(),
            supplied: "staging".into(),
            recorded: "prod".into(),
        }));
    }

    #[test]
    fn notices_report_unreferenced_and_unused_variables() {
        let migrator = Migrator::new(vec![up(1, "{{ env.tier }}"), up(2, "{{ this.cutoff }}")])
            .vars(
                Vars::new()
                    .with_env("tier", "prod")
                    .with_env("teir", "prod")
                    .with_this(2, "cutoff", "T")
                    .with_this(1, "cutoff", "T"),
            );
        migrator.render(1, Direction::Up).unwrap();
        let notices = migrator.var_notices();
        let unreferenced: Vec<String> = notices
            .iter()
            .filter_map(|n| match n {
                VarNotice::Unreferenced { key, .. } => Some(key.key()),
                _ => None,
            })
            .collect();
        assert_eq!(unreferenced, ["teir", "1_cutoff"]);
        let unused: Vec<String> = notices
            .iter()
            .filter_map(|n| match n {
                VarNotice::Unused { key, .. } => Some(key.key()),
                _ => None,
            })
            .collect();
        assert_eq!(unused, ["2_cutoff"]);
    }
}
