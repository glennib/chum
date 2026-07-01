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
//! The tests embed `tests/migrations/` and `tests/bootstrap_migrations/`
//! (small, generic schemas) via `include_dir`, exercising the compile-time
//! embedding front-end, and run the full apply → idempotent re-run → info →
//! revert cycle.
//!
//! The compose file (`compose.yaml`) publishes ClickHouse on host port **8124**
//! (not the default 8123) so it never collides with another project's
//! ClickHouse. Point the tests at it with `CLICKHOUSE_URL=http://localhost:8124`;
//! the default below matches.

use chum::Bookkeeping;
use chum::Migrator;
use chum::State;
use chum::source;
use include_dir::Dir;
use include_dir::include_dir;

/// The test-fixture migrations, embedded at compile time.
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/migrations");

/// A self-bootstrapping migration that creates its own database.
static BOOTSTRAP_MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/bootstrap_migrations");

/// A second self-bootstrapping migration using a *distinct* app database, so
/// the bookkeeping-override test can run in parallel with the bootstrap test
/// without colliding on `CREATE DATABASE` (the migrations deliberately omit
/// `IF NOT EXISTS`).
static BOOTSTRAP_MIGRATIONS_OVERRIDE: Dir =
    include_dir!("$CARGO_MANIFEST_DIR/tests/bootstrap_migrations_override");

const TEST_DB: &str = "chum_integration_test";
const TABLE: &str = "_chum_migrations";
/// The dedicated bookkeeping database used by these tests. Kept distinct from
/// the production default (`_chum`) so a stray production run is not disturbed.
const BOOKKEEPING_DB: &str = "chum_it_bookkeeping";

/// The app database a bootstrap migration creates for itself (matches the
/// fixture in `tests/bootstrap_migrations/`).
const BOOTSTRAP_APP_DB: &str = "chum_bootstrap_app";

fn default_url() -> String {
    std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8124".into())
}

/// A client pinned to a specific session default database.
fn client(database: &str) -> clickhouse::Client {
    clickhouse::Client::default()
        .with_url(default_url())
        .with_database(database)
}

/// A client with NO session default database pinned — it stays on the
/// `clickhouse` crate's built-in `default` database, which always exists. This
/// is how the CLI connects when `--database` is not given, so a migration can
/// bootstrap its own database.
fn unpinned_client() -> clickhouse::Client {
    clickhouse::Client::default().with_url(default_url())
}

async fn exec(client: &clickhouse::Client, sql: &str) {
    client.query(sql).execute().await.expect("ddl");
}

async fn table_count(client: &clickhouse::Client, database: &str) -> u64 {
    client
        .query("SELECT count() FROM system.tables WHERE database = ?")
        .bind(database)
        .fetch_one()
        .await
        .expect("count")
}

async fn database_exists(client: &clickhouse::Client, database: &str) -> bool {
    let n: u64 = client
        .query("SELECT count() FROM system.databases WHERE name = ?")
        .bind(database)
        .fetch_one()
        .await
        .expect("count databases");
    n == 1
}

#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn full_lifecycle_against_real_schema() {
    let admin = client("default");
    exec(&admin, &format!("DROP DATABASE IF EXISTS {TEST_DB}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {BOOKKEEPING_DB}")).await;
    exec(&admin, &format!("CREATE DATABASE {TEST_DB}")).await;

    let db = client(TEST_DB);
    let bookkeeping = Bookkeeping::new(BOOKKEEPING_DB, TABLE).expect("valid bookkeeping");
    let migrator = Migrator::new(source::from_dir(&MIGRATIONS).expect("embedded migrations"));

    // The embedded source has exactly one version, with an up and a down.
    assert_eq!(
        migrator.info(&db, &bookkeeping).await.expect("info").len(),
        1
    );

    // ensure_* created the dedicated bookkeeping database on the first call.
    assert!(
        database_exists(&admin, BOOKKEEPING_DB).await,
        "chum created the bookkeeping database"
    );

    // Apply.
    let applied = migrator.run(&db, &bookkeeping, None).await.expect("run");
    assert_eq!(applied.len(), 1, "one migration applied");

    // Bookkeeping was recorded in the dedicated database, not the app database.
    let recorded = chum::backend::list_applied(&db, &bookkeeping)
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

    // The bookkeeping table lives in BOOKKEEPING_DB, and the app database holds
    // only its own six schema objects (no bookkeeping table).
    let bookkeeping_rows: u64 = admin
        .query(&format!(
            "SELECT count() FROM {BOOKKEEPING_DB}.{TABLE} WHERE success"
        ))
        .fetch_one()
        .await
        .expect("count bookkeeping");
    assert_eq!(bookkeeping_rows, 1);

    let count: u64 = db
        .query(&format!(
            "SELECT count() FROM system.tables WHERE database = '{TEST_DB}' AND name IN \
             ('events', 'users', 'events_daily', 'events_daily_mv', 'users_dict', 'active_users')"
        ))
        .fetch_one()
        .await
        .expect("count");
    assert_eq!(count, 6, "all six schema objects exist");

    // The bookkeeping table is NOT in the app database.
    assert_eq!(
        table_count(&db, TEST_DB).await,
        6,
        "app database holds only the schema objects, no bookkeeping table"
    );

    // Idempotent re-run applies nothing.
    assert!(
        migrator
            .run(&db, &bookkeeping, None)
            .await
            .expect("rerun")
            .is_empty()
    );

    // info reports it applied.
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(statuses[0].state, State::Applied);

    // Revert everything.
    let reverted = migrator
        .undo(&db, &bookkeeping, i64::MIN)
        .await
        .expect("undo");
    assert_eq!(reverted.len(), 1);

    // The app database is now empty (bookkeeping never lived here); it is the
    // bookkeeping table, in its own database, that survives with no rows.
    assert_eq!(
        table_count(&db, TEST_DB).await,
        0,
        "app database is empty after revert"
    );
    let remaining: u64 = admin
        .query(&format!("SELECT count() FROM {BOOKKEEPING_DB}.{TABLE}"))
        .fetch_one()
        .await
        .expect("count bookkeeping after revert");
    assert_eq!(remaining, 0, "bookkeeping rows removed on revert");
    assert_eq!(
        migrator.info(&db, &bookkeeping).await.expect("info")[0].state,
        State::Pending
    );

    exec(&admin, &format!("DROP DATABASE IF EXISTS {TEST_DB}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {BOOKKEEPING_DB}")).await;
}

