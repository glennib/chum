-- Initial schema for chum's end-to-end test.
--
-- This file deliberately mixes DDL that sqlparser's ClickHouse dialect cannot
-- fully parse (CREATE DICTIONARY, the ENGINE … ORDER BY tail, a materialized
-- view) so the statement splitter — which only finds boundaries, never a full
-- AST — is exercised. Note the semicolon inside this comment; the splitter must
-- not treat it as a statement boundary.

CREATE TABLE events
(
    event_id   UUID,
    event_name LowCardinality(String),
    -- a string literal containing a semicolon: must not split here either
    note       String DEFAULT 'a;b;c',
    event_time DateTime64(3, 'UTC')
)
ENGINE = MergeTree
ORDER BY (event_name, event_time);

CREATE TABLE users
(
    user_id   UInt64,
    user_name String
)
ENGINE = MergeTree
ORDER BY user_id;

CREATE TABLE events_daily
(
    day        Date,
    event_name LowCardinality(String),
    cnt        UInt64
)
ENGINE = SummingMergeTree
ORDER BY (day, event_name);

CREATE MATERIALIZED VIEW events_daily_mv TO events_daily AS
SELECT
    toDate(event_time) AS day,
    event_name,
    count() AS cnt
FROM events
GROUP BY day, event_name;

CREATE DICTIONARY users_dict
(
    user_id   UInt64,
    user_name String
)
PRIMARY KEY user_id
SOURCE(CLICKHOUSE(TABLE 'users'))
LIFETIME(MIN 0 MAX 0)
LAYOUT(HASHED());

CREATE VIEW active_users AS
SELECT user_id, user_name FROM users;
