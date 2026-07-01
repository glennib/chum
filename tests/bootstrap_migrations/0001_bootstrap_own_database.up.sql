-- A self-bootstrapping migration: it creates its own database and then a
-- fully-qualified table inside it. This is the motivating use case for the
-- dedicated bookkeeping database — chum must be able to run this without the
-- app database (`chum_bootstrap_app`) existing beforehand, connecting with the
-- always-present `default` session database and keeping its bookkeeping
-- fully-qualified to its own database.
CREATE DATABASE chum_bootstrap_app;

CREATE TABLE chum_bootstrap_app.t
(
    x UInt8
)
ENGINE = MergeTree
ORDER BY x;
