-- Cluster-level objects: a login role and a second database.
--
-- Load it the same way as seed.sql, for example:
--     cargo run -p vsql-server -- --superuser-password 'TopSecret!' --init "$(cat testdata/cluster.sql)"
--
-- Then connect to the new database as the new role. `analytics.sql` holds the
-- tables that belong in it:
--     psql -h 127.0.0.1 -p 5210 -U app -d analytics -f testdata/analytics.sql
--
-- `app` is an ordinary role: it may log in and read or write tables, and it is
-- refused when it tries to create a role or a database.

CREATE ROLE app LOGIN PASSWORD 'app-pass';

CREATE DATABASE analytics;
