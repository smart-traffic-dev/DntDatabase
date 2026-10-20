-- MariaDB only features.
SET NAMES utf8mb4;
USE dnt_fixture;

CREATE TABLE maria_json (
  id int NOT NULL PRIMARY KEY,
  doc longtext CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL,
  CONSTRAINT chk_maria_json_doc CHECK (doc IS NULL OR json_valid(doc))
) ENGINE=InnoDB;

INSERT INTO maria_json (id, doc) VALUES (1, '{"name": "Nguyễn Văn A"}'), (2, NULL);

CREATE SEQUENCE seq_inv START WITH 1000 INCREMENT BY 1;

CREATE TABLE versioned_items (
  id int NOT NULL PRIMARY KEY,
  label varchar(50) NOT NULL
) WITH SYSTEM VERSIONING;

INSERT INTO versioned_items (id, label) VALUES (1, 'first');

ALTER TABLE customers ADD CONSTRAINT chk_customers_email CHECK (email <> '');
