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
//! version reads as dirty until resolved with `chum reset` or `chum force`.
//!
//! # Upgrading a table an older chum created
//!
//! [`ensure_table`] creates the current schema, but `CREATE TABLE IF NOT
//! EXISTS` leaves a table an older chum created as it was. Each release so far
//! has only added columns:
//!
//! | chum           | columns added                                |
//! |----------------|----------------------------------------------|
//! | 0.1.0 – 0.2.5  | `version` … `seq` (the base schema)          |
//! | 0.3.0 – 0.5.0  | `vars`                                       |
//! | after 0.5.0    | `unset`, `records_unsets`                    |
//!
//! [`upgrade_table`] adds whichever of [`ADDED_COLUMNS`] are missing, so it
//! brings a table from any of these up to date in one `ALTER`; read-only
//! commands read a missing column as its default instead (see
//! [`list_applied`]). An older chum keeps writing to the upgraded table, since
//! it names its columns on insert and select; its rows get each added column's
//! default.
//!
//! `records_unsets` tells a row with no unsets apart from a row written before
//! unsets were recorded: it is `false` for every row an older chum wrote,
//! before or after the upgrade, and `true` for every row this chum writes.
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
use std::collections::BTreeSet;

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

/// Row shape read back from the bookkeeping table. Fields are read by
/// position, so the [`ADDED_COLUMNS`] come last, in their order.
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
    unset: Vec<String>,
    records_unsets: bool,
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

/// A column of the bookkeeping table.
struct Column {
    name: &'static str,
    /// The type, spelled as `system.columns` reports it.
    ty: &'static str,
    /// The `DEFAULT` expression, if the column has one.
    default: Option<&'static str>,
}

impl Column {
    const fn new(name: &'static str, ty: &'static str) -> Self {
        Self {
            name,
            ty,
            default: None,
        }
    }

    const fn with_default(self, default: &'static str) -> Self {
        Self {
            default: Some(default),
            ..self
        }
    }

    /// The column's definition in `CREATE TABLE` and `ADD COLUMN`.
    fn definition(&self) -> String {
        match self.default {
            Some(default) => format!("{} {} DEFAULT {default}", self.name, self.ty),
            None => format!("{} {}", self.name, self.ty),
        }
    }

    /// What a row reads for the column in a table that lacks it: the value an
    /// older chum's inserts get once the column is added, so a row reads the
    /// same before and after the upgrade.
    fn missing_value(&self) -> String {
        match self.default {
            Some(default) => format!("CAST({default}, '{}')", self.ty),
            None => format!("defaultValueOfTypeName('{}')", self.ty),
        }
    }
}

/// The columns of the table chum 0.1.0 created, which every bookkeeping table
/// has.
const BASE_COLUMNS: [Column; 7] = [
    Column::new("version", "Int64"),
    Column::new("description", "String"),
    Column::new("checksum", "String"),
    Column::new("execution_ms", "UInt64"),
    Column::new("success", "Bool"),
    Column::new("applied_at", "DateTime64(3, 'UTC')").with_default("now64(3)"),
    Column::new("seq", "UInt64"),
];

/// The columns added since chum 0.1.0, oldest first: `vars` (the recorded
/// variable values), `unset` (the `this.*` names a version was applied
/// without) and `records_unsets` (whether the row fills `unset`).
///
/// A new column goes at the end, with a `DEFAULT` that is right for the rows an
/// older chum keeps writing without it, and in [`AppliedRow`] at the same
/// position. [`ensure_table`], [`upgrade_statement`] and [`list_applied`]
/// follow from this list.
const ADDED_COLUMNS: [Column; 3] = [
    Column::new("vars", "Map(String, String)"),
    Column::new("unset", "Array(String)"),
    Column::new("records_unsets", "Bool").with_default("false"),
];

/// Create the bookkeeping table if it does not already exist.
///
/// Unlike user migrations (which deliberately omit `IF NOT EXISTS`), this is
/// chum's internal table and is created idempotently on every run. The table is
/// fully-qualified to the bookkeeping database, so [`ensure_database`] must run
/// first. A table an older chum created is left as it was; see
/// [`upgrade_table`].
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the DDL fails.
pub async fn ensure_table(client: &clickhouse::Client, bookkeeping: &Bookkeeping) -> Result<()> {
    let columns: Vec<String> = BASE_COLUMNS
        .iter()
        .chain(&ADDED_COLUMNS)
        .map(Column::definition)
        .collect();
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS {} ({}) ENGINE = MergeTree ORDER BY (version, seq)",
        bookkeeping.qualified(),
        columns.join(", ")
    );
    client.query(&ddl).execute().await?;
    Ok(())
}

