-- The warehouse that lives in the `analytics` database created by cluster.sql.
-- Sessions are bound to one database, so connect to it first:
--     cargo run -p vsql-cli -- -d analytics < testdata/analytics.sql

CREATE TABLE events (
    id      BIGINT PRIMARY KEY,
    day     DATE NOT NULL,
    source  TEXT NOT NULL,
    kind    TEXT NOT NULL,
    payload JSONB,
    seen_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX events_by_day ON events (day);

-- A composite primary key, which is how a rollup normally keys its rows.
CREATE TABLE daily_rollup (
    day         DATE NOT NULL,
    source      TEXT NOT NULL,
    event_count INT NOT NULL CHECK (event_count >= 0),
    PRIMARY KEY (day, source)
);

CREATE VIEW events_by_source AS
    SELECT day,
           source,
           count(*) AS events
      FROM events
     GROUP BY day, source;

INSERT INTO events (id, day, source, kind, payload, seen_at) VALUES
    (101, '2026-07-01', 'web',     'visit',  '{"path": "/", "ms": 31}',            '2026-07-01 00:04:12+00'),
    (102, '2026-07-01', 'web',     'visit',  '{"path": "/pricing", "ms": 74}',      '2026-07-01 00:11:03+00'),
    (103, '2026-07-01', 'web',     'signup', '{"plan": "pro"}',                    '2026-07-01 00:26:41+00'),
    (104, '2026-07-01', 'mobile',  'visit',  '{"os": "ios", "ms": 120}',            '2026-07-01 01:02:19+00'),
    (105, '2026-07-02', 'web',     'visit',  '{"path": "/docs", "ms": 45}',         '2026-07-02 08:30:00+00'),
    (106, '2026-07-02', 'web',     'error',  '{"status": 500, "path": "/checkout"}', '2026-07-02 09:14:22+00'),
    (107, '2026-07-02', 'mobile',  'visit',  '{"os": "android", "ms": 210}',        '2026-07-02 10:05:37+00'),
    (108, '2026-07-02', 'mobile',  'signup', '{"plan": "trial"}',                  '2026-07-02 10:44:08+00'),
    (109, '2026-07-03', 'web',     'visit',  '{"path": "/", "ms": 28}',             '2026-07-03 07:20:15+00'),
    (110, '2026-07-03', 'web',     'signup', '{"plan": "pro"}',                    '2026-07-03 07:58:44+00'),
    (111, '2026-07-03', 'partner', 'import', '{"rows": 1830}',                     '2026-07-03 12:00:00+00'),
    (112, '2026-07-03', 'partner', 'error',  '{"status": 409, "rows": 12}',         '2026-07-03 12:04:51+00'),
    (113, '2026-07-04', 'web',     'visit',  '{"path": "/blog", "ms": 52}',         '2026-07-04 15:41:09+00'),
    (114, '2026-07-04', 'mobile',  'error',  '{"status": 503, "os": "ios"}',        '2026-07-04 16:22:30+00'),
    (115, '2026-07-04', 'partner', 'import', '{"rows": 940}',                      '2026-07-04 18:00:00+00'),
    (116, '2026-07-04', 'web',     'visit',  '{"path": "/", "ms": 33}',             '2026-07-04 23:58:12+00');

INSERT INTO daily_rollup (day, source, event_count) VALUES
    ('2026-07-01', 'web',     3),
    ('2026-07-01', 'mobile',  1),
    ('2026-07-02', 'web',     2),
    ('2026-07-02', 'mobile',  2),
    ('2026-07-03', 'web',     2),
    ('2026-07-03', 'partner', 2),
    ('2026-07-04', 'web',     2),
    ('2026-07-04', 'mobile',  1),
    ('2026-07-04', 'partner', 1);
