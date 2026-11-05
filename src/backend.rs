//! ClickHouse-side bookkeeping: the table that records which migrations have
//! been applied.
//!
//! # Engine choice: plain `MergeTree`
//!
//! The bookkeeping table must be durable, deletable (for `revert`), and — for a
//! general tool — work unchanged on a managed/clustered ClickHouse. That last
//! requirement is what decides the engine:
//!
//! * `Memory` is ruled out — applied state must survive a restart.
//! * `TinyLog` / `Log` / `StripeLog` are ruled out — they support no mutations
//!   (so no `DELETE` for revert), no replication, and a `Replicated` database
//!   rejects or fails to replicate them. (golang-migrate uses `TinyLog`, which
//!   is exactly why it needs an `x-migrations-table-engine=MergeTree` override
//!   on managed clusters.)
//! * `EmbeddedRocksDB` is node-local; `KeeperMap` needs Keeper configured.
//!
//! Plain `MergeTree` is durable, supports lightweight `DELETE`, and on a
//! `Replicated` database is auto-promoted to `ReplicatedMergeTree` — so the
//! *same* DDL gives consistent, replicated bookkeeping on a local single node
//! and on a managed cluster. (A non-`Replicated` cluster wanting an explicit
//! `ReplicatedMergeTree` + `ON CLUSTER` would need a configurable engine; that
//! is deliberately out of scope for the single-node-first v1.)
//!
//! Within the family we use plain `MergeTree` rather than `ReplacingMergeTree`:
//! an **append-only** table read with `argMax(col, seq)` (where `seq` is a
//! server-assigned nanosecond counter) is correct regardless of merge timing
//! (no `FINAL` needed), avoids the tie-ambiguity of a version column when the
//! dirty and success markers land in the same millisecond, and keeps a full
//! audit trail of every apply / revert / force attempt.
//!
//! Apply flow for one migration (each statement is its own HTTP request, since
//! ClickHouse executes one statement per request):
//!
//! 1. append a `success = false` marker (the version is now *dirty*),
//! 2. run each split statement,
//! 3. append a `success = true` marker with the elapsed time.
//!
//! If step 2 fails partway, the latest marker stays `success = false` and the
//! version reads as dirty until resolved with `chum force`.
//!
//! # Cross-replica consistency
//!
//! The append-only + `argMax` design above makes a read correct regardless of
//! *merge timing within a node*. It does **not**, on its own, guarantee
//! *read-after-write across replicas*: on a managed/`Replicated` cluster behind
//! a load-balanced endpoint, a later command can be routed to a replica that
//! has not yet replicated an earlier command's write, making chum re-apply a
//! migration or misreport state. That guarantee comes from a bundle of session
//! settings (opt-in via `--strict-consistency`) applied client-wide where the
//! client is built (see `build_client` in the `chum` binary), not from anything
//! in this module. The bundle is **off by default** because its `insert_quorum`
//! settings were found to hang every bookkeeping `INSERT` against the Aiven
//! cluster (the `INSERT` blocks until `insert_quorum_timeout`):
//!
//! * `insert_quorum=auto` + `insert_quorum_parallel=0` +
//!   `select_sequential_consistency=1` — the INSERT waits for a majority of
//!   replicas and the SELECT refuses to read behind it (the three are a single
//!   bundle; sequential consistency is inert without non-parallel quorum);
//! * `lightweight_deletes_sync=2` — the revert `DELETE` waits for all replicas;
//! * `async_insert=0` + `wait_end_of_query=1` — the bookkeeping insert is never
//!   server-buffered and the HTTP response waits until it is committed.
//!
//! None of this requires a table-DDL or cluster-config change: the bookkeeping
//! table being `ReplicatedMergeTree` is supplied automatically by the
//! `Replicated` database engine (the reason for the plain-`MergeTree` choice
//! above). These settings are inert no-ops on a single node; their
//! cross-replica behavior is documented but not yet exercised against the Aiven
//! cluster.

use std::collections::BTreeMap;

use crate::error::ChumError;
use crate::error::Result;
use crate::migration::AppliedMigration;
use crate::migration::Migration;

/// Where chum keeps its bookkeeping: the dedicated database plus the table
/// within it. Every bookkeeping statement is fully-qualified as
/// `<database>.<table>`, so bookkeeping never depends on the *app* database
/// (the connection's session default) existing — a migration is free to create
/// its own database.
///
/// Both identifiers are validated at construction ([`Bookkeeping::new`]) since
/// they are interpolated into SQL directly, not bound as query parameters.
#[derive(Debug, Clone)]
pub struct Bookkeeping {
    database: String,
    table: String,
}

impl Bookkeeping {
    /// Build a bookkeeping target, validating both identifiers.
    ///
    /// # Errors
    ///
    /// Returns [`ChumError::Source`] if either identifier is not a bare
    /// `[A-Za-z0-9_]` SQL identifier.
    pub fn new(database: impl Into<String>, table: impl Into<String>) -> Result<Self> {
        let database = database.into();
        let table = table.into();
        validate_ident(&database, "bookkeeping database")?;
        validate_ident(&table, "bookkeeping table")?;
        Ok(Self { database, table })
    }

