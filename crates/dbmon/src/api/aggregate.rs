//! 조회 시점 집계 — 다이제스트 표와 월간 통계.
//!
//! # 왜 뷰 위에서 집계하는가
//!
//! 다이제스트 표는 **대표 SQL 을 보여준다.** 도메인 레코드에서 바로 집계하면 그
//! SQL 이 [`super::view::SlowQueryView`] 의 리터럴 통제를 건너뛴다 —
//! [08 §6.1](../../../../docs/08-security-auth.md) 이 "직렬화 계층에서 강제하므로
//! 빠뜨릴 수 없다" 고 보는 근거가 바로 그 통제다. 그래서 이 모듈은 **이미 통제를
//! 지난 뷰**만 입력으로 받는다. 권한 없는 사용자에게는 대표 SQL 이 `None` 이 된다.
//!
//! # 왜 조회 시점에 계산하는가
//!
//! 규정([04](../../../../docs/04-data-model.md))은 다이제스트 롤업을 **수집 시점에**
//! 스냅샷으로 쌓는 설계다(M12). 그 배선이 아직 없고, 저장된 실행 레코드만으로도
//! 같은 표를 만들 수 있다.
//!
//! ⚠ **천장이 있다.** 조회 구간의 레코드를 전부 읽어 메모리에서 접는다. 그래서
//! 호출부가 상한을 주고, 상한에 걸리면 응답이 `truncated=true` 로 **말한다** —
//! 조용히 자르면 "그만큼만 실행됐다" 로 읽힌다. 월 단위 대량 집계가 필요해지면
//! 그때 M12 롤업으로 바꾼다.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::view::SlowQueryView;

/// `app_digest` 하나로 접은 행. 참조 대시보드의 `Slow Query Digest` 표와 같은 열.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DigestRow {
    pub instance_id: String,
    pub app_digest: String,
    /// 대표 SQL. **권한이 없으면 `None`** — 뷰에서 이미 가려진 값을 쓴다.
    pub digest_query: Option<String>,
    /// 이 다이제스트를 실행한 DB 사용자들.
    pub users: Vec<String>,
    pub statement_type: String,
    pub schema_name: Option<String>,
    pub exec_count: u64,
    pub total_time_ms: i64,
    pub avg_time_ms: f64,
    pub max_time_ms: i64,
    /// 평균 조사 행. **행 정보가 있는 실행만 분모에 넣는다** — 실시간 캡처는 행
    /// 카운터를 얻지 못하므로 `None` 이 섞인다([19 §G2]).
    pub avg_rows_examined: Option<f64>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// 인스턴스별 월간 통계. 참조 대시보드의 `SQL Statistics` 표와 같은 열.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstanceStats {
    pub instance_id: String,
    pub unique_digest_count: u64,
    /// 관측된 슬로우 쿼리 실행 수.
    ///
    /// 참조 구현은 CloudWatch 다이제스트의 `execution_count` 를 따로 들고 있었지만,
    /// 여기서는 **레코드 하나가 실행 하나**이므로 두 값이 같다. 둘 다 내보내되
    /// 같다는 사실을 이 주석으로 남긴다.
    pub total_slow_query_count: u64,
    pub total_execution_count: u64,
    pub total_execution_time_ms: i64,
    pub avg_execution_time_ms: f64,
    pub max_execution_time_ms: i64,
    pub total_rows_examined: u64,
    pub read_query_count: u64,
    pub write_query_count: u64,
    pub ddl_query_count: u64,
    pub commit_query_count: u64,
    pub other_query_count: u64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// 사용자별 통계. 참조 대시보드의 `User Statistics` 표와 같은 열.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserStats {
    pub instance_id: String,
    pub user: String,
    pub total_queries: u64,
    pub unique_digest_count: u64,
    pub total_exec_time_ms: i64,
    pub avg_execution_time_ms: f64,
    pub max_execution_time_ms: i64,
    pub read_query_count: u64,
    pub write_query_count: u64,
    pub ddl_query_count: u64,
    pub commit_query_count: u64,
    pub other_query_count: u64,
}

/// 문장 유형 분류. 참조 대시보드가 읽기/쓰기/DDL/Commit 으로 세던 것과 같다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Ddl,
    Commit,
    Other,
}

