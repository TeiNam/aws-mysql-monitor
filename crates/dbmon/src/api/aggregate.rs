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
    /// 평균 조사 행.
    ///
    /// **실시간 캡처만으로 만든 실행은 분모에서 뺀다.** `PROCESSLIST` 는 실행 *중*
    /// 문장의 행 카운터를 주지 못해 `0` 이 들어오고([19 §G2]), 그 `0` 을 평균에
    /// 넣으면 값이 깎여 "인덱스가 잘 타고 있다" 로 오독된다. 슬로우로그가 붙은
    /// 실행(`slowlog`·`merged`·`backfill`)의 `0` 은 **진짜 0** 이므로 센다.
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
        // ⚠ **바이트로 자르지 않는다.** `&text[..6]` 은 6바이트째가 UTF-8 문자
        // 중간이면 패닉한다 — `/*가*/COMMIT` 같은 주석 하나로 다이제스트·통계
        // 요청 전체가 500 이 된다. 문자 단위로 비교한다.
        _ if starts_with_ignore_case(sql, "commit") => Kind::Commit,
        _ => Kind::Other,
    }
}

/// `sql` 이 `needle` 로 시작하는가 (대소문자 무시, **문자 단위**).
fn starts_with_ignore_case(sql: Option<&str>, needle: &str) -> bool {
    let Some(text) = sql else { return false };
    let mut head = text.trim_start().chars();
    needle.chars().all(|want| {
        head.next()
            .is_some_and(|got| got.eq_ignore_ascii_case(&want))
    })
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
        // 실시간 캡처만으로 만든 레코드의 행 수는 **측정값이 아니다.**
        if let Some(rows) = view.rows_examined {
            if view.capture_source != "processlist" {
                self.rows_examined_sum += rows;
                self.rows_examined_n += 1;
            }
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

    /// 측정된 실행이 하나도 없으면 **`None`** 이다. `0` 으로 내면 "행을 안 읽었다" 가 된다.
    fn avg_rows_examined(&self) -> Option<f64> {
        if self.rows_examined_n == 0 {
            return None;
        }
        Some(self.rows_examined_sum as f64 / self.rows_examined_n as f64)
    }
}

/// 같은 실행이 두 행으로 온 것을 하나로 접는다. **접은 수를 함께 돌려준다.**
///
/// # 왜 필요한가 (실측)
///
/// `record_id` 는 실행 하나의 신원이다. 그런데 저장소에 **항목이 둘** 생길 수 있다 —
/// 실시간 캡처와 슬로우로그가 처음 쓰기를 동시에 하면 둘 다 "없다" 를 보고, `SK` 의
/// 밀리초가 몇 ms 달라 조건부 쓰기가 양쪽 다 통과한다. 로컬 스택에서 200행 중 3건이
/// 그 상태였다:
///
/// ```text
/// state inflight   capture processlist  dur 1993  rows_examined 0       ← 유령
/// state finalized  capture merged       dur 2125  rows_examined 21633   ← 진실
/// ```
///
/// 그대로 내보내면 세 가지가 동시에 틀린다: 표에 같은 쿼리가 두 줄(그리고 React 는
/// 키가 겹친다), 통계가 실행 수를 **두 배**로 세고, 유령 줄은 영원히 "진행 중" 이다.
///
/// 저장소 쪽 경합은 별도 결함이다([20 참조](../../../../docs/20-review-log.md)).
/// 조회는 그것과 무관하게 **실행 하나를 한 줄로** 말해야 한다.
///
/// # 무엇을 남기나
///
/// 1. **끝을 관측한 쪽** — finalized > abandoned > inflight. 진행 중은 아직 값이 자란다.
/// 2. **정확 지표가 있는 쪽** — 슬로우로그가 붙은 캡처(`merged`/`slowlog`). 처리목록
///    캡처의 `rows_examined` 는 0 이다(실행 중에는 얻을 수 없다).
/// 3. **더 길게 관측한 쪽** — 같은 조건이면 큰 `duration_ms` 가 진실에 가깝다.
pub fn dedupe_executions(views: Vec<SlowQueryView>) -> (Vec<SlowQueryView>, usize) {
    let before = views.len();
    let views = collapse_same_second(views);
    let mut best: BTreeMap<String, SlowQueryView> = BTreeMap::new();
    for view in views {
        match best.remove(&view.record_id) {
            // 둘 다 있으면 이긴 쪽을 남기고 **진 쪽의 정보를 채운다.**
            Some(kept) => {
                let (winner, loser) = if rank(&kept) >= rank(&view) {
                    (kept, view)
                } else {
                    (view, kept)
                };
                best.insert(winner.record_id.clone(), absorb(winner, &loser));
            }
            None => {
                best.insert(view.record_id.clone(), view);
            }
        }
    }
    let out: Vec<SlowQueryView> = best.into_values().collect();
    let collapsed = before - out.len();
    (out, collapsed)
}

/// **초 경계를 걸친 쌍둥이**도 접는다 — `record_id` 가 다르므로 위 단계가 못 잡는다.
///
/// 실시간 캡처의 시작 시각은 `now − PROCESSLIST.TIME × 1000` 이라 **정수 초**이고
/// 슬로우로그는 밀리초다. 두 추정이 초 경계를 사이에 두면 `record_id` 가 갈린다 —
/// 저장소가 `find_merge_candidate` 로 흡수하려는 그 상황이고, 첫 쓰기가 정확히
/// 동시면 흡수에 실패해 항목이 둘 남는다.
///
/// # 왜 오접기가 불가능한가 — **구간이 겹쳐야 한다**
///
/// 창(2초)만 보면 임계값(`long_query_time`)이 2초보다 작을 때 **연속한 두 실행**이
/// 접힌다(7라운드 지적). 그래서 시간 창에 더해 **실행 구간이 겹칠 것**을 요구한다:
///
/// ```text
/// 같은 실행의 쌍둥이 :  [====실행====]        시작 3ms 차이 → 완전히 겹친다
///                       [====실행====]
/// 연속한 두 실행     :  [==1==]  [==2==]      1이 끝난 뒤 2가 시작한다 → 안 겹친다
/// ```
///
/// 커넥션 하나는 **한 번에 한 문장**만 실행한다. 그래서 구간이 겹치면 그 둘은 같은
/// 실행이다 — 임계값 설정과 무관하게 성립한다. 중첩 문장(`is_nested`)은 겹칠 수
/// 있지만 다이제스트가 다르므로 여기서 걸러진다.
fn collapse_same_second(mut views: Vec<SlowQueryView>) -> Vec<SlowQueryView> {
    const WINDOW_MS: i64 = dbmon_core::clock_offset::BASE_MERGE_WINDOW_MS;

    // (인스턴스, 스레드, 다이제스트)로 묶고 시간순으로 본다.
    views.sort_by(|a, b| {
        (
            a.instance_id.as_str(),
            a.thread_id,
            a.app_digest.as_str(),
            a.started_at_ms,
        )
            .cmp(&(
                b.instance_id.as_str(),
                b.thread_id,
                b.app_digest.as_str(),
                b.started_at_ms,
            ))
    });

    let mut out: Vec<SlowQueryView> = Vec::with_capacity(views.len());
    // **묶음의 첫(가장 이른) 행을 기준으로 창을 잰다.** 남긴 행 기준으로 재면 이긴 행이
    // 더 늦은 쪽일 때 기준이 밀려 **사슬처럼 이어 붙는다** — 2초 창이 실제로는
    // 무한히 늘어날 수 있다. 입력이 시간 오름차순이므로 첫 행 기준이면 한 묶음은
    // 최대 2초 폭이다.
    let mut anchor_ms = i64::MIN;
    for view in views {
        let same_run = out.last().is_some_and(|prev| {
            prev.instance_id == view.instance_id
                && prev.thread_id == view.thread_id
                && prev.app_digest == view.app_digest
                && view.started_at_ms.saturating_sub(anchor_ms) <= WINDOW_MS
                // **구간이 겹쳐야 같은 실행이다.** 입력이 시간 오름차순이므로
                // "뒤 행의 시작 < 앞 행의 끝" 이면 겹친다.
                && view.started_at_ms < prev.started_at_ms.saturating_add(prev.duration_ms)
        });
        if !same_run {
            anchor_ms = view.started_at_ms;
            out.push(view);
            continue;
        }
        let prev = out.pop().expect("same_run 이면 마지막이 있다");
        let (winner, loser) = if rank(&prev) >= rank(&view) {
            (prev, view)
        } else {
            (view, prev)
        };
        out.push(absorb(winner, &loser));
    }
    out
}

/// 진 행에만 있는 정보를 이긴 행으로 옮긴다.
///
/// **버리는 쪽이 유일한 출처인 값이 있다.** 두 항목은 병합되지 않은 쌍둥이이므로
/// 각자 다른 것을 들고 있다 — 실행계획이 진행 중 레코드에만 수집돼 있으면, 이긴 행만
/// 남기면 **계획이 화면에서 사라진다.** 없는 값만 채우고, 있는 값은 건드리지 않는다.
fn absorb(mut winner: SlowQueryView, loser: &SlowQueryView) -> SlowQueryView {
    // ⚠ **`has_plan` 은 옮기지 않는다.** 계획은 항목에 붙어 있고 화면은 **남긴 행의
    // `record_id`** 로 계획을 가져온다. 진 행의 계획을 "있다" 고 표시하면 눌렀을 때
    // 비어 있는 상세로 간다 — 없는 것보다 나쁜 거짓말이다(7라운드 지적). 대신
    // 동순위일 때 계획이 있는 쪽을 이기게 해서([`rank`]) 계획을 잃지 않는다.
    if winner.sql_text.is_none() && loser.sql_text.is_some() {
        winner.sql_text = loser.sql_text.clone();
        winner.sql_redacted_reason = loser.sql_redacted_reason;
        winner.sql_text_truncated = loser.sql_text_truncated;
    }
    if winner.schema_name.is_none() {
        winner.schema_name = loser.schema_name.clone();
    }
    if winner.db_user.is_none() {
        winner.db_user = loser.db_user.clone();
    }
    // **처리목록 캡처의 행 수는 채우지 않는다.** 실행 중에는 얻을 수 없어 0 이 오고,
    // 그 0 을 "측정된 0" 으로 채우면 평균이 깎여 "인덱스가 잘 탄다" 로 오독된다.
    if loser.capture_source != "processlist" {
        if winner.rows_examined.is_none() {
            winner.rows_examined = loser.rows_examined;
        }
        if winner.rows_sent.is_none() {
            winner.rows_sent = loser.rows_sent;
        }
        if winner.lock_time_ms.is_none() {
            winner.lock_time_ms = loser.lock_time_ms;
        }
    }
    // 한쪽이 "관측이 끊겼다" 를 기록했으면 그 사실을 지우지 않는다 — 정확 지표를
    // 의심할 근거이고, 화면이 배지에 함께 보여준다.
    if winner.abandoned_reason.is_none() {
        winner.abandoned_reason = loser.abandoned_reason.clone();
    }
    winner
}

/// 접을 때의 우선순위. 큰 값이 남는다.
fn rank(v: &SlowQueryView) -> (u8, u8, u8, i64) {
    let closed = match v.state.as_str() {
        "finalized" => 2,
        "abandoned" => 1,
        _ => 0,
    };
    let exact = match v.capture_source.as_str() {
        "merged" | "slowlog" => 1,
        _ => 0,
    };
    // 동순위면 **계획이 있는 쪽**을 남긴다 — 그 행의 `record_id` 로 계획을 가져올 수
    // 있어야 하므로, 계획을 옮기는 대신 계획이 붙은 행을 남긴다.
    let plan = u8::from(v.has_plan);
    (closed, exact, plan, v.duration_ms)
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

    /// **같은 실행이 두 행으로 오면 하나만 남고, 남는 쪽은 진실이어야 한다.**
    ///
    /// 실측한 상태를 그대로 옮겼다: 유령(진행 중·처리목록·`rows_examined=0`)과
    /// 진실(확정·병합·정확 지표)이 같은 `record_id` 로 함께 왔다. 유령이 남으면
    /// 표는 끝난 쿼리를 "진행 중" 으로, 통계는 실행 수를 두 배로 말한다.
    #[test]
    fn a_duplicated_execution_collapses_to_the_row_that_saw_the_end() {
        let ghost = view(|v| {
            v.state = "inflight".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 1_993;
            v.rows_examined = Some(0);
        });
        let truth = view(|v| {
            v.state = "finalized".into();
            v.capture_source = "merged".into();
            v.duration_ms = 2_125;
            v.rows_examined = Some(21_633);
        });

        // 순서를 바꿔도 같은 결과여야 한다 — 저장소가 어느 쪽을 먼저 주는지는 모른다.
        for pair in [
            vec![ghost.clone(), truth.clone()],
            vec![truth.clone(), ghost.clone()],
        ] {
            let (out, collapsed) = dedupe_executions(pair);
            assert_eq!(collapsed, 1, "접은 수를 말해야 한다 (조용히 접지 않는다)");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].state, "finalized");
            assert_eq!(out[0].rows_examined, Some(21_633));
            assert_eq!(out[0].duration_ms, 2_125);
        }

        // 다른 실행은 접지 않는다.
        let other = view(|v| {
            v.record_id = "i:2:1".into();
            v.thread_id = 2;
        });
        let (out, collapsed) = dedupe_executions(vec![ghost.clone(), truth, other]);
        assert_eq!(collapsed, 1);
        assert_eq!(out.len(), 2);

        // **진행 중 둘뿐이면 더 오래 관측한 쪽이 남는다** — 실행시간의 하한이 더 크다.
        let longer = view(|v| {
            v.state = "inflight".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 5_000;
            v.rows_examined = Some(0);
        });
        let (out, _) = dedupe_executions(vec![ghost, longer]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].duration_ms, 5_000);
    }

    /// **버리는 행이 유일한 출처인 값은 옮겨야 한다.**
    ///
    /// 쌍둥이는 병합되지 않았으므로 각자 다른 것을 들고 있다. 실행계획이 진행 중
    /// 레코드에만 수집돼 있으면, 이긴 행만 남기면 계획이 화면에서 사라진다.
    #[test]
    fn collapsing_carries_over_what_only_the_discarded_row_had() {
        let ghost = view(|v| {
            v.state = "inflight".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 1_000;
            v.has_plan = true; // 계획은 여기에만 있다
            v.rows_examined = Some(0); // 처리목록의 0 — 의미 없는 값이다
            v.rows_sent = Some(0);
            v.schema_name = Some("shop".into());
            v.abandoned_reason = Some("owner_lost".into());
        });
        let truth = view(|v| {
            v.state = "finalized".into();
            v.capture_source = "merged".into();
            v.duration_ms = 2_000;
            v.has_plan = false;
            v.rows_examined = None; // 아직 모른다
            v.rows_sent = None;
            v.schema_name = None;
            v.db_user = None;
            v.sql_text = None;
            v.abandoned_reason = None;
        });

        let (out, _) = dedupe_executions(vec![ghost.clone(), truth.clone()]);
        let kept = &out[0];
        assert_eq!(kept.state, "finalized");
        // **`has_plan` 은 옮기지 않는다.** 계획은 항목에 붙어 있고 화면은 남긴 행의
        // `record_id` 로 가져온다 — "있다" 고 하고 비어 있는 상세로 보내면 없는 것보다
        // 나쁘다. 계획을 잃지 않는 장치는 동순위 우선순위다(아래 테스트).
        assert!(
            !kept.has_plan,
            "진 행의 계획을 '있다' 로 표시했다 — 누르면 빈 상세가 나온다"
        );
        assert_eq!(kept.schema_name.as_deref(), Some("shop"));
        assert_eq!(kept.sql_text.as_deref(), Some("SELECT ?"));
        assert_eq!(
            kept.abandoned_reason.as_deref(),
            Some("owner_lost"),
            "한쪽이 기록한 '관측이 끊겼다' 를 지웠다"
        );
        // **처리목록의 0 은 채우지 않는다** — 측정값이 아니다.
        assert_eq!(
            kept.rows_examined, None,
            "실행 중 캡처의 0 을 측정값으로 채웠다 — 평균이 깎인다"
        );
        assert_eq!(kept.rows_sent, None);

        // 반대 순서도 같아야 한다.
        let (out2, _) = dedupe_executions(vec![truth, ghost]);
        assert_eq!(out2[0].has_plan, out[0].has_plan);
        assert_eq!(out2[0].rows_examined, out[0].rows_examined);

        // **동순위면 계획이 있는 쪽을 남긴다** — 그래야 계획이 화면에서 사라지지 않는다.
        let plain = view(|v| {
            v.state = "finalized".into();
            v.capture_source = "merged".into();
            v.has_plan = false;
        });
        let with_plan = view(|v| {
            v.state = "finalized".into();
            v.capture_source = "merged".into();
            v.has_plan = true;
        });
        let (out3, _) = dedupe_executions(vec![plain, with_plan]);
        assert!(
            out3[0].has_plan,
            "계획이 붙은 행을 버렸다 — 계획을 볼 길이 없다"
        );
    }

    /// **초 경계를 걸친 쌍둥이는 `record_id` 가 다르다.** 그래도 한 실행이다.
    ///
    /// 실시간 캡처의 시작 시각은 정수 초, 슬로우로그는 밀리초다. 두 추정이 초 경계를
    /// 사이에 두면 멱등 키가 갈린다 — 저장소가 ±2초 창으로 흡수하려는 그 상황이고,
    /// 첫 쓰기가 정확히 동시면 흡수에 실패한다.
    #[test]
    fn twins_that_straddle_a_second_boundary_also_collapse() {
        // 같은 실행: 실시간은 ...999, 슬로우로그는 ...002 로 안다.
        let live = view(|v| {
            v.record_id = "i:5:1755500399".into();
            v.thread_id = 5;
            v.started_at_ms = 1_755_500_399_999;
            v.state = "inflight".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 2_100;
            v.rows_examined = Some(0);
        });
        let log = view(|v| {
            v.record_id = "i:5:1755500400".into(); // ← 초가 다르다
            v.thread_id = 5;
            v.started_at_ms = 1_755_500_400_002;
            v.state = "finalized".into();
            v.capture_source = "slowlog".into();
            v.duration_ms = 2_240;
            v.rows_examined = Some(900);
        });

        let (out, collapsed) = dedupe_executions(vec![live.clone(), log.clone()]);
        assert_eq!(collapsed, 1, "초 경계를 걸친 쌍둥이를 접지 못했다");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, "finalized");
        assert_eq!(out[0].rows_examined, Some(900));

        // **창을 넘으면 접지 않는다** — 같은 스레드에서 나중에 다시 돈 실행이다.
        let later = view(|v| {
            v.record_id = "i:5:1755500410".into();
            v.thread_id = 5;
            v.started_at_ms = 1_755_500_410_000; // 10초 뒤
            v.state = "finalized".into();
            v.capture_source = "slowlog".into();
        });
        let (out, collapsed) = dedupe_executions(vec![live.clone(), log, later]);
        assert_eq!(collapsed, 1);
        assert_eq!(out.len(), 2, "다른 실행을 접었다 — 실행이 사라진다");

        // **사슬처럼 이어 붙지 않는다.** 2초씩 이어진 행들이 한 줄로 접히면
        // 실제로는 창이 무한히 늘어난다 — 기준은 묶음의 첫 행이다.
        let chain = |ms: i64, state: &str| {
            view(move |v| {
                v.record_id = format!("i:9:{ms}");
                v.thread_id = 9;
                v.started_at_ms = ms;
                v.state = state.into();
            })
        };
        let (out, collapsed) = dedupe_executions(vec![
            chain(0, "inflight"),
            chain(1_900, "finalized"),
            chain(3_500, "inflight"),
        ]);
        // 0 과 1,900 은 접히고(1,900 ≤ 2,000), 3,500 은 첫 행 기준 3.5초라 남는다.
        assert_eq!(collapsed, 1);
        assert_eq!(out.len(), 2, "사슬 접기가 일어나 실행이 사라졌다");

        // 다이제스트가 다르면 같은 스레드·같은 시각이어도 다른 실행이다.
        let other_digest = view(|v| {
            v.record_id = "i:5:1755500400".into();
            v.thread_id = 5;
            v.started_at_ms = 1_755_500_400_002;
            v.app_digest = "d2".into();
        });
        let (out, _) = dedupe_executions(vec![live, other_digest]);
        assert_eq!(out.len(), 2);
    }

    /// **연속한 두 실행은 접지 않는다 — 구간이 겹치지 않는다.**
    ///
    /// 임계값(`long_query_time`)이 2초보다 작으면 같은 스레드·같은 다이제스트가 창
    /// 안에 두 번 시작할 수 있다. 창만 보면 그 둘이 한 줄로 접혀 **실행이 사라진다**
    /// (7라운드 지적). 커넥션은 한 번에 한 문장만 실행하므로 **구간 겹침**이 판정이다.
    #[test]
    fn two_sequential_executions_on_one_thread_are_not_collapsed() {
        // 1.2초짜리 실행이 끝난 뒤 0.1초 뒤에 같은 쿼리가 또 돈다 (임계값 1초 설정).
        let first = view(|v| {
            v.record_id = "i:7:1000".into();
            v.thread_id = 7;
            v.started_at_ms = 1_000_000;
            v.duration_ms = 1_200;
            v.state = "finalized".into();
            v.capture_source = "slowlog".into();
        });
        let second = view(|v| {
            v.record_id = "i:7:1001".into();
            v.thread_id = 7;
            v.started_at_ms = 1_001_300; // 첫 실행이 끝난(1,001,200) 뒤에 시작
            v.duration_ms = 1_100;
            v.state = "finalized".into();
            v.capture_source = "slowlog".into();
        });
        let (out, collapsed) = dedupe_executions(vec![first.clone(), second]);
        assert_eq!(collapsed, 0, "연속한 두 실행을 접었다 — 실행이 사라진다");
        assert_eq!(out.len(), 2);

        // 반면 **겹치면** 같은 실행이다 — 커넥션은 한 번에 한 문장만 실행한다.
        let overlapping = view(|v| {
            v.record_id = "i:7:1000".into();
            v.thread_id = 7;
            v.started_at_ms = 1_000_500; // 첫 실행이 도는 중에 관측됐다
            v.duration_ms = 700;
            v.state = "inflight".into();
            v.capture_source = "processlist".into();
        });
        let (out, collapsed) = dedupe_executions(vec![first, overlapping]);
        assert_eq!(collapsed, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, "finalized");
    }

    /// 추적 끊김은 **진행 중보다는 낫고 확정보다는 못하다.** 순서가 뒤집히면
    /// "관측이 끊겼다" 가 확정값을 밀어낸다.
    #[test]
    fn an_abandoned_row_beats_in_flight_but_loses_to_finalized() {
        let inflight = view(|v| {
            v.state = "inflight".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 9_000;
        });
        let abandoned = view(|v| {
            v.state = "abandoned".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 3_000;
            v.abandoned_reason = Some("owner_lost".into());
        });
        let finalized = view(|v| {
            v.state = "finalized".into();
            v.capture_source = "processlist".into();
            v.duration_ms = 3_000;
        });

        let (out, _) = dedupe_executions(vec![inflight.clone(), abandoned.clone()]);
        assert_eq!(out[0].state, "abandoned", "진행 중이 추적 끊김을 밀어냈다");
        let (out, _) = dedupe_executions(vec![abandoned, finalized]);
        assert_eq!(out[0].state, "finalized");
        let _ = inflight;
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

    /// **측정되지 않은 행 수를 분모에 넣지 않는다.**
    ///
    /// 실시간 캡처는 실행 중 문장의 행 카운터를 얻을 수 없어 `0` 이 들어온다.
    /// 그 `0` 을 평균에 넣으면 값이 깎이고, 그건 "인덱스가 잘 타고 있다" 로 오독된다.
    /// 실측: 로컬 83건 중 처리목록 전용 7건이 전부 `rows_examined=0` 이었다.
    #[test]
    fn rows_examined_average_ignores_unmeasured_captures() {
        let rows = digest_rows(&[
            view(|v| {
                v.capture_source = "slowlog".into();
                v.rows_examined = Some(100);
            }),
            view(|v| {
                // 실시간 캡처의 0 은 측정값이 아니다.
                v.capture_source = "processlist".into();
                v.rows_examined = Some(0);
            }),
            view(|v| v.rows_examined = None),
        ]);
        assert_eq!(rows[0].avg_rows_examined, Some(100.0));

        // 슬로우로그가 붙은 0 은 **진짜 0** 이므로 센다.
        let real_zero = digest_rows(&[view(|v| {
            v.capture_source = "merged".into();
            v.rows_examined = Some(0);
        })]);
        assert_eq!(real_zero[0].avg_rows_examined, Some(0.0));

        let none_at_all = digest_rows(&[view(|v| v.rows_examined = None)]);
        assert_eq!(
            none_at_all[0].avg_rows_examined, None,
            "측정된 실행이 하나도 없으면 0 이 아니라 없음이다"
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

    /// **멀티바이트 문자로 시작하는 SQL 이 패닉을 만들지 않는다.**
    ///
    /// `&text[..6]` 로 자르면 6바이트째가 문자 중간이라 패닉하고, 그런 레코드 하나가
    /// 다이제스트·통계 요청 전체를 500 으로 만든다.
    #[test]
    fn multibyte_sql_does_not_panic_the_classifier() {
        for sql in [
            "/*가*/COMMIT",
            "가",
            "COM",
            "  commit",
            "커밋",
            "COMMIT /* 한글 주석 */",
        ] {
            let stats = instance_stats(&[view(|v| {
                v.statement_type = "other".into();
                v.sql_text = Some(sql.to_string());
            })]);
            assert_eq!(stats.len(), 1, "{sql:?} 에서 패닉했거나 행이 사라졌다");
        }

        // 분류 결과도 맞아야 한다.
        let commit = instance_stats(&[view(|v| {
            v.statement_type = "other".into();
            v.sql_text = Some("  commit".into());
        })]);
        assert_eq!(commit[0].commit_query_count, 1);
        let not_commit = instance_stats(&[view(|v| {
            v.statement_type = "other".into();
            v.sql_text = Some("/*가*/COMMIT".into());
        })]);
        assert_eq!(
            not_commit[0].other_query_count, 1,
            "주석으로 시작하면 commit 으로 단정하지 않는다"
        );
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
