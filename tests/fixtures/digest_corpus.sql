# 다이제스트 골든 코퍼스 (M1-6, [05 §3.2](../../docs/05-collector.md)).
#
# 한 줄에 문장 하나. `#` 로 시작하는 줄은 주석이고 빈 줄은 무시한다.
# 세미콜론은 붙이지 않는다 (`STATEMENT_DIGEST_TEXT()` 에 그대로 넘긴다).
#
# 검증하는 것: normalize(원문) == normalize(MySQL DIGEST_TEXT)
# 이 등가성이 3소스 조인 키(app_digest)의 근거다.

# ── 기본 형태 ────────────────────────────────────────────────────────────────
SELECT 1
SELECT * FROM orders WHERE id = 1
select * from orders where id = 1
SELECT   *   FROM   orders   WHERE   id  =  1
SELECT id, status, amount FROM orders WHERE status = 'PENDING' ORDER BY created_at DESC LIMIT 10
SELECT COUNT(*) FROM orders
SELECT COUNT(*) AS c, status FROM orders GROUP BY status HAVING COUNT(*) > 100
SELECT o.id, c.email FROM orders o JOIN customers c ON o.customer_id = c.id WHERE o.amount > 5000
SELECT o.id FROM orders o LEFT JOIN order_items i ON i.order_id = o.id WHERE i.sku IS NULL
SELECT DISTINCT region_code FROM customers

# ── 대소문자·백틱·스키마 한정 ─────────────────────────────────────────────────
SELECT `id`, `status` FROM `orders` WHERE `id` = 1
SELECT id FROM shop.orders WHERE id = 1
SELECT id FROM `shop`.`orders` WHERE id = 1
SELECT ID FROM ORDERS WHERE ID = 1
SELECT o.`amount` FROM `shop`.`orders` AS `o` WHERE `o`.`id` = 1

# ── 리터럴 종류 ──────────────────────────────────────────────────────────────
SELECT * FROM orders WHERE amount = 1234.56
SELECT * FROM orders WHERE amount = 1.5e3
SELECT * FROM orders WHERE amount = .5
SELECT * FROM orders WHERE id = 0x1F
SELECT * FROM orders WHERE id = b'1010'
SELECT * FROM orders WHERE memo = X'414243'
SELECT * FROM orders WHERE memo = _utf8mb4'한글'
SELECT * FROM orders WHERE memo = N'unicode'
SELECT * FROM orders WHERE created_at > DATE '2026-01-01'
SELECT * FROM orders WHERE created_at > TIMESTAMP '2026-01-01 00:00:00'
SELECT * FROM orders WHERE created_at > NOW() - INTERVAL 7 DAY
SELECT * FROM orders WHERE status = 'it''s'
SELECT * FROM orders WHERE memo IS NULL AND status <> 'PAID'
SELECT * FROM orders WHERE status IN ('PENDING')
SELECT * FROM customers WHERE name = '고객-1'
SELECT * FROM orders WHERE amount BETWEEN 100 AND 200

# ── IN 절 축약 ───────────────────────────────────────────────────────────────
SELECT * FROM orders WHERE id IN (1)
SELECT * FROM orders WHERE id IN (1,2,3)
SELECT * FROM orders WHERE id IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10)
SELECT * FROM orders WHERE status IN ('PENDING','PAID','SHIPPED')
SELECT * FROM orders WHERE id IN (SELECT order_id FROM order_items WHERE qty > 3)
SELECT * FROM orders WHERE id NOT IN (1,2,3)

# ── DML ──────────────────────────────────────────────────────────────────────
INSERT INTO lock_arena (id, val) VALUES (99, 1)
INSERT INTO lock_arena (id, val) VALUES (99,1),(98,2),(97,3)
INSERT INTO lock_arena (id, val) VALUES (99, 1) ON DUPLICATE KEY UPDATE val = val + 1
REPLACE INTO lock_arena (id, val) VALUES (99, 1)
UPDATE orders SET status = 'PAID' WHERE id = 1
UPDATE orders SET status = 'PAID', memo = 'done' WHERE customer_id = 1 AND status = 'PENDING'
DELETE FROM order_items WHERE order_id = 1
DELETE FROM order_items WHERE order_id IN (1,2,3) LIMIT 100

