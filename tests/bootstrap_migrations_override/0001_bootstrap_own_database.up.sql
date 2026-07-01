-- Self-bootstrapping migration for the `--bookkeeping-database` override test.
-- Uses a distinct app database from tests/bootstrap_migrations/ so the two
-- e2e tests can run in parallel without colliding on `CREATE DATABASE`.
CREATE DATABASE chum_bootstrap_app_override;

CREATE TABLE chum_bootstrap_app_override.t
(
    x UInt8
)
ENGINE = MergeTree
ORDER BY x;
