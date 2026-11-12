//! 락·트랜잭션 조회 — **운영과 같은 권한의 모니터링 계정**으로 실제 MySQL 에서 확인한다.
//!
//! 이 경로의 결함은 단위 테스트로 잡히지 않는다. 컬럼 이름, 권한(sys 뷰는 `EXECUTE` 가
//! 필요해 `ERROR 1356` 이었다), `UNION` 의 문자셋·타입 합치기는 서버만 판정한다.
//!
//! 블로커·대기자 세션은 **`ROOT`** 로 붙는다 — `reset_targets` 가 root 를 죽이지 않는다.
mod support;

use std::time::Duration;

use dbmon::mysql::{TargetMysql, Timeouts};
use dbmon_core::ports::TargetDb;
use mysql_async::Conn;
use mysql_async::prelude::*;
use support::*;

fn monitor_db(target: Target) -> TargetMysql {
    TargetMysql::connect(opts(target, MONITOR), Timeouts::default(), target.label)
        .expect("풀 조립은 네트워크를 타지 않는다")
}

async fn connection_id(conn: &mut Conn) -> u64 {
    conn.query_first("SELECT CONNECTION_ID()")
        .await
        .expect("CONNECTION_ID")
        .expect("한 행")
}

/// 조건이 설 때까지 기다린다. 대기는 다른 세션이 만들므로 즉시 보이지 않을 수 있다.
async fn eventually<T, F, Fut>(mut probe: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..50 {
        if let Some(v) = probe().await {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test]
async fn global_status_carries_innodb_lock_counters() {
    for target in [MYSQL84, MYSQL80] {
        let Some(root) = connect(target, ROOT).await else {
            return;
        };
        drop(root);
        // **가드를 잡는다.** 다른 테스트의 `reset_targets` 가 root 가 아닌 계정의 실행 중
        // 쿼리를 죽인다 — 모니터링 계정의 이 조회가 그 순간에 걸리면 실패한다(실제로 걸렸다).
        let _guard = exclusive_target(target).await;
        let db = monitor_db(target);
        let vars = db.global_status().await.expect("상태 조회");
        // **`UNION ALL` 이 문자셋(utf8mb3)·타입(BIGINT)이 다른 갈래를 합친다.** 드라이버를
        // 거친 값이 숫자로 읽혀야 비율을 낼 수 있다.
        for name in ["lock_deadlocks", "lock_timeouts"] {
            let v = vars
                .get(name)
                .unwrap_or_else(|| panic!("{}: {name} 이 없다", target.label));
            assert!(v.parse::<u64>().is_ok(), "{}: {name}={v}", target.label);
        }
        db.disconnect().await;
    }
}

#[tokio::test]
async fn lock_waits_name_the_idle_blocker_and_what_it_ran() {
    for target in [MYSQL84, MYSQL80] {
        let Some(mut setup) = connect(target, ROOT).await else {
            return;
        };
        let _guard = exclusive_target(target).await;
        setup
            .query_drop(
                "DROP TABLE IF EXISTS shop.it_lockwait;
                 CREATE TABLE shop.it_lockwait (id INT PRIMARY KEY, v INT NOT NULL);
                 INSERT INTO shop.it_lockwait VALUES (1, 0), (2, 0);",
            )
            .await
            .expect("준비");

        // 블로커: 행 1 을 잡고 **유휴**로 남는다.
        let mut blocker = connect(target, ROOT).await.expect("블로커");
        let blocker_id = connection_id(&mut blocker).await;
        blocker.query_drop("BEGIN").await.expect("BEGIN");
        blocker
            .query_drop("UPDATE shop.it_lockwait SET v = v + 1 WHERE id = 1")
            .await
            .expect("락을 잡는다");

        // 대기자: 같은 행을 기다린다.
        let mut waiter = connect(target, ROOT).await.expect("대기자");
        let waiter_id = connection_id(&mut waiter).await;
        let waiting = tokio::spawn(async move {
            let _ = waiter
                .query_drop(
                    "SET SESSION innodb_lock_wait_timeout = 10;
                     UPDATE shop.it_lockwait SET v = 10 WHERE id = 1",
                )
                .await;
            waiter
        });

        let db = monitor_db(target);
        let row = eventually(|| async {
            db.lock_waits(&[])
                .await
                .expect("락 대기 조회")
                .into_iter()
                .find(|w| w.waiting_pid == waiter_id)
        })
        .await
        .unwrap_or_else(|| panic!("{}: 대기가 보이지 않는다", target.label));

        assert_eq!(row.blocking_pid, Some(blocker_id), "{}", target.label);
        // **블로커는 유휴다** — 처리목록에서는 아무것도 하지 않는 세션으로 보인다.
        assert_eq!(
            row.blocking_command.as_deref(),
            Some("Sleep"),
            "{}",
            target.label
        );
        assert!(
            row.blocking_statements
                .iter()
                .any(|s| s.contains("SET v = v + 1 WHERE id = 1")),
            "{}: 락을 잡은 문장이 없다: {:?}",
            target.label,
            row.blocking_statements
        );
        assert_eq!(row.locked_schema.as_deref(), Some("shop"));
        assert_eq!(row.locked_table.as_deref(), Some("it_lockwait"));
        assert_eq!(row.locked_index.as_deref(), Some("PRIMARY"));
        assert!(
            row.lock_mode.as_deref().is_some_and(|m| m.contains('X')),
            "{:?}",
            row.lock_mode
        );
        assert!(
            row.waiting_sql
                .as_deref()
                .is_some_and(|s| s.contains("SET v = 10")),
            "{:?}",
            row.waiting_sql
        );

        // **대기자로 거를 수 있다** — 수집기가 슬로우 쿼리 후보로 묻는 모양이다.
        let only = db.lock_waits(&[waiter_id]).await.expect("필터");
        assert!(only.iter().all(|w| w.waiting_pid == waiter_id) && !only.is_empty());
        let none = db.lock_waits(&[blocker_id]).await.expect("필터");
        assert!(none.is_empty(), "블로커는 대기자가 아니다: {none:?}");

        // 장기·유휴 트랜잭션: 블로커는 유휴로, 대기자는 LOCK WAIT 로 보인다.
        let trx = db.transactions(0, 50).await.expect("트랜잭션 조회");
        let b = trx
            .iter()
            .find(|t| t.pid == Some(blocker_id))
            .expect("블로커 트랜잭션");
        assert_eq!(b.command.as_deref(), Some("Sleep"));
        assert_eq!(b.rows_modified, Some(1));
        assert!(
            b.statements.iter().any(|s| s.contains("SET v = v + 1")),
            "{:?}",
            b.statements
        );
        let w = trx
            .iter()
            .find(|t| t.pid == Some(waiter_id))
            .expect("대기자 트랜잭션");
        assert_eq!(w.state.as_deref(), Some("LOCK WAIT"));

        blocker.query_drop("ROLLBACK").await.expect("ROLLBACK");
        let waiter = waiting.await.expect("대기자 종료");
        drop(waiter);
        drop(blocker);
        setup
            .query_drop("DROP TABLE shop.it_lockwait")
            .await
            .expect("정리");
        db.disconnect().await;
    }
}

#[tokio::test]
async fn metadata_lock_waits_show_the_idle_holder_behind_a_ddl() {
    for target in [MYSQL84, MYSQL80] {
        let Some(mut setup) = connect(target, ROOT).await else {
            return;
        };
        let _guard = exclusive_target(target).await;
        setup
            .query_drop(
                "DROP TABLE IF EXISTS shop.it_mdlwait;
                 CREATE TABLE shop.it_mdlwait (id INT PRIMARY KEY);",
            )
            .await
            .expect("준비");

        // 보유자: 열린 트랜잭션에서 읽고 유휴로 남는다(SHARED_READ).
        let mut holder = connect(target, ROOT).await.expect("보유자");
        let holder_id = connection_id(&mut holder).await;
        holder.query_drop("BEGIN").await.expect("BEGIN");
        holder
            .query_drop("SELECT COUNT(*) FROM shop.it_mdlwait")
            .await
            .expect("읽기");

        // DDL 이 EXCLUSIVE 를 기다린다.
        let mut ddl = connect(target, ROOT).await.expect("DDL");
        let ddl_id = connection_id(&mut ddl).await;
        let altering = tokio::spawn(async move {
            let _ = ddl
                .query_drop(
                    "SET SESSION lock_wait_timeout = 10;
                     ALTER TABLE shop.it_mdlwait ADD COLUMN c INT",
                )
                .await;
            ddl
        });

        let db = monitor_db(target);
        let rows = eventually(|| async {
            let rows = db.metadata_lock_waits(200).await.expect("MDL 조회");
            rows.iter()
                .any(|r| r.pid == Some(ddl_id) && r.lock_status.as_deref() == Some("PENDING"))
                .then_some(rows)
        })
        .await
        .unwrap_or_else(|| panic!("{}: MDL 대기가 보이지 않는다", target.label));

        let held = rows
            .iter()
            .find(|r| r.pid == Some(holder_id))
            .expect("보유자가 보여야 한다 — 처리목록에는 Sleep 일 뿐이다");
        assert_eq!(held.lock_status.as_deref(), Some("GRANTED"));
        assert_eq!(held.lock_type.as_deref(), Some("SHARED_READ"));
        assert_eq!(held.command.as_deref(), Some("Sleep"));
        assert_eq!(held.table.as_deref(), Some("it_mdlwait"));
        let pending = rows
            .iter()
            .find(|r| r.pid == Some(ddl_id) && r.lock_status.as_deref() == Some("PENDING"))
            .expect("DDL");
        assert_eq!(pending.lock_type.as_deref(), Some("EXCLUSIVE"));

        holder.query_drop("ROLLBACK").await.expect("ROLLBACK");
        let ddl = altering.await.expect("DDL 종료");
        drop(ddl);
        drop(holder);
        setup
            .query_drop("DROP TABLE shop.it_mdlwait")
            .await
            .expect("정리");
        db.disconnect().await;
    }
}