# ── 서브쿼리·CTE·윈도우 함수 ─────────────────────────────────────────────────
SELECT (SELECT COUNT(*) FROM order_items i WHERE i.order_id = o.id) AS n FROM orders o WHERE o.id = 1
WITH recent AS (SELECT * FROM orders WHERE created_at > NOW() - INTERVAL 1 DAY) SELECT COUNT(*) FROM recent
WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 10) SELECT SUM(n) FROM seq
SELECT id, amount, ROW_NUMBER() OVER (PARTITION BY status ORDER BY amount DESC) AS rn FROM orders
SELECT id, SUM(amount) OVER (ORDER BY created_at ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM orders
SELECT * FROM orders WHERE EXISTS (SELECT 1 FROM order_items i WHERE i.order_id = orders.id)
SELECT * FROM (SELECT id, amount FROM orders WHERE amount > 100) AS d WHERE d.id < 1000
SELECT id FROM orders UNION SELECT id FROM order_items
SELECT id FROM orders UNION ALL SELECT id FROM order_items

# ── 옵티마이저 힌트 (보존해야 한다) ──────────────────────────────────────────
SELECT /*+ MAX_EXECUTION_TIME(1000) */ * FROM orders WHERE id = 1
SELECT /*+ NO_ICP(orders) */ * FROM orders WHERE status = 'PAID'
SELECT /*+ JOIN_ORDER(o, c) */ o.id FROM orders o JOIN customers c ON o.customer_id = c.id
SELECT /*+ SET_VAR(sort_buffer_size = 16M) */ * FROM orders ORDER BY amount

# ── 주석 (제거해야 한다) ─────────────────────────────────────────────────────
/* leading block */ SELECT * FROM orders WHERE id = 1
SELECT /* inline */ * FROM orders WHERE id = 1
SELECT * FROM orders WHERE id = 1 -- trailing line
SELECT * FROM orders WHERE id = 1 # hash comment

# ── 함수·표현식 ──────────────────────────────────────────────────────────────
SELECT UPPER(status), LENGTH(memo), COALESCE(memo, 'none') FROM orders WHERE id = 1
SELECT CASE WHEN amount > 1000 THEN 'big' ELSE 'small' END FROM orders
SELECT DATE_FORMAT(created_at, '%Y-%m') AS m, SUM(amount) FROM orders GROUP BY m
SELECT JSON_EXTRACT('{"a":1}', '$.a')
SELECT CAST(amount AS CHAR) FROM orders WHERE id = 1
SELECT amount / 3, amount * 2, amount + 1, amount - 1, amount % 7 FROM orders WHERE id = 1
SELECT SLEEP(0)

# ── 유니코드·긴 문장 ─────────────────────────────────────────────────────────
SELECT * FROM customers WHERE name LIKE '%고객%' AND region_code = 'KR'
SELECT '이모지 🙂 포함' AS t

# ── M1-6 실측 후 추가: MySQL 이 정규화하는 동의어를 넓게 훑는다 ────────────────
SELECT TRUE, FALSE
SELECT 1 IS TRUE
SELECT NOT (1 = 1)
SELECT !(1 = 1)
SELECT 1 AND 1
SELECT 1 && 1
SELECT 1 OR 0
SELECT 1 XOR 0
SELECT o.id FROM orders o LEFT OUTER JOIN order_items i ON i.order_id = o.id
SELECT o.id FROM orders o LEFT JOIN order_items i ON i.order_id = o.id
SELECT o.id FROM orders o RIGHT OUTER JOIN order_items i ON i.order_id = o.id
SELECT o.id FROM orders o INNER JOIN order_items i ON i.order_id = o.id
SELECT o.id FROM orders o, customers c WHERE o.customer_id = c.id
SELECT o.id FROM orders o CROSS JOIN customers c
SELECT SUBSTRING(memo, 1, 3) FROM orders WHERE id = 1
SELECT SUBSTR(memo, 1, 3) FROM orders WHERE id = 1
SELECT MID(memo, 1, 3) FROM orders WHERE id = 1
SELECT * FROM customers WHERE name RLIKE '고객'
SELECT * FROM customers WHERE name REGEXP '고객'
SELECT CAST(1 AS DECIMAL(10,2))
SELECT CAST(1 AS SIGNED INTEGER)
SELECT CAST(1 AS SIGNED INT)
SELECT CAST(1 AS UNSIGNED)
SELECT CAST('a' AS BINARY)
SELECT BINARY 'a'
SELECT NOW() + INTERVAL 1 HOUR
SELECT NOW() + INTERVAL 1 MONTH
SELECT NOW() + INTERVAL 1 YEAR
SELECT NOW() + INTERVAL '1:1' MINUTE_SECOND
SELECT COUNT(DISTINCT status) FROM orders
SELECT GROUP_CONCAT(status SEPARATOR ',') FROM orders
SELECT GROUP_CONCAT(DISTINCT status ORDER BY status ASC) FROM orders
SELECT * FROM orders WHERE id = 1 FOR UPDATE
SELECT * FROM orders WHERE id = 1 FOR SHARE
SELECT * FROM orders WHERE id = 1 LOCK IN SHARE MODE
SELECT id FROM orders ORDER BY id ASC LIMIT 10
SELECT id FROM orders ORDER BY id DESC LIMIT 10 OFFSET 5
SELECT id FROM orders LIMIT 5, 10
SELECT * FROM orders USE INDEX (idx_orders_customer) WHERE customer_id = 1
SELECT * FROM orders IGNORE INDEX (idx_orders_customer) WHERE customer_id = 1
SELECT * FROM orders FORCE INDEX (idx_orders_customer) WHERE customer_id = 1
SELECT @@version
SELECT @@session.sql_mode
SELECT CURRENT_TIMESTAMP
SELECT CURRENT_TIMESTAMP()
SELECT LOCALTIMESTAMP
SELECT DATABASE()
SELECT SCHEMA()
SELECT * FROM orders WHERE memo LIKE '%a%' ESCAPE '!'
SELECT 1 UNION DISTINCT SELECT 2
INSERT IGNORE INTO lock_arena (id, val) VALUES (1, 1)
UPDATE LOW_PRIORITY orders SET status = 'X' WHERE id = 1
SELECT CONVERT('a' USING utf8mb4)
SELECT 'a' COLLATE utf8mb4_bin
SELECT POSITION('a' IN memo) FROM orders WHERE id = 1
SELECT TRIM(BOTH ' ' FROM memo) FROM orders WHERE id = 1
SELECT EXTRACT(YEAR FROM created_at) FROM orders WHERE id = 1
SELECT IF(amount > 100, 'big', 'small') FROM orders WHERE id = 1
SELECT IFNULL(memo, 'none') FROM orders WHERE id = 1
SELECT NULLIF(amount, 0) FROM orders WHERE id = 1