/// `statement_type` 은 `SlowQueryView` 가 소문자로 직렬화한 값이다.
///
/// `commit` 을 따로 세려면 문장을 봐야 한다 — `StatementType::Other` 안에 섞여
/// 있고, 트랜잭션 커밋이 슬로우 쿼리로 잡히는 것은 **락 대기**를 뜻하므로
/// 운영자에게 다른 신호다. SQL 이 가려졌으면 `Other` 로 남긴다(추측하지 않는다).
fn classify(statement_type: &str, sql: Option<&str>) -> Kind {
    match statement_type {
        "select" => Kind::Read,
        "insert" | "update" | "delete" | "replace" => Kind::Write,
        "ddl" => Kind::Ddl,
        _ => match sql {
            Some(text) if text.trim_start().len() >= 6 => {
                let head = &text.trim_start()[..6];
                if head.eq_ignore_ascii_case("commit") {
                    Kind::Commit
                } else {
                    Kind::Other
                }
            }
            _ => Kind::Other,
        },
    }
}

/// 누산기. 다이제스트·인스턴스·사용자 집계가 같은 셈을 쓴다.
#[derive(Debug, Default)]
struct Counters {
    count: u64,
    total_time_ms: i64,
    max_time_ms: i64,
    rows_examined_sum: u64,
    /// 행 정보가 **있는** 실행 수. 평균의 분모다.
    rows_examined_n: u64,
    read: u64,
    write: u64,
    ddl: u64,
    commit: u64,
    other: u64,
    first_seen_ms: i64,
    last_seen_ms: i64,
}

impl Counters {
    fn add(&mut self, view: &SlowQueryView) {
        if self.count == 0 {
            self.first_seen_ms = view.started_at_ms;
            self.last_seen_ms = view.started_at_ms;
        } else {
            self.first_seen_ms = self.first_seen_ms.min(view.started_at_ms);
            self.last_seen_ms = self.last_seen_ms.max(view.started_at_ms);
        }
        self.count += 1;
        self.total_time_ms += view.duration_ms;
        self.max_time_ms = self.max_time_ms.max(view.duration_ms);
        if let Some(rows) = view.rows_examined {
            self.rows_examined_sum += rows;
            self.rows_examined_n += 1;
        }
        match classify(&view.statement_type, view.sql_text.as_deref()) {
            Kind::Read => self.read += 1,
            Kind::Write => self.write += 1,
            Kind::Ddl => self.ddl += 1,
            Kind::Commit => self.commit += 1,
            Kind::Other => self.other += 1,
        }
    }

    fn avg_time_ms(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.total_time_ms as f64 / self.count as f64
    }

    /// 행 정보가 하나도 없으면 **`None`** 이다. `0` 으로 내면 "행을 안 읽었다" 가 된다.
    fn avg_rows_examined(&self) -> Option<f64> {
        if self.rows_examined_n == 0 {
            return None;
        }
        Some(self.rows_examined_sum as f64 / self.rows_examined_n as f64)
    }
}

/// 다이제스트 하나를 접는 동안의 상태. 튜플로 두면 필드가 뭔지 세어야 한다.
#[derive(Debug)]
struct DigestAcc<'a> {
    counters: Counters,
    users: BTreeSet<&'a str>,
    /// 대표 SQL. 가려지지 않은 첫 값을 쓴다.
    sql: Option<&'a str>,
    statement_type: &'a str,
    schema_name: Option<&'a str>,
}

