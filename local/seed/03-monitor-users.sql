-- 모니터링 계정 3종. [07 §2.3](../../docs/07-credentials-bootstrap.md) 의 권한 모드 A/B/C 를
-- 그대로 만들어 두고, M1-4 / OPEN-Q-06 을 로컬에서 실측한다.
--
-- 로컬에서는 IAM DB Auth 가 없으므로 비밀번호를 쓴다. **프로덕션 경로가 아니다.**
-- 이 파일의 비밀번호는 로컬 컨테이너 전용이며 코드에 하드코딩된 운영 비밀이 아니다.
--
-- 호스트를 `%` 로 두는 이유: 컨테이너 네트워크의 클라이언트 IP 가 매번 다르다.
-- 프로덕션에서는 `monitor_host` 로 좁힌다([07 §2.6](../../docs/07-credentials-bootstrap.md)).

-- ── 모드 B (least): 기본값. 스키마 화이트리스트 방식 ─────────────────────────
CREATE USER IF NOT EXISTS 'dbmon'@'%' IDENTIFIED BY 'dbmon-local-monitor';
GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO 'dbmon'@'%';
GRANT SELECT ON `performance_schema`.* TO 'dbmon'@'%';
GRANT SELECT ON `sys`.* TO 'dbmon'@'%';
-- 어드바이저가 스키마·카디널리티를 읽어야 하는 대상 스키마만 명시적으로 허용한다.
GRANT SELECT ON `shop`.* TO 'dbmon'@'%';

-- ── 모드 C (minimal): 데이터 SELECT 권한 없음 ────────────────────────────────
-- OPEN-Q-06 검증용. "데이터 SELECT 권한 없이 EXPLAIN FOR CONNECTION 이 되는가"
-- 사실이면 평상시 운영 데이터 읽기 권한을 0으로 둘 수 있다.
CREATE USER IF NOT EXISTS 'dbmon_minimal'@'%' IDENTIFIED BY 'dbmon-local-minimal';
GRANT PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO 'dbmon_minimal'@'%';
GRANT SELECT ON `performance_schema`.* TO 'dbmon_minimal'@'%';
GRANT SELECT ON `sys`.* TO 'dbmon_minimal'@'%';
-- shop 스키마 권한을 주지 않는다.

-- ── 권한 부족 계정: 자가진단이 실제로 잡아내는지 확인하는 음성 대조군 ──────────
-- PROCESS 가 없으면 다른 세션의 SQL 전문을 볼 수 없고 EXPLAIN FOR CONNECTION 이 거부된다
-- (에러 코드 1044/1045/1227 → PlanFailure::Denied).
CREATE USER IF NOT EXISTS 'dbmon_broken'@'%' IDENTIFIED BY 'dbmon-local-broken';
GRANT SELECT ON `performance_schema`.* TO 'dbmon_broken'@'%';

-- ── 부하 생성기 계정 ────────────────────────────────────────────────────────
-- 자기 제외(§10) 검증을 위해 우리 계정과 분리한다. `dbmon` 이 아닌 계정의 쿼리는
-- 워크로드 통계에 들어가야 하고, `dbmon` 의 쿼리는 별도 집계로 빠져야 한다.
CREATE USER IF NOT EXISTS 'loadgen'@'%' IDENTIFIED BY 'dbmon-local-loadgen';
GRANT SELECT, INSERT, UPDATE, DELETE ON `shop`.* TO 'loadgen'@'%';
