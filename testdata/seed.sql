-- Sample data: a small storefront.
--
-- Load it into a running server (the flag takes SQL text, not a path):
--     cargo run -p vsql-server -- --init "$(cat testdata/seed.sql)"
--
-- Or pipe it into the built-in console, which reads SQL from stdin:
--     cargo run -p vsql-cli < testdata/seed.sql
--
-- Timestamps are fixed rather than `now()`, so the same file doubles as the
-- fixture for crates/vsql-executor/tests/testdata.rs.

CREATE SCHEMA shop;

CREATE TABLE shop.customers (
    id        SERIAL PRIMARY KEY,
    email     TEXT NOT NULL UNIQUE,
    full_name TEXT NOT NULL,
    country   CHAR(2) NOT NULL DEFAULT 'US',
    signed_up TIMESTAMPTZ NOT NULL,
    credit    NUMERIC(10, 2) NOT NULL DEFAULT 0 CHECK (credit >= 0),
    is_active BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE shop.products (
    sku      TEXT PRIMARY KEY,
    title    TEXT NOT NULL,
    price    NUMERIC(10, 2) NOT NULL CHECK (price > 0),
    category TEXT NOT NULL,
    in_stock INT NOT NULL DEFAULT 0 CHECK (in_stock >= 0),
    added_on DATE NOT NULL
);

CREATE TABLE shop.orders (
    id          SERIAL PRIMARY KEY,
    customer_id INT NOT NULL REFERENCES shop.customers (id),
    placed_at   TIMESTAMPTZ NOT NULL,
    status      TEXT NOT NULL DEFAULT 'new',
    note        TEXT,
    CONSTRAINT orders_status_known
        CHECK (status IN ('new', 'paid', 'shipped', 'refunded'))
);

CREATE TABLE shop.order_lines (
    order_id   INT NOT NULL REFERENCES shop.orders (id),
    sku        TEXT NOT NULL REFERENCES shop.products (sku),
    quantity   INT NOT NULL CHECK (quantity > 0),
    unit_price NUMERIC(10, 2) NOT NULL
);

CREATE INDEX orders_by_customer ON shop.orders (customer_id);
CREATE INDEX orders_by_status ON shop.orders (status);
CREATE INDEX products_by_category ON shop.products (category);

-- One row per order that has lines, with the sum the application would
-- otherwise compute itself.
CREATE VIEW shop.order_totals AS
    SELECT o.id AS order_id,
           o.customer_id,
           o.status,
           sum(l.quantity * l.unit_price) AS total
      FROM shop.orders o
      JOIN shop.order_lines l ON l.order_id = o.id
     GROUP BY o.id, o.customer_id, o.status;

INSERT INTO shop.customers (email, full_name, country, signed_up, credit) VALUES
    ('ada@example.com',       'Ada Lovelace',      'GB', '2026-01-04 09:12:00+00', 120.50),
    ('grace@example.com',     'Grace Hopper',      'US', '2026-01-11 14:03:00+00',   0.00),
    ('alan@example.com',      'Alan Turing',       'GB', '2026-02-02 08:45:00+00',  40.00),
    ('katherine@example.com', 'Katherine Johnson', 'US', '2026-02-19 17:20:00+00',  15.75),
    ('edsger@example.com',    'Edsger Dijkstra',   'NL', '2026-03-07 11:05:00+00',   0.00),
    ('barbara@example.com',   'Barbara Liskov',    'US', '2026-03-28 06:30:00+00', 200.00),
    ('leslie@example.com',    'Leslie Lamport',    'US', '2026-04-15 21:40:00+00',   5.00),
    ('radia@example.com',     'Radia Perlman',     'US', '2026-05-02 10:10:00+00',   0.00);

INSERT INTO shop.products (sku, title, price, category, in_stock, added_on) VALUES
    ('KEY-001', 'Mechanical keyboard', 89.90, 'input',   12, '2025-11-02'),
    ('KEY-002', 'Split keyboard',     149.00, 'input',    4, '2026-01-15'),
    ('KEY-003', 'Keycap set',          39.50, 'input',    0, '2026-02-01'),
    ('MOU-001', 'Trackball mouse',     59.00, 'input',    7, '2025-12-10'),
    ('MOU-002', 'Vertical mouse',      49.90, 'input',    0, '2026-03-03'),
    ('MON-001', '27 inch monitor',    329.00, 'display',  5, '2025-10-21'),
    ('MON-002', '34 inch ultrawide',  649.00, 'display',  2, '2026-01-08'),
    ('MON-003', 'Portable monitor',   219.00, 'display',  9, '2026-02-14'),
    ('CAB-001', 'USB-C cable',         12.50, 'cable',  240, '2025-09-30'),
    ('CAB-002', 'Thunderbolt cable',   45.00, 'cable',   18, '2026-01-22'),
    ('CAB-003', 'DisplayPort cable',   18.75, 'cable',    0, '2026-03-19'),
    ('DES-001', 'Desk mat',            29.00, 'desk',    33, '2025-11-27');

INSERT INTO shop.orders (customer_id, placed_at, status, note) VALUES
    (1, '2026-05-04 10:00:00+00', 'shipped',  NULL),
    (1, '2026-05-19 16:22:00+00', 'shipped',  NULL),
    (2, '2026-05-21 09:05:00+00', 'paid',     NULL),
    (3, '2026-06-01 12:40:00+00', 'paid',     'gift wrap'),
    (4, '2026-06-07 18:15:00+00', 'shipped',  NULL),
    (6, '2026-06-11 07:50:00+00', 'paid',     NULL),
    (6, '2026-06-25 13:30:00+00', 'new',      NULL),
    (7, '2026-07-02 20:05:00+00', 'refunded', 'screen cracked'),
    (8, '2026-07-09 11:11:00+00', 'shipped',  NULL),
    (2, '2026-07-14 15:45:00+00', 'new',      NULL);

INSERT INTO shop.order_lines (order_id, sku, quantity, unit_price) VALUES
    ( 1, 'KEY-001', 1,  89.90), ( 1, 'CAB-001', 2, 12.50),
    ( 2, 'MON-001', 1, 329.00), ( 2, 'CAB-002', 1, 45.00),
    ( 3, 'MOU-001', 1,  59.00), ( 3, 'DES-001', 2, 29.00), ( 3, 'CAB-001', 1, 12.50),
    ( 4, 'KEY-002', 1, 149.00), ( 4, 'KEY-003', 1, 39.50),
    ( 5, 'MON-002', 1, 649.00),
    ( 6, 'KEY-001', 2,  89.90), ( 6, 'MOU-002', 1, 49.90), ( 6, 'CAB-003', 2, 18.75),
    ( 7, 'DES-001', 1,  29.00),
    ( 8, 'MON-003', 1, 219.00), ( 8, 'CAB-002', 1, 45.00),
    ( 9, 'CAB-001', 5,  12.50), ( 9, 'CAB-002', 2, 45.00),
    (10, 'KEY-003', 2,  39.50), (10, 'MOU-001', 1, 59.00);
