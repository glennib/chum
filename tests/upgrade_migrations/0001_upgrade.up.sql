CREATE DATABASE chum_upgrade_app;

CREATE TABLE chum_upgrade_app.t
(
    id UInt64
)
ENGINE = MergeTree
ORDER BY id;
