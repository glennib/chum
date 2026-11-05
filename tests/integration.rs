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
//! The tests embed `tests/migrations/`, `tests/bootstrap_migrations/` and
//! `tests/question_mark_migrations/` (small, generic schemas) via
//! `include_dir`, exercising the compile-time embedding front-end, and run the
//! full apply → idempotent re-run → info → revert cycle.
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

/// Migrations whose SQL contains `?` and `??` in string literals and comments.
static QUESTION_MARK_MIGRATIONS: Dir =
    include_dir!("$CARGO_MANIFEST_DIR/tests/question_mark_migrations");

/// A self-bootstrapping migration used against a bookkeeping table created
/// with the schema chum used before it recorded variables.
static UPGRADE_MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/upgrade_migrations");

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

/// Migration SQL reaches ClickHouse verbatim: a `?` in a string literal or a
/// comment is not a bind placeholder, and `??` is not unescaped to `?`, in
/// both the up and the down direction.
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn question_marks_in_migration_sql_are_sent_verbatim() {
    let admin = client("default");
    let bookkeeping_db = "chum_it_question_mark_bk";
    let app_db = "chum_question_mark_app";

    exec(&admin, &format!("DROP DATABASE IF EXISTS {app_db}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {bookkeeping_db}")).await;

    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");
    let migrator =
        Migrator::new(source::from_dir(&QUESTION_MARK_MIGRATIONS).expect("embedded migrations"));

    let applied = migrator.run(&db, &bookkeeping, None).await.expect("run");
    assert_eq!(applied.len(), 1, "one migration applied");

    let stored: Vec<String> = admin
        .query_raw(&format!("SELECT s FROM {app_db}.t ORDER BY id"))
        .fetch_all()
        .await
        .expect("read stored values");
    assert_eq!(stored, ["a?b", "??"]);

    let reverted = migrator
        .undo(&db, &bookkeeping, i64::MIN)
        .await
        .expect("undo");
    assert_eq!(reverted.len(), 1);
    assert!(
        !database_exists(&admin, app_db).await,
        "revert dropped the app database"
    );

    exec(&admin, &format!("DROP DATABASE IF EXISTS {bookkeeping_db}")).await;
}

/// A bookkeeping table without the `vars` column, as older chum versions
/// created it, is read as having empty variables by read-only commands, which
/// leave it alone, and gains the column on the first writing command. Inserts
/// that don't name `vars`, as an older chum writes them, keep working.
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn writing_commands_add_the_vars_column_to_an_old_table() {
    let admin = client("default");
    let bookkeeping_db = "chum_it_upgrade_bk";
    let app_db = "chum_upgrade_app";

    exec(&admin, &format!("DROP DATABASE IF EXISTS {app_db}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {bookkeeping_db}")).await;
    exec(&admin, &format!("CREATE DATABASE {bookkeeping_db}")).await;
    exec(
        &admin,
        &format!(
            "CREATE TABLE {bookkeeping_db}.{TABLE}
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
        ),
    )
    .await;

    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");
    let migrator =
        Migrator::new(source::from_dir(&UPGRADE_MIGRATIONS).expect("embedded migrations"));

    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(statuses[0].state, State::Pending);
    assert!(statuses[0].vars.is_empty());
    migrator
        .plan(&db, &bookkeeping, None)
        .await
        .expect("dry run");
    assert!(
        !chum::backend::has_vars_column(&db, &bookkeeping)
            .await
            .expect("column lookup"),
        "read-only commands leave the table alone"
    );

    migrator.run(&db, &bookkeeping, None).await.expect("run");
    assert!(
        chum::backend::has_vars_column(&db, &bookkeeping)
            .await
            .expect("column lookup"),
        "run added the column"
    );
    let recorded = chum::backend::list_applied(&db, &bookkeeping)
        .await
        .expect("list_applied");
    assert!(recorded[0].success);
    assert!(recorded[0].vars.is_empty());

    // An older chum's insert names its columns and omits `vars`.
    exec(
        &admin,
        &format!(
            "INSERT INTO {bookkeeping_db}.{TABLE}
                 (version, description, checksum, execution_ms, success, seq)
             SELECT 1, 'upgrade', '{}', 0, true, toUnixTimestamp64Nano(now64(9))",
            recorded[0].checksum
        ),
    )
    .await;
    let recorded = chum::backend::list_applied(&db, &bookkeeping)
        .await
        .expect("list_applied after an old-style insert");
    assert!(recorded[0].success);
    assert!(recorded[0].vars.is_empty());

    migrator
        .undo(&db, &bookkeeping, i64::MIN)
        .await
        .expect("undo");
    exec(&admin, &format!("DROP DATABASE IF EXISTS {app_db}")).await;
    exec(&admin, &format!("DROP DATABASE IF EXISTS {bookkeeping_db}")).await;
}

