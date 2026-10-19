-- MySQL 8.0 only features.
SET NAMES utf8mb4;
USE dnt_fixture;

ALTER TABLE customers ADD CONSTRAINT chk_customers_email CHECK (email <> '');

CREATE INDEX idx_customers_email_lower ON customers ((lower(email)));

ALTER TABLE customers ADD COLUMN secret_note varchar(50) INVISIBLE NULL;
