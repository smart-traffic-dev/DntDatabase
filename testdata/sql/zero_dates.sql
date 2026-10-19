-- MySQL 5.7 and MariaDB only: legacy zero dates (needs a permissive sql_mode).
SET NAMES utf8mb4;
SET SESSION sql_mode = '';
USE dnt_fixture;

CREATE TABLE zero_dates (
  id int NOT NULL PRIMARY KEY,
  d date NULL,
  dt datetime NULL,
  ts timestamp NULL DEFAULT NULL
);

INSERT INTO zero_dates (id, d, dt, ts) VALUES
  (1, '0000-00-00', '0000-00-00 00:00:00', '0000-00-00 00:00:00'),
  (2, '2020-00-00', '2020-00-00 00:00:00', NULL),
  (3, '2024-02-29', '2024-02-29 12:30:45', '2024-02-29 12:30:45');
