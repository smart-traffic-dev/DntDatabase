CREATE TABLE `orders` (
  `id` int NOT NULL AUTO_INCREMENT,
  `customer_id` int NOT NULL,
  `status` enum('new','paid','shipped','cancelled') COLLATE utf8mb4_unicode_ci NOT NULL DEFAULT 'new',
  `total` decimal(12,2) NOT NULL DEFAULT '0.00',
  `created_at` datetime(6) DEFAULT NULL,
  PRIMARY KEY (`id`),
  KEY `idx_orders_customer_status` (`customer_id`,`status`),
  CONSTRAINT `fk_orders_customer` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`),
  CONSTRAINT `chk_orders_total` CHECK ((`total` >= 0))
) ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci
