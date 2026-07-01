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

use crate::error::ChumError;
use crate::error::Result;
use crate::migration::AppliedMigration;

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
             seq          UInt64
         )
         ENGINE = MergeTree
         ORDER BY (version, seq)"
    );
    client.query(&ddl).execute().await?;
    Ok(())
}

/// Return the latest recorded state of each version, ordered ascending.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the query fails.
pub async fn list_applied(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
) -> Result<Vec<AppliedMigration>> {
    let table = bookkeeping.qualified();
    let sql = format!(
        "SELECT version,
                argMax(description, seq)  AS description,
                argMax(checksum, seq)     AS checksum,
                argMax(execution_ms, seq) AS execution_ms,
                argMax(applied_at, seq)   AS applied_at,
                argMax(success, seq)      AS success
         FROM {table}
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
        })
        .collect())
}

/// Append a marker row recording an apply attempt or completion.
///
/// `applied_at` falls back to its `DEFAULT now64(3)`; `seq` is a server-side
/// nanosecond counter so the latest marker per version is unambiguous.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the insert fails.
pub async fn record(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
    version: i64,
    description: &str,
    checksum: &str,
    execution_ms: u64,
    success: bool,
) -> Result<()> {
    let table = bookkeeping.qualified();
    let sql = format!(
        "INSERT INTO {table}
             (version, description, checksum, execution_ms, success, seq)
         SELECT ?, ?, ?, ?, ?, toUnixTimestamp64Nano(now64(9))"
    );
    client
        .query(&sql)
        .bind(version)
        .bind(description)
        .bind(checksum)
        .bind(execution_ms)
        .bind(success)
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