/// The statement that adds every column in [`ADDED_COLUMNS`] to a bookkeeping
/// table created by an older chum. It is idempotent, so it upgrades a table
/// missing any subset of them.
#[must_use]
pub fn upgrade_statement(bookkeeping: &Bookkeeping) -> String {
    let adds: Vec<String> = ADDED_COLUMNS
        .iter()
        .map(|c| format!("ADD COLUMN IF NOT EXISTS {}", c.definition()))
        .collect();
    format!(
        "ALTER TABLE {} {}",
        bookkeeping.qualified(),
        adds.join(", ")
    )
}

/// Which of [`ADDED_COLUMNS`] the bookkeeping table has.
async fn added_columns(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
) -> Result<BTreeSet<String>> {
    let names: Vec<String> = client
        .query(
            "SELECT name FROM system.columns \
             WHERE database = ? AND table = ? AND name IN ?",
        )
        .bind(bookkeeping.database())
        .bind(bookkeeping.table())
        .bind(ADDED_COLUMNS.map(|c| c.name))
        .fetch_all()
        .await?;
    Ok(names.into_iter().collect())
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
    Ok(added_columns(client, bookkeeping).await?.contains("vars"))
}

/// Whether the bookkeeping table has the `unset` column.
///
/// # Errors
///
/// Returns [`ChumError::ClickHouse`] if the query fails.
pub async fn has_unset_column(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
) -> Result<bool> {
    Ok(added_columns(client, bookkeeping).await?.contains("unset"))
}

/// Add the columns in [`ADDED_COLUMNS`] to a bookkeeping table that lacks any
/// of them, returning the statement that was run, or `None` when all were
/// already there.
///
/// Only commands that write bookkeeping rows call this; read-only commands
/// read a missing column as empty (see [`list_applied`]), so inspecting a
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
    let present = added_columns(client, bookkeeping).await?;
    if ADDED_COLUMNS.iter().all(|c| present.contains(c.name)) {
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
/// A table without the `vars` or `unset` column reads every version's values
/// or unset names as empty, and one without `records_unsets` reads it as
/// `false`, the value an older chum's rows get. An unset name reads as a
/// `None` value.
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
    let present = added_columns(client, bookkeeping).await?;
    let added: Vec<String> = ADDED_COLUMNS
        .iter()
        .map(|c| {
            let value = if present.contains(c.name) {
                format!("argMax({}, seq)", c.name)
            } else {
                c.missing_value()
            };
            format!("{value} AS {}", c.name)
        })
        .collect();
    let added = added.join(",\n                ");
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
                {added}
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
            vars: r
                .vars
                .into_iter()
                .map(|(k, v)| (k, Some(v)))
                .chain(r.unset.into_iter().map(|k| (k, None)))
                .collect(),
            records_unsets: r.records_unsets,
        })
        .collect())
}

/// Append a marker row recording an apply attempt or completion of
/// `migration`, with the variables it used: values in `vars`, and the names
/// it was applied without (`None`) in `unset`.
///
/// `applied_at` falls back to its `DEFAULT now64(3)`; `seq` is a server-side
/// nanosecond counter so the latest marker per version is unambiguous. The
/// table must have every column in [`ADDED_COLUMNS`] (see [`upgrade_table`]).
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
    vars: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    let marker = Marker {
        version: migration.version,
        description: &migration.description,
        checksum: &migration.checksum,
        execution_ms,
        success,
        vars,
        records_unsets: true,
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
        records_unsets: applied.records_unsets,
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
    vars: &'a BTreeMap<String, Option<String>>,
    /// Whether `vars` lists every name the version was applied without; see
    /// [`AppliedMigration::records_unsets`].
    records_unsets: bool,
}

async fn insert_marker(
    client: &clickhouse::Client,
    bookkeeping: &Bookkeeping,
    marker: &Marker<'_>,
) -> Result<()> {
    let table = bookkeeping.qualified();
    let sql = format!(
        "INSERT INTO {table}
             (version, description, checksum, execution_ms, success, seq, vars, unset,
              records_unsets)
         SELECT ?, ?, ?, ?, ?, toUnixTimestamp64Nano(now64(9)),
                CAST(mapFromArrays(?, ?), 'Map(String, String)'),
                CAST(?, 'Array(String)'), ?"
    );
    let (keys, values): (Vec<&str>, Vec<&str>) = marker
        .vars
        .iter()
        .filter_map(|(k, v)| Some((k.as_str(), v.as_deref()?)))
        .unzip();
    let unset: Vec<&str> = marker
        .vars
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.as_str())
        .collect();
    client
        .query(&sql)
        .bind(marker.version)
        .bind(marker.description)
        .bind(marker.checksum)
        .bind(marker.execution_ms)
        .bind(marker.success)
        .bind(keys)
        .bind(values)
        .bind(unset)
        .bind(marker.records_unsets)
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
