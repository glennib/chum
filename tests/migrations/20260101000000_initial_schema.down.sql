-- Reverse of the initial schema. Drop the dependent objects (the views, the
-- materialized view, and the dictionary) before the tables they read from.

DROP VIEW active_users;
DROP DICTIONARY users_dict;
DROP VIEW events_daily_mv;
DROP TABLE events_daily;
DROP TABLE users;
DROP TABLE events;