/// Templated migrations whose database names come from `env.db` and
/// `env.log`, so each test can use its own databases.
#[cfg(feature = "template")]
static TEMPLATE_MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/tests/template_migrations");

/// A fresh bookkeeping database and log database (with a `reverted` table the
/// fixture's down migration writes to), and no app database.
#[cfg(feature = "template")]
async fn template_setup(
    admin: &clickhouse::Client,
    bookkeeping_db: &str,
    app_db: &str,
    log_db: &str,
) {
    for db in [app_db, bookkeeping_db, log_db] {
        exec(admin, &format!("DROP DATABASE IF EXISTS {db}")).await;
    }
    exec(admin, &format!("CREATE DATABASE {log_db}")).await;
    exec(
        admin,
        &format!(
            "CREATE TABLE {log_db}.reverted (cutoff String) ENGINE = MergeTree ORDER BY cutoff"
        ),
    )
    .await;
}

#[cfg(feature = "template")]
async fn template_teardown(admin: &clickhouse::Client, dbs: &[&str]) {
    for db in dbs {
        exec(admin, &format!("DROP DATABASE IF EXISTS {db}")).await;
    }
}

#[cfg(feature = "template")]
fn template_migrator(vars: chum::Vars) -> Migrator {
    Migrator::new(source::from_dir(&TEMPLATE_MIGRATIONS).expect("embedded migrations")).vars(vars)
}

#[cfg(feature = "template")]
async fn bookkeeping_rows(admin: &clickhouse::Client, bookkeeping_db: &str) -> u64 {
    admin
        .query(&format!("SELECT count() FROM {bookkeeping_db}.{TABLE}"))
        .fetch_one()
        .await
        .expect("count bookkeeping")
}

/// Apply templates, read their recorded variables back from `info` and with
/// the SQL documented for jobs outside chum, then revert with no variables
/// supplied: the down templates render from the recorded values, including a
/// value only a down migration uses.
#[cfg(feature = "template")]
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn templates_record_their_variables_and_revert_from_them() {
    use std::collections::BTreeMap;

    let admin = client("default");
    let (bookkeeping_db, app_db, log_db) = ("chum_it_tpl_bk", "chum_it_tpl_app", "chum_it_tpl_log");
    template_setup(&admin, bookkeeping_db, app_db, log_db).await;
    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");
    let cutoff = "2026-11-10 09:00:00";

    let migrator = template_migrator(
        chum::Vars::new()
            .with_env("db", app_db)
            .with_env("log", log_db)
            .with_env("tier", "prod")
            .with_this(2, "cutoff", cutoff),
    );
    // The pending templates list what they refer to.
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert!(statuses[1].template_vars.contains("this.cutoff"));

    let applied = migrator.run(&db, &bookkeeping, None).await.expect("run");
    assert_eq!(applied.len(), 2);
    assert_eq!(
        table_count(&admin, app_db).await,
        3,
        "events, prod_only, cutoffs"
    );

    let expected = |items: &[(&str, &str)]| -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(
        statuses[0].vars,
        expected(&[("env.db", app_db), ("env.tier", "prod")])
    );
    // `env.log` is recorded because the down migration uses it.
    assert_eq!(
        statuses[1].vars,
        expected(&[
            ("env.db", app_db),
            ("env.log", log_db),
            ("this.cutoff", cutoff)
        ])
    );
    assert!(statuses[1].template_vars.is_empty(), "applied");

    let recorded: Vec<String> = admin
        .query(&format!(
            "SELECT argMax(vars, seq)['this.cutoff'] AS cutoff
             FROM {bookkeeping_db}.{TABLE}
             WHERE version = 2
             GROUP BY version
             HAVING argMax(success, seq) AND mapContains(argMax(vars, seq), 'this.cutoff')"
        ))
        .fetch_all()
        .await
        .expect("documented query");
    assert_eq!(recorded, [cutoff]);

    // A supplied `this.*` value that differs from the recorded one is refused.
    let conflicting = template_migrator(chum::Vars::new().with_this(2, "cutoff", "other"));
    let err = conflicting
        .revert_plan(&db, &bookkeeping, i64::MIN)
        .await
        .expect_err("conflict");
    assert!(
        matches!(err, chum::ChumError::VarConflict { version: 2, .. }),
        "{err}"
    );

    let bare = template_migrator(chum::Vars::new());
    let reverted = bare.undo(&db, &bookkeeping, i64::MIN).await.expect("undo");
    assert_eq!(reverted, [2, 1]);
    assert!(
        !database_exists(&admin, app_db).await,
        "reverted from recorded env.db"
    );
    let logged: Vec<String> = admin
        .query(&format!("SELECT cutoff FROM {log_db}.reverted"))
        .fetch_all()
        .await
        .expect("read log");
    assert_eq!(logged, [cutoff], "the down rendered the recorded cutoff");
    assert_eq!(bookkeeping_rows(&admin, bookkeeping_db).await, 0);

    template_teardown(&admin, &[app_db, bookkeeping_db, log_db]).await;
}

