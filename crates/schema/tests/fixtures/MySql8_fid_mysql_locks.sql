CREATE TABLE `fid_mysql_locks` (
  `id` int NOT NULL,
  `a` int DEFAULT NULL,
  `body` text COLLATE utf8mb4_unicode_ci,
  PRIMARY KEY (`id`),
  KEY `k_a` (`a`),
  FULLTEXT KEY `ft_body` (`body`),
  CONSTRAINT `ck_enforced` CHECK ((`a` > 0)),
  CONSTRAINT `ck_not_enforced` CHECK ((`a` < 100)) /*!80016 NOT ENFORCED */
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci
