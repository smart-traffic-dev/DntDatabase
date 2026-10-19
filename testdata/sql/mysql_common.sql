-- Common fixture for MySQL 5.7 / 8.0 / MariaDB 10.6 / 11.4.
SET NAMES utf8mb4;
USE dnt_fixture;

CREATE TABLE customers (
  id int NOT NULL AUTO_INCREMENT PRIMARY KEY,
  name varchar(100) NOT NULL,
  email varchar(255) NOT NULL,
  created_at datetime(6) NULL,
  UNIQUE KEY uq_customers_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE orders (
  id int NOT NULL AUTO_INCREMENT PRIMARY KEY,
  customer_id int NOT NULL,
  status enum('new','paid','shipped','cancelled') NOT NULL DEFAULT 'new',
  total decimal(12,2) NOT NULL DEFAULT 0,
  created_at datetime(6) NULL,
  KEY idx_orders_customer_status (customer_id, status),
  CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers (id),
  CONSTRAINT chk_orders_total CHECK (total >= 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE `đơn_hàng` (
  id int NOT NULL AUTO_INCREMENT PRIMARY KEY,
  ten varchar(100) NOT NULL,
  ghi_chu text NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE weird_names (
  id int NOT NULL PRIMARY KEY,
  `we``ird"col` varchar(50) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE bin_blob (
  id int NOT NULL PRIMARY KEY,
  data blob NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE all_types (
  id int NOT NULL PRIMARY KEY,
  c_tinyint tinyint NULL,
  c_tinyint_u tinyint unsigned NULL,
  c_smallint smallint NULL,
  c_smallint_u smallint unsigned NULL,
  c_mediumint mediumint NULL,
  c_mediumint_u mediumint unsigned NULL,
  c_int int NULL,
  c_int_u int unsigned NULL,
  c_bigint bigint NULL,
  c_bigint_u bigint unsigned NULL,
  c_decimal decimal(40,10) NULL,
  c_float float NULL,
  c_double double NULL,
  c_bit bit(8) NULL,
  c_date date NULL,
  c_datetime datetime(6) NULL,
  c_timestamp timestamp NULL DEFAULT NULL,
  c_time time(6) NULL,
  c_year year NULL,
  c_char char(10) NULL,
  c_varchar varchar(255) NULL,
  c_text text NULL,
  c_binary binary(8) NULL,
  c_varbinary varbinary(16) NULL,
  c_blob blob NULL,
  c_enum enum('small','medium','large') NULL,
  c_set set('a','b','c') NULL,
  c_json json NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT INTO customers (id, name, email, created_at) VALUES
  (1, 'Nguyễn Văn A', 'a@example.com', '2024-01-01 10:00:00.000000'),
  (2, 'Trần Thị B', 'b@example.com', '2024-01-02 11:30:00.000000'),
  (3, 'Alice Smith', 'alice@example.com', NULL);

INSERT INTO orders (id, customer_id, status, total, created_at) VALUES
  (1, 1, 'new', 100.50, '2024-02-01 09:00:00.000000'),
  (2, 1, 'paid', 250.00, '2024-02-02 09:00:00.000000'),
  (3, 2, 'shipped', 75.25, '2024-02-03 09:00:00.000000');

INSERT INTO `đơn_hàng` (id, ten, ghi_chu) VALUES
  (1, 'Nguyễn Văn A', 'Giao hàng buổi sáng'),
  (2, 'Trần Thị B', NULL);

INSERT INTO weird_names (id, `we``ird"col`) VALUES (1, 'quote''d "value"');

INSERT INTO bin_blob (id, data) VALUES (1, REPEAT(UNHEX('00FF'), 500));

INSERT INTO all_types VALUES (
  1,
  -128, 255,
  -32768, 65535,
  -8388608, 16777215,
  -2147483648, 4294967295,
  9223372036854775807, 18446744073709551615,
  123456789012345678901234567890.1234567890,
  3.40282e38,
  1.7976931348623157e308,
  b'10101010',
  '9999-12-31',
  '9999-12-31 23:59:59.999999',
  '2038-01-19 03:14:07',
  '838:59:59.000000',
  2155,
  'Nguyễn',
  'Nguyễn Văn A',
  'Nguyễn Văn A — văn bản dài',
  X'0102030405060708',
  X'00FF10',
  X'DEADBEEF',
  'large',
  'a,c',
  '{"name": "Nguyễn Văn A", "tags": [1, 2, 3]}'
);

INSERT INTO all_types (id) VALUES (2);

CREATE VIEW v_customer_orders AS
  SELECT c.id AS customer_id, c.name AS customer_name, o.id AS order_id, o.status, o.total
  FROM customers c JOIN orders o ON o.customer_id = c.id;

CREATE FUNCTION f_add(a INT, b INT) RETURNS INT DETERMINISTIC RETURN a + b;

DELIMITER $$
CREATE PROCEDURE p_noop()
BEGIN
END$$
DELIMITER ;

CREATE TRIGGER trg_orders_bi BEFORE INSERT ON orders
  FOR EACH ROW SET NEW.created_at = IFNULL(NEW.created_at, NOW(6));

CREATE EVENT ev_cleanup
  ON SCHEDULE EVERY 1 DAY
  DISABLE
  DO DELETE FROM orders WHERE status = 'cancelled';

CREATE USER 'dnt_limited'@'%' IDENTIFIED BY 'dnt_test';
GRANT SELECT ON dnt_fixture.customers TO 'dnt_limited'@'%';
FLUSH PRIVILEGES;