/// `(instance, app_digest)` 로 접는다. 실행 수 내림차순, 동수면 총 시간 내림차순.
pub fn digest_rows(views: &[SlowQueryView]) -> Vec<DigestRow> {
    let mut acc: BTreeMap<(&str, &str), DigestAcc<'_>> = BTreeMap::new();

    for view in views {
        let entry = acc
            .entry((view.instance_id.as_str(), view.app_digest.as_str()))
            .or_insert_with(|| DigestAcc {
                counters: Counters::default(),
                users: BTreeSet::new(),
                sql: view.sql_text.as_deref(),
                statement_type: view.statement_type.as_str(),
                schema_name: view.schema_name.as_deref(),
            });
        entry.counters.add(view);
        if let Some(user) = view.db_user.as_deref() {
            entry.users.insert(user);
        }
        // 대표 SQL 은 **처음 만난 것 중 가려지지 않은 것**을 쓴다. 정책이 원문을
        // 저장하지 않는 레코드가 섞여 있어도 표에 SQL 이 나오게 한다.
        if entry.sql.is_none() {
            entry.sql = view.sql_text.as_deref();
        }
    }

    let mut rows: Vec<DigestRow> = acc
        .into_iter()
        .map(|((instance_id, app_digest), a)| {
            let c = a.counters;
            DigestRow {
                instance_id: instance_id.to_string(),
                app_digest: app_digest.to_string(),
                digest_query: a.sql.map(str::to_string),
                users: a.users.into_iter().map(str::to_string).collect(),
                statement_type: a.statement_type.to_string(),
                schema_name: a.schema_name.map(str::to_string),
                exec_count: c.count,
                total_time_ms: c.total_time_ms,
                avg_time_ms: c.avg_time_ms(),
                max_time_ms: c.max_time_ms,
                avg_rows_examined: c.avg_rows_examined(),
                first_seen_ms: c.first_seen_ms,
                last_seen_ms: c.last_seen_ms,
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        b.exec_count
            .cmp(&a.exec_count)
            .then(b.total_time_ms.cmp(&a.total_time_ms))
            .then(a.app_digest.cmp(&b.app_digest))
    });
    rows
}

/// 인스턴스별로 접는다. 총 실행시간 내림차순.
pub fn instance_stats(views: &[SlowQueryView]) -> Vec<InstanceStats> {
    let mut acc: BTreeMap<&str, (Counters, BTreeSet<&str>)> = BTreeMap::new();
    for view in views {
        let entry = acc
            .entry(view.instance_id.as_str())
            .or_insert_with(|| (Counters::default(), BTreeSet::new()));
        entry.0.add(view);
        entry.1.insert(view.app_digest.as_str());
    }

    let mut rows: Vec<InstanceStats> = acc
        .into_iter()
        .map(|(instance_id, (c, digests))| InstanceStats {
            instance_id: instance_id.to_string(),
            unique_digest_count: digests.len() as u64,
            total_slow_query_count: c.count,
            total_execution_count: c.count,
            total_execution_time_ms: c.total_time_ms,
            avg_execution_time_ms: c.avg_time_ms(),
            max_execution_time_ms: c.max_time_ms,
            total_rows_examined: c.rows_examined_sum,
            read_query_count: c.read,
            write_query_count: c.write,
            ddl_query_count: c.ddl,
            commit_query_count: c.commit,
            other_query_count: c.other,
            first_seen_ms: c.first_seen_ms,
            last_seen_ms: c.last_seen_ms,
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.total_execution_time_ms));
    rows
}

/// `(instance, db_user)` 로 접는다. **사용자를 모르는 실행은 제외한다** —
/// `"unknown"` 같은 가짜 키를 만들면 그 행이 실제 사용자처럼 보인다.
pub fn user_stats(views: &[SlowQueryView]) -> Vec<UserStats> {
    let mut acc: BTreeMap<(&str, &str), (Counters, BTreeSet<&str>)> = BTreeMap::new();
    for view in views {
        let Some(user) = view.db_user.as_deref() else {
            continue;
        };
        let entry = acc
            .entry((view.instance_id.as_str(), user))
            .or_insert_with(|| (Counters::default(), BTreeSet::new()));
        entry.0.add(view);
        entry.1.insert(view.app_digest.as_str());
    }

    let mut rows: Vec<UserStats> = acc
        .into_iter()
        .map(|((instance_id, user), (c, digests))| UserStats {
            instance_id: instance_id.to_string(),
            user: user.to_string(),
            total_queries: c.count,
            unique_digest_count: digests.len() as u64,
            total_exec_time_ms: c.total_time_ms,
            avg_execution_time_ms: c.avg_time_ms(),
            max_execution_time_ms: c.max_time_ms,
            read_query_count: c.read,
            write_query_count: c.write,
            ddl_query_count: c.ddl,
            commit_query_count: c.commit,
            other_query_count: c.other,
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.total_exec_time_ms));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(overrides: impl FnOnce(&mut SlowQueryView)) -> SlowQueryView {
        let mut v = SlowQueryView {
            record_id: "i:1:1".into(),
            instance_id: "inst-a".into(),
            env: "dev".into(),
            state: "finalized".into(),
            thread_id: 1,
            started_at_ms: 1_000,
            duration_ms: 2_000,
            duration_source: "slowlog".into(),
            capture_source: "merged".into(),
            app_digest: "d1".into(),
            statement_type: "select".into(),
            schema_name: Some("shop".into()),
            db_user: Some("app".into()),
            sql_text: Some("SELECT ?".into()),
            sql_redacted_reason: None,
            sql_text_truncated: false,
            rows_examined: Some(100),
            rows_sent: Some(1),
            lock_time_ms: Some(0),
            has_plan: true,
            abandoned_reason: None,
        };
        overrides(&mut v);
        v
    }

    #[test]
    fn digest_rows_fold_by_instance_and_digest() {
        let rows = digest_rows(&[
            view(|v| v.duration_ms = 1_000),
            view(|v| {
                v.duration_ms = 3_000;
                v.db_user = Some("batch".into());
            }),
            view(|v| {
                v.app_digest = "d2".into();
                v.duration_ms = 5_000;
            }),
        ]);

        assert_eq!(rows.len(), 2);
        // 실행 수가 많은 쪽이 먼저.
        assert_eq!(rows[0].app_digest, "d1");
        assert_eq!(rows[0].exec_count, 2);
        assert_eq!(rows[0].total_time_ms, 4_000);
        assert_eq!(rows[0].avg_time_ms, 2_000.0);
        assert_eq!(rows[0].max_time_ms, 3_000);
        // 사용자는 합집합이다 — 한 다이제스트를 여러 계정이 돌린다.
        assert_eq!(rows[0].users, vec!["app", "batch"]);
    }

    /// **행 정보가 없는 실행을 분모에 넣지 않는다.**
    ///
    /// 실시간 캡처는 실행 중 문장의 행 카운터를 얻을 수 없다. `None` 을 0 으로
    /// 세면 평균이 절반으로 깎이고, 그건 "인덱스가 잘 타고 있다" 로 오독된다.
    #[test]
    fn rows_examined_average_ignores_records_without_row_counters() {
        let rows = digest_rows(&[
            view(|v| v.rows_examined = Some(100)),
            view(|v| v.rows_examined = None),
        ]);
        assert_eq!(rows[0].avg_rows_examined, Some(100.0));

        let none_at_all = digest_rows(&[view(|v| v.rows_examined = None)]);
        assert_eq!(
            none_at_all[0].avg_rows_examined, None,
            "행 정보가 하나도 없으면 0 이 아니라 없음이다"
        );
    }

    #[test]
    fn the_representative_sql_skips_redacted_records() {
        // 권한이 없거나 정책이 저장하지 않은 레코드가 먼저 와도 표에 SQL 이 나온다.
        let rows = digest_rows(&[
            view(|v| {
                v.sql_text = None;
                v.sql_redacted_reason = Some("not_stored");
            }),
            view(|v| v.sql_text = Some("SELECT 1".into())),
        ]);
        assert_eq!(rows[0].digest_query.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn all_redacted_means_no_representative_sql() {
        // 리터럴 통제를 우회할 경로가 없어야 한다.
        let rows = digest_rows(&[view(|v| {
            v.sql_text = None;
            v.sql_redacted_reason = Some("insufficient_role");
        })]);
        assert_eq!(rows[0].digest_query, None);
    }

    #[test]
    fn statements_are_classified_like_the_reference_dashboard() {
        let stats = instance_stats(&[
            view(|v| v.statement_type = "select".into()),
            view(|v| v.statement_type = "insert".into()),
            view(|v| v.statement_type = "update".into()),
            view(|v| v.statement_type = "ddl".into()),
            view(|v| {
                v.statement_type = "other".into();
                v.sql_text = Some("COMMIT".into());
            }),
            view(|v| {
                v.statement_type = "other".into();
                v.sql_text = Some("SET autocommit=?".into());
            }),
        ]);

        let s = &stats[0];
        assert_eq!(s.read_query_count, 1);
        assert_eq!(s.write_query_count, 2);
        assert_eq!(s.ddl_query_count, 1);
        assert_eq!(s.commit_query_count, 1);
        assert_eq!(s.other_query_count, 1);
        assert_eq!(s.total_slow_query_count, 6);
    }

    /// SQL 이 가려졌으면 `commit` 을 **추측하지 않는다.**
    #[test]
    fn a_redacted_statement_is_not_guessed_to_be_a_commit() {
        let stats = instance_stats(&[view(|v| {
            v.statement_type = "other".into();
            v.sql_text = None;
        })]);
        assert_eq!(stats[0].commit_query_count, 0);
        assert_eq!(stats[0].other_query_count, 1);
    }

    #[test]
    fn instance_stats_count_unique_digests() {
        let stats = instance_stats(&[
            view(|v| v.app_digest = "d1".into()),
            view(|v| v.app_digest = "d1".into()),
            view(|v| v.app_digest = "d2".into()),
            view(|v| {
                v.instance_id = "inst-b".into();
                v.app_digest = "d9".into();
            }),
        ]);
        assert_eq!(stats.len(), 2);
        let a = stats
            .iter()
            .find(|s| s.instance_id == "inst-a")
            .expect("inst-a");
        assert_eq!(a.unique_digest_count, 2);
        assert_eq!(a.total_slow_query_count, 3);
    }

    /// **사용자를 모르는 실행은 사용자 표에 넣지 않는다.**
    #[test]
    fn user_stats_exclude_records_without_a_db_user() {
        let rows = user_stats(&[
            view(|v| v.db_user = Some("app".into())),
            view(|v| v.db_user = None),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user, "app");
        assert_eq!(rows[0].total_queries, 1);
    }

    #[test]
    fn empty_input_produces_no_rows() {
        assert!(digest_rows(&[]).is_empty());
        assert!(instance_stats(&[]).is_empty());
        assert!(user_stats(&[]).is_empty());
    }
}
