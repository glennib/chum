//! End-to-end tests against a live ClickHouse.
//!
//! These are `#[ignore]`d by default so the regular `cargo nextest run` /
//! `mise run ci` flow needs no infrastructure. Run them against a local
//! ClickHouse (`mise run db:up`) with:
//!
//! ```bash
//! cargo nextest run --run-ignored all -E 'test(full_lifecycle)'
//! # or: cargo test -- --ignored
//! ```
//!
//! The test embeds `tests/migrations/` (a small, generic schema that
//! deliberately uses DDL the splitter must handle: a materialized view, a
//! dictionary, and `ENGINE … ORDER BY`) via `include_dir`, exercising the
//! compile-time embedding front-end, and runs the full apply → idempotent
//! re-run → info → revert cycle against a throwaway database that it creates
//! and drops.

use chum::Migrator;
use chum::State;
use chum::source;
use include_dir::Dir;
use include_dir::include_dir;

/// The test-fixture migrations, embedded at compile time.
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/migrations");

const TEST_DB: &str = "chum_integration_test";
const TABLE: &str = "_chum_migrations";

fn client(database: &str) -> clickhouse::Client {
    let url = std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into());
    clickhouse::Client::default()
        .with_url(url)
        .with_database(database)
}

async fn exec(client: &clickhouse::Client, sql: &str) {
    client.query(sql).execute().await.expect("ddl");
}

#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn full_lifecycle_against_real_schema() {
    let admin = client("default");
    exec(&admin, &format!("DROP DATABASE IF EXISTS {TEST_DB}")).await;
    exec(&admin, &format!("CREATE DATABASE {TEST_DB}")).await;

    let db = client(TEST_DB);
    let migrator = Migrator::new(source::from_dir(&MIGRATIONS).expect("embedded migrations"));

    // The embedded source has exactly one version, with an up and a down.
    assert_eq!(migrator.info(&db, TABLE).await.expect("info").len(), 1);

    // Apply.
    let applied = migrator.run(&db, TABLE, None).await.expect("run");
    assert_eq!(applied.len(), 1, "one migration applied");

    // The recorded sequence reflects the apply, with the widened fields
    // populated (description, execution time, applied-at timestamp).
    let recorded = chum::backend::list_applied(&db, TABLE)
        .await
        .expect("list_applied");
    assert_eq!(recorded.len(), 1);
    let row = &recorded[0];
    assert!(row.success);
    assert_eq!(row.description, "initial schema");
    assert!(
        row.applied_at.timestamp() > 0,
        "applied_at should be a real timestamp"
    );

    // All six schema objects the migration creates exist. (Checking the named
    // set rather than a raw table count keeps the assertion robust against
    // engine-specific bookkeeping like a materialized view's inner table.)
    let count: u64 = db
        .query(&format!(
            "SELECT count() FROM system.tables WHERE database = '{TEST_DB}' AND name IN \
             ('events', 'users', 'events_daily', 'events_daily_mv', 'users_dict', 'active_users')"
        ))
        .fetch_one()
        .await
        .expect("count");
    assert_eq!(count, 6, "all six schema objects exist");

    // Idempotent re-run applies nothing.
    assert!(
        migrator
            .run(&db, TABLE, None)
            .await
            .expect("rerun")
            .is_empty()
    );

    // info reports it applied.
    let statuses = migrator.info(&db, TABLE).await.expect("info");
    assert_eq!(statuses[0].state, State::Applied);

    // Revert everything.
    let reverted = migrator.undo(&db, TABLE, i64::MIN).await.expect("undo");
    assert_eq!(reverted.len(), 1);

    // Only the bookkeeping table remains, and it has no rows.
    let count: u64 = db
        .query(&format!(
            "SELECT count() FROM system.tables WHERE database = '{TEST_DB}'"
        ))
        .fetch_one()
        .await
        .expect("count");
    assert_eq!(count, 1, "only _chum_migrations remains");
    assert_eq!(
        migrator.info(&db, TABLE).await.expect("info")[0].state,
        State::Pending
    );

    exec(&admin, &format!("DROP DATABASE IF EXISTS {TEST_DB}")).await;
}

/// `max_version` scans filenames only (no ClickHouse), so this runs in the
/// default test flow. It exercises the seam the `add` command uses to continue
/// a sequential scheme: a missing dir, an empty dir, and the highest version
/// with its on-disk padding width.
#[test]
fn max_version_scans_filenames() {
    use std::path::Path;

    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join("max_version");
    let _ = std::fs::remove_dir_all(&base);

    // A directory that does not exist reads as "no migrations".
    let missing = base.join("missing");
    assert_eq!(source::max_version(&missing).expect("missing dir"), None);

    // An empty directory reads as "no migrations".
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).expect("mkdir empty");
    assert_eq!(source::max_version(&empty).expect("empty dir"), None);

    // A sequential scheme: the max is 2, padded to width 4 on disk; non-matching
    // files are ignored.
    let seq = base.join("seq");
    std::fs::create_dir_all(&seq).expect("mkdir seq");
    for name in [
        "0001_first.up.sql",
        "0001_first.down.sql",
        "0002_second.up.sql",
        "0002_second.down.sql",
        "README.md",
    ] {
        std::fs::write(seq.join(name), "-- x\n").expect("write");
    }
    assert_eq!(source::max_version(&seq).expect("seq dir"), Some((2, 4)));

    let _ = std::fs::remove_dir_all(&base);
}