    /// The bookkeeping database name.
    #[must_use]
    pub fn database(&self) -> &str {
        &self.database
    }

    /// The bookkeeping table name (unqualified).
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The fully-qualified `<database>.<table>` reference used in SQL.
    #[must_use]
    fn qualified(&self) -> String {
        format!("{}.{}", self.database, self.table)
    }
}

/// Validate that `ident` is a bare SQL identifier.
///
/// Database and table names cannot be bound as query parameters, so they are
/// interpolated into SQL directly; this guards against injection. `kind` names
/// the identifier in the error message (e.g. `"bookkeeping table"`).
fn validate_ident(ident: &str, kind: &str) -> Result<()> {
    let ok = !ident.is_empty() && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ChumError::Source(format!(
            "invalid {kind} name {ident:?}: expected [A-Za-z0-9_]"
        )))
    }
}

/// Row shape read back from the bookkeeping table.
#[derive(Debug, clickhouse::Row, serde::Deserialize)]
struct AppliedRow {
    version: i64,
    description: String,
    checksum: String,
    execution_ms: u64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    applied_at: chrono::DateTime<chrono::Utc>,
    success: bool,
    succeeded_before: bool,
    vars: Vec<(String, String)>,
}

/// Create the bookkeeping database if it does not already exist.
///
/// This runs before [`ensure_table`] so the fully-qualified bookkeeping table
/// has a database to live in. Like the table, it is chum's own object and is
/// created idempotently on every run (unlike user migrations, which
/// deliberately omit `IF NOT EXISTS`). It never depends on the app database, so
/// a migration is free to create its own.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the DDL fails.
pub async fn ensure_database(client: &clickhouse::Client, bookkeeping: &Bookkeeping) -> Result<()> {
    let ddl = format!("CREATE DATABASE IF NOT EXISTS {}", bookkeeping.database);
    client.query(&ddl).execute().await?;
    Ok(())
}

/// Create the bookkeeping table if it does not already exist.
///
/// Unlike user migrations (which deliberately omit `IF NOT EXISTS`), this is
/// chum's internal table and is created idempotently on every run. The table is
/// fully-qualified to the bookkeeping database, so [`ensure_database`] must run
/// first.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the DDL fails.
pub async fn ensure_table(client: &clickhouse::Client, bookkeeping: &Bookkeeping) -> Result<()> {
    let table = bookkeeping.qualified();
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS {table}
         (
             version      Int64,
             description  String,
             checksum     String,
             execution_ms UInt64,
             success      Bool,
             applied_at   DateTime64(3, 'UTC') DEFAULT now64(3),
             seq          UInt64,
             vars         Map(String, String)
         )
         ENGINE = MergeTree
         ORDER BY (version, seq)"
    );
    client.query(&ddl).execute().await?;
    Ok(())
}

/// The statement that adds the `vars` column to a bookkeeping table created
/// before chum recorded variables.
#[must_use]
pub fn upgrade_statement(bookkeeping: &Bookkeeping) -> String {
    format!(
        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS vars Map(String, String)",
        bookkeeping.qualified()
    )
}

/// Whether the bookkeeping table has the `vars` column.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the query fails.
pub async fn has_vars_column(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
) -> Result<bool> {
    let n: u64 = client
        .query(
            "SELECT count() FROM system.columns WHERE database = ? AND table = ? AND name = 'vars'",
        )
        .bind(bookkeeping.database())
        .bind(bookkeeping.table())
        .fetch_one()
        .await?;
    Ok(n > 0)
}

/// Add the `vars` column to a bookkeeping table that lacks it, returning the
/// statement that was run, or `None` when the column was already there.
///
/// Only commands that write bookkeeping rows call this; read-only commands
/// read a missing column as empty maps (see [`list_applied`]), so inspecting a
/// database never alters it. The change is additive: an older chum names its
/// columns on insert and select, so it keeps working against the upgraded
/// table.
///
/// # Errors
///
/// Returns [`ChumError::BookkeepingUpgrade`], carrying the statement, if the
/// `ALTER` is refused, or [`ChumError::ClickHouse`] if the column lookup fails.
pub async fn upgrade_table(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
) -> Result<Option<String>> {
    if has_vars_column(client, bookkeeping).await? {
        return Ok(None);
    }
    let statement = upgrade_statement(bookkeeping);
    client
        .query(&statement)
        .execute()
        .await
        .map_err(|source| ChumError::BookkeepingUpgrade {
            statement: statement.clone(),
            source,
        })?;
    Ok(Some(statement))
}

