-- 로컬 개발용 샘플 스키마 (M0-10).
--
-- 목적은 예쁜 데이터가 아니라 **검증 시나리오를 만들 수 있는 형태**다:
--   - 인덱스가 없는 컬럼으로 조건을 걸어 풀스캔을 유발할 수 있다
--   - 조인이 느려질 만큼의 행수를 만들 수 있다
--   - 1024바이트를 넘는 SQL 을 자연스럽게 만들 수 있다 (긴 IN 절)
--   - 락 경합·데드락을 재현할 수 있다

SET NAMES utf8mb4;

CREATE TABLE IF NOT EXISTS customers (
  id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  email        VARCHAR(255)    NOT NULL,
  name         VARCHAR(100)    NOT NULL,
  -- 인덱스를 일부러 걸지 않는다. 풀스캔 시나리오의 조건 컬럼.
  region_code  VARCHAR(8)      NOT NULL,
  created_at   DATETIME(6)     NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  PRIMARY KEY (id),
  UNIQUE KEY uk_customers_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS orders (
  id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  customer_id  BIGINT UNSIGNED NOT NULL,
  status       VARCHAR(16)     NOT NULL,
  amount       DECIMAL(12,2)   NOT NULL,
  memo         VARCHAR(500)        NULL,
  created_at   DATETIME(6)     NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  PRIMARY KEY (id),
  KEY idx_orders_customer (customer_id)
  -- status 에 인덱스가 없다 → "상태로 필터" 쿼리가 풀스캔이 된다
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS order_items (
  id         BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id   BIGINT UNSIGNED NOT NULL,
  sku        VARCHAR(32)     NOT NULL,
  qty        INT             NOT NULL,
  unit_price DECIMAL(12,2)   NOT NULL,
  PRIMARY KEY (id),
  KEY idx_items_order (order_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- AUTO_INCREMENT 고갈 감시(M8-8) 검증용. 의도적으로 좁은 타입.
CREATE TABLE IF NOT EXISTS narrow_seq (
  id   SMALLINT UNSIGNED NOT NULL AUTO_INCREMENT,
  note VARCHAR(32) NOT NULL,
  PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 미사용 인덱스 감시(M8-7) 검증용. 아무 쿼리도 이 인덱스를 쓰지 않는다.
CREATE TABLE IF NOT EXISTS dusty (
  id   BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  a    INT NOT NULL,
  b    INT NOT NULL,
  PRIMARY KEY (id),
  KEY idx_dusty_never_used (a),
  KEY idx_dusty_redundant_a_b (a, b)   -- idx_dusty_never_used 를 포함한다 (중복 인덱스)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 락 경합·데드락 시나리오 전용. 행이 적어야 재현이 쉽다.
CREATE TABLE IF NOT EXISTS lock_arena (
  id  INT NOT NULL,
  val INT NOT NULL,
  PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO lock_arena (id, val) VALUES (1, 0), (2, 0), (3, 0), (4, 0);