/// The motivating case: chum applies a migration that creates its **own**
/// database, without the app database pre-existing and without pinning it as
/// the session default. Bookkeeping lands in the dedicated `_chum`-style
/// database.
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn bootstrap_migration_creates_its_own_database() {
    let admin = client("default");
    // The default bookkeeping database name, matching the CLI default `_chum`.
    let bookkeeping_db = chum::DEFAULT_BOOKKEEPING_DATABASE;

    // Clean slate: neither the app database nor the bookkeeping database exists.
    exec(
        &admin,
        &format!("DROP DATABASE IF EXISTS {BOOTSTRAP_APP_DB}"),
    )
    .await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {bookkeeping_db}")).await;
    assert!(
        !database_exists(&admin, BOOTSTRAP_APP_DB).await,
        "app database must not pre-exist"
    );

    // Connect with NO app database pinned (session default stays `default`).
    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");
    let migrator =
        Migrator::new(source::from_dir(&BOOTSTRAP_MIGRATIONS).expect("embedded migrations"));

    // Apply: the migration's own `CREATE DATABASE` + fully-qualified table run,
    // and chum bootstraps its bookkeeping database along the way.
    let applied = migrator.run(&db, &bookkeeping, None).await.expect("run");
    assert_eq!(applied.len(), 1, "one bootstrap migration applied");

    // chum created its dedicated bookkeeping database (`_chum`).
    assert!(
        database_exists(&admin, bookkeeping_db).await,
        "chum created the default bookkeeping database"
    );
    // The migration created its own app database + table.
    assert!(
        database_exists(&admin, BOOTSTRAP_APP_DB).await,
        "migration created its own database"
    );
    assert_eq!(
        table_count(&admin, BOOTSTRAP_APP_DB).await,
        1,
        "app database holds its one table"
    );

    // Bookkeeping is recorded in `_chum._chum_migrations`.
    let bookkeeping_rows: u64 = admin
        .query(&format!(
            "SELECT count() FROM {bookkeeping_db}.{TABLE} WHERE success"
        ))
        .fetch_one()
        .await
        .expect("count bookkeeping");
    assert_eq!(bookkeeping_rows, 1);

    // A second run is idempotent — nothing re-applies.
    assert!(
        migrator
            .run(&db, &bookkeeping, None)
            .await
            .expect("rerun")
            .is_empty(),
        "second run is a no-op"
    );

    // Revert works under the new model: the down migration drops the whole app
    // database, and the bookkeeping row is removed.
    let reverted = migrator
        .undo(&db, &bookkeeping, i64::MIN)
        .await
        .expect("undo");
    assert_eq!(reverted.len(), 1);
    assert!(
        !database_exists(&admin, BOOTSTRAP_APP_DB).await,
        "revert dropped the app database"
    );
    let remaining: u64 = admin
        .query(&format!("SELECT count() FROM {bookkeeping_db}.{TABLE}"))
        .fetch_one()
        .await
        .expect("count bookkeeping after revert");
    assert_eq!(remaining, 0, "bookkeeping rows removed on revert");

    exec(&admin, &format!("DROP DATABASE IF EXISTS {bookkeeping_db}")).await;
}

/// `--bookkeeping-database` override lands bookkeeping in the chosen database.
/// Exercised at the library level via the `Bookkeeping` target.
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn bookkeeping_database_override_is_honored() {
    let admin = client("default");
    let override_db = "chum_it_override_bk";
    let app_db = "chum_bootstrap_app_override";

    exec(&admin, &format!("DROP DATABASE IF EXISTS {app_db}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {override_db}")).await;

    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(override_db, TABLE).expect("valid bookkeeping");
    let migrator = Migrator::new(
        source::from_dir(&BOOTSTRAP_MIGRATIONS_OVERRIDE).expect("embedded migrations"),
    );

    migrator.run(&db, &bookkeeping, None).await.expect("run");

    // Bookkeeping landed in the overridden database, not the default `_chum`.
    assert!(
        database_exists(&admin, override_db).await,
        "override bookkeeping database was created"
    );
    let rows: u64 = admin
        .query(&format!(
            "SELECT count() FROM {override_db}.{TABLE} WHERE success"
        ))
        .fetch_one()
        .await
        .expect("count override bookkeeping");
    assert_eq!(rows, 1);

    // Clean up.
    migrator
        .undo(&db, &bookkeeping, i64::MIN)
        .await
        .expect("undo");
    exec(&admin, &format!("DROP DATABASE IF EXISTS {override_db}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {app_db}")).await;
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