/// Return the latest recorded state of each version, ordered ascending.
///
/// A table without the `vars` column reads every version's variables as
/// empty.
///
/// A dirty version with an earlier `success = true` marker reads as
/// [`reverting`](AppliedMigration::reverting): only a revert appends a `false`
/// marker after a `true` one, since `run` only applies versions with no rows.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the query fails.
pub async fn list_applied(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
) -> Result<Vec<AppliedMigration>> {
    let table = bookkeeping.qualified();
    let vars = if has_vars_column(client, bookkeeping).await? {
        "argMax(vars, seq)"
    } else {
        "CAST(map(), 'Map(String, String)')"
    };
    // `bk.success` names the column: a bare `success` inside `max` resolves to
    // the `argMax` alias, which ClickHouse rejects as a nested aggregate.
    let sql = format!(
        "SELECT version,
                argMax(description, seq)  AS description,
                argMax(checksum, seq)     AS checksum,
                argMax(execution_ms, seq) AS execution_ms,
                argMax(applied_at, seq)   AS applied_at,
                argMax(success, seq)      AS success,
                max(bk.success)           AS succeeded_before,
                {vars}                    AS vars
         FROM {table} AS bk
         GROUP BY version
         ORDER BY version"
    );
    let rows = client.query(&sql).fetch_all::<AppliedRow>().await?;
    Ok(rows
        .into_iter()
        .map(|r| AppliedMigration {
            version: r.version,
            description: r.description,
            checksum: r.checksum,
            execution_ms: r.execution_ms,
            applied_at: r.applied_at,
            success: r.success,
            reverting: !r.success && r.succeeded_before,
            vars: r.vars.into_iter().collect(),
        })
        .collect())
}

/// Append a marker row recording an apply attempt or completion of
/// `migration`, with the variables it used.
///
/// `applied_at` falls back to its `DEFAULT now64(3)`; `seq` is a server-side
/// nanosecond counter so the latest marker per version is unambiguous. The
/// table must have the `vars` column (see [`upgrade_table`]).
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the insert fails.
pub async fn record(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
    migration: &Migration,
    execution_ms: u64,
    success: bool,
    vars: &BTreeMap<String, String>,
) -> Result<()> {
    let marker = Marker {
        version: migration.version,
        description: &migration.description,
        checksum: &migration.checksum,
        execution_ms,
        success,
        vars,
    };
    insert_marker(client, bookkeeping, &marker).await
}

/// Append a `success = false` marker for an applied version that is about to
/// be reverted, so it reads as dirty if the revert fails partway. The marker
/// repeats what `applied` recorded, so a later `force` restores it as applied.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the insert fails.
pub(crate) async fn record_revert_started(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
    applied: &AppliedMigration,
) -> Result<()> {
    let marker = Marker {
        version: applied.version,
        description: &applied.description,
        checksum: &applied.checksum,
        execution_ms: 0,
        success: false,
        vars: &applied.vars,
    };
    insert_marker(client, bookkeeping, &marker).await
}

/// One bookkeeping row to append.
struct Marker<'a> {
    version: i64,
    description: &'a str,
    checksum: &'a str,
    execution_ms: u64,
    success: bool,
    vars: &'a BTreeMap<String, String>,
}

async fn insert_marker(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
    marker: &Marker<'_>,
) -> Result<()> {
    let table = bookkeeping.qualified();
    let sql = format!(
        "INSERT INTO {table}
             (version, description, checksum, execution_ms, success, seq, vars)
         SELECT ?, ?, ?, ?, ?, toUnixTimestamp64Nano(now64(9)),
                CAST(mapFromArrays(?, ?), 'Map(String, String)')"
    );
    let (keys, values): (Vec<&str>, Vec<&str>) = marker
        .vars
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .unzip();
    client
        .query(&sql)
        .bind(marker.version)
        .bind(marker.description)
        .bind(marker.checksum)
        .bind(marker.execution_ms)
        .bind(marker.success)
        .bind(keys)
        .bind(values)
        .execute()
        .await?;
    Ok(())
}

/// Remove all bookkeeping rows for a version (used when reverting it).
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the delete fails.
pub async fn delete_version(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
    version: i64,
) -> Result<()> {
    let table = bookkeeping.qualified();
    let sql = format!("DELETE FROM {table} WHERE version = ?");
    client.query(&sql).bind(version).execute().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Bookkeeping;

    #[test]
    fn bookkeeping_new_accepts_bare_identifiers_and_qualifies() {
        let bk = Bookkeeping::new("_chum", "_chum_migrations").expect("valid identifiers");
        assert_eq!(bk.database(), "_chum");
        assert_eq!(bk.table(), "_chum_migrations");
        assert_eq!(bk.qualified(), "_chum._chum_migrations");
    }

    #[test]
    fn bookkeeping_new_rejects_injection_and_empty_identifiers() {
        // The database and table are interpolated into SQL, so anything that is
        // not a bare `[A-Za-z0-9_]` identifier must be rejected at construction.
        assert!(Bookkeeping::new("a.b", "t").is_err(), "dotted database");
        assert!(Bookkeeping::new("db", "a.b").is_err(), "dotted table");
        assert!(Bookkeeping::new("", "t").is_err(), "empty database");
        assert!(Bookkeeping::new("db", "").is_err(), "empty table");
        assert!(
            Bookkeeping::new("db; DROP DATABASE x", "t").is_err(),
            "injection attempt"
        );
        assert!(
            Bookkeeping::new("db", "t`").is_err(),
            "backtick in identifier"
        );
    }
}
