CREATE TABLE `fid_mysql_locks` (
  `id` int(11) NOT NULL,
  `a` int(11) DEFAULT NULL,
  `body` text DEFAULT NULL,
  PRIMARY KEY (`id`),
  KEY `k_a` (`a`),
  FULLTEXT KEY `ft_body` (`body`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci
