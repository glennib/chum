-- `?` in user SQL must reach ClickHouse verbatim, never as a bind placeholder.
-- The splitter drops leading comment lines, so the `?`-bearing comment sits
-- inside the INSERT where it is sent to the server.
CREATE DATABASE chum_question_mark_app;

CREATE TABLE chum_question_mark_app.t
(
    id UInt8,
    s  String
)
ENGINE = MergeTree
ORDER BY id;

INSERT INTO chum_question_mark_app.t VALUES
    -- Stored verbatim? `?` must not be bound and `??` must not become `?`.
    (1, 'a?b'),
    (2, '??');
