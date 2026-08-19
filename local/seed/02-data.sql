-- 샘플 데이터. **양보다 형태가 중요하다.**
--
-- 2초 넘는 쿼리를 만들기 위해 수백만 행을 넣지 않는다. 컨테이너 초기화가 느려지고
-- 그건 개발 반복 속도를 직접 깎는다. 대신 부하 생성기가 **인덱스가 없는 조건 + 조인**으로
-- 느린 플랜을 만든다 — 실제 튜닝 대상과 같은 형태다.

SET SESSION cte_max_recursion_depth = 1000000;

INSERT INTO customers (email, name, region_code)
WITH RECURSIVE seq(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 20000
)
SELECT CONCAT('user', n, '@example.com'),
       CONCAT('고객-', n),
       ELT(1 + (n % 5), 'KR', 'JP', 'US', 'SG', 'DE')
FROM seq;

INSERT INTO orders (customer_id, status, amount, memo, created_at)
WITH RECURSIVE seq(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 60000
)
SELECT 1 + (n % 20000),
       ELT(1 + (n % 4), 'PENDING', 'PAID', 'SHIPPED', 'CANCELLED'),
       ROUND(1000 + RAND(n) * 90000, 2),
       CASE WHEN n % 7 = 0 THEN CONCAT('메모 ', n) ELSE NULL END,
       NOW(6) - INTERVAL (n % 90) DAY
FROM seq;

INSERT INTO order_items (order_id, sku, qty, unit_price)
WITH RECURSIVE seq(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 120000
)
SELECT 1 + (n % 60000),
       CONCAT('SKU-', LPAD(n % 5000, 6, '0')),
       1 + (n % 5),
       ROUND(100 + RAND(n) * 5000, 2)
FROM seq;

INSERT INTO dusty (a, b)
WITH RECURSIVE seq(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 5000
)
SELECT n % 100, n % 37 FROM seq;

ANALYZE TABLE customers, orders, order_items, dusty;