/// A variable missing from the last pending migration aborts the run before
/// the first migration runs and before any marker is written.
#[cfg(feature = "template")]
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn a_missing_variable_aborts_before_anything_runs() {
    let admin = client("default");
    let (bookkeeping_db, app_db, log_db) = (
        "chum_it_tpl_missing_bk",
        "chum_it_tpl_missing_app",
        "chum_it_tpl_missing_log",
    );
    template_setup(&admin, bookkeeping_db, app_db, log_db).await;
    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");

    let migrator = template_migrator(
        chum::Vars::new()
            .with_env("db", app_db)
            .with_env("tier", "local"),
    );
    let err = migrator
        .run(&db, &bookkeeping, None)
        .await
        .expect_err("missing cutoff");
    let chum::ChumError::Render(failures) = &err else {
        panic!("expected a render error, got {err}");
    };
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].version, 2);
    assert_eq!(failures[0].missing, ["this.cutoff"]);
    assert!(
        !database_exists(&admin, app_db).await,
        "migration 1 did not run"
    );
    assert_eq!(
        bookkeeping_rows(&admin, bookkeeping_db).await,
        0,
        "no markers"
    );

    // `--target` applies the part that can render.
    migrator
        .run(&db, &bookkeeping, Some(1))
        .await
        .expect("run to 1");
    assert!(database_exists(&admin, app_db).await);

    template_teardown(&admin, &[app_db, bookkeeping_db, log_db]).await;
}

/// A guard that rejects its input leaves the version dirty with its variables
/// recorded; `force` refuses a different value for a recorded `this.*`, and
/// `reset` makes the version pending so a corrected run applies it.
#[cfg(feature = "template")]
#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn a_failed_guard_is_cleared_with_reset() {
    let admin = client("default");
    let (bookkeeping_db, app_db, log_db) = (
        "chum_it_tpl_guard_bk",
        "chum_it_tpl_guard_app",
        "chum_it_tpl_guard_log",
    );
    template_setup(&admin, bookkeeping_db, app_db, log_db).await;
    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");
    let base = chum::Vars::new()
        .with_env("db", app_db)
        .with_env("tier", "local");

    let migrator = template_migrator(base.clone().with_this(2, "cutoff", "bad"));
    let err = migrator
        .run(&db, &bookkeeping, None)
        .await
        .expect_err("guard");
    assert!(err.to_string().contains("cutoff rejected"), "{err}");
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(statuses[0].state, State::Applied);
    assert_eq!(statuses[1].state, State::Dirty);
    assert_eq!(statuses[1].vars["this.cutoff"], "bad");

    let err = template_migrator(base.clone().with_this(2, "cutoff", "good"))
        .force(&db, &bookkeeping, 2)
        .await
        .expect_err("conflict with the recorded value");
    assert!(matches!(err, chum::ChumError::VarConflict { .. }), "{err}");

    for (version, state) in [(1, State::Applied), (3, State::Pending)] {
        let err = migrator
            .reset(&db, &bookkeeping, version)
            .await
            .expect_err("not dirty");
        assert!(
            matches!(err, chum::ChumError::NotDirty { state: s, .. } if s == state),
            "{err}"
        );
    }
    migrator.reset(&db, &bookkeeping, 2).await.expect("reset");
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(statuses[1].state, State::Pending);

    let fixed = template_migrator(base.with_this(2, "cutoff", "good"));
    let applied = fixed.run(&db, &bookkeeping, None).await.expect("run");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].vars["this.cutoff"], "good");

    template_teardown(&admin, &[app_db, bookkeeping_db, log_db]).await;
}

#[tokio::test]
#[ignore = "requires a running ClickHouse (mise run db:up)"]
async fn failed_statements_leave_the_version_dirty() {
    use chum::ChumError;
    use chum::Direction;
    use chum::Migration;

    let admin = client("default");
    let (bookkeeping_db, app_db) = ("chum_it_failed_stmt_bk", "chum_it_failed_stmt_app");
    let drop_databases = async || {
        for db in [app_db, bookkeeping_db] {
            exec(&admin, &format!("DROP DATABASE IF EXISTS {db}")).await;
        }
    };
    drop_databases().await;
    let db = unpinned_client();
    let bookkeeping = Bookkeeping::new(bookkeeping_db, TABLE).expect("valid bookkeeping");
    let migration = |version, direction, sql: String| Migration::new(version, "m", direction, sql);
    let base = |down: &str| {
        vec![
            migration(
                1,
                Direction::Up,
                format!(
                    "CREATE DATABASE {app_db}; CREATE TABLE {app_db}.a (x UInt8) ENGINE = Memory"
                ),
            ),
            migration(1, Direction::Down, down.to_owned()),
        ]
    };
    let good_down = format!("DROP TABLE {app_db}.a; DROP DATABASE {app_db}");
    let bad_down = format!("DROP TABLE {app_db}.a; DROP TABLE {app_db}.missing");
    let mut failing_up = base(&good_down);
    failing_up.push(migration(
        2,
        Direction::Up,
        format!(
            "CREATE TABLE {app_db}.b (x UInt8) ENGINE = Memory;\n-- guard\nSELECT throwIf(1, \
             'boom')"
        ),
    ));

    // An apply that fails at its second statement.
    let err = Migrator::new(failing_up.clone())
        .run(&db, &bookkeeping, None)
        .await
        .expect_err("throwIf");
    assert!(
        matches!(
            &err,
            ChumError::StatementFailed { version: 2, direction: Direction::Up, index: 2, total: 2, statement, .. }
                if statement.contains("throwIf")
        ),
        "{err}"
    );
    let err = Migrator::new(failing_up)
        .run(&db, &bookkeeping, None)
        .await
        .expect_err("dirty");
    assert!(matches!(err, ChumError::Dirty(2)), "{err}");
    exec(&admin, &format!("DROP TABLE {app_db}.b")).await;
    let migrator = Migrator::new(base(&bad_down));
    migrator.reset(&db, &bookkeeping, 2).await.expect("reset");

    // A revert that fails at its second statement reads as dirty, not applied.
    let err = migrator
        .undo(&db, &bookkeeping, 0)
        .await
        .expect_err("missing table");
    assert!(
        matches!(
            err,
            ChumError::StatementFailed {
                version: 1,
                direction: Direction::Down,
                index: 2,
                total: 2,
                ..
            }
        ),
        "{err}"
    );
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(statuses[0].state, State::Dirty);
    let err = migrator
        .undo(&db, &bookkeeping, 0)
        .await
        .expect_err("dirty");
    assert!(matches!(err, ChumError::DirtyRevert(1)), "{err}");

    // Restoring the version by hand and forcing it makes it revertable again.
    exec(
        &admin,
        &format!("CREATE TABLE {app_db}.a (x UInt8) ENGINE = Memory"),
    )
    .await;
    migrator.force(&db, &bookkeeping, 1).await.expect("force");
    let migrator = Migrator::new(base(&good_down));
    assert_eq!(
        migrator.undo(&db, &bookkeeping, 0).await.expect("undo"),
        [1]
    );
    assert!(!database_exists(&admin, app_db).await);
    let statuses = migrator.info(&db, &bookkeeping).await.expect("info");
    assert_eq!(statuses[0].state, State::Pending);

    drop_databases().await;
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
