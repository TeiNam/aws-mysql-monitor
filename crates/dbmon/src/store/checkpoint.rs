//! 재개 지점 저장 (`CKPT`, [04 §7](../../../../docs/04-data-model.md)).
//!
//! # 이것이 없으면 장애 구간이 영구히 빈다
//!
//! 백필은 시간 구간을 받는다. 마지막 처리 시각을 저장하지 않으면 매 라운드
//! `now − 고정창` 을 읽게 되고, **그 창보다 긴 중단이 생기면 그 구간은 영원히
//! 읽히지 않는다.** 배포로 2분, 리더 교체로 1분만 멈춰도 그만큼 구멍이 난다.
//!
//! 실행별 정확 지표의 유일한 출처가 슬로우로그이므로([19 §G2]) 그 구멍은
//! "그 시간대 쿼리는 `rows_examined` 가 영구히 없다" 로 나타난다.
//!
//! # 왜 뒤로도 갈 수 있게 두는가
//!
//! 체크포인트를 **앞으로만** 움직이면(`max`) 시계가 뒤로 튀거나 잘못된 값이 한 번
//! 저장됐을 때 복구할 수가 없다. 그래서 값을 그대로 쓰고, 호출부가 무엇을 저장할지
//! 결정한다 — 병합이 멱등이므로 같은 구간을 다시 읽는 것은 안전하다(비용만 든다).

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use dbmon_core::error::Result;
use dbmon_core::time::EpochMs;

use super::map_sdk_err;

const PK: &str = "CKPT";

pub struct DynamoCheckpointStore {
    client: Client,
    table: String,
}

/// 슬로우로그 백필의 잡 이름. 인스턴스별로 따로 진행한다.
///
/// 이름 규칙은 [05 §8.3](../../../../docs/05-collector.md) 의 `cwlog#<instance_id>` 다.
pub fn slowlog_job(instance: &dbmon_core::ids::InstanceId) -> String {
    format!("cwlog#{}", instance.as_str())
}

impl DynamoCheckpointStore {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self {
            client,
            table: table.into(),
        }
    }

    /// 마지막 처리 시각. 없으면 `None` — 호출부가 초기 창을 정한다.
    pub async fn get(&self, job: &str) -> Result<Option<EpochMs>> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(PK.to_string()))
            .key("SK", AttributeValue::S(job.to_string()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out
            .item
            .as_ref()
            .and_then(|i| i.get("position_ms"))
            .and_then(|v| v.as_n().ok())
            .and_then(|s| s.parse().ok()))
    }

    /// 재개 지점을 저장한다.
    pub async fn put(&self, job: &str, position_ms: EpochMs) -> Result<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", AttributeValue::S(PK.to_string()))
            .key("SK", AttributeValue::S(job.to_string()))
            .update_expression("SET position_ms = :p, updated_at_ms = :p")
            .expression_attribute_values(":p", AttributeValue::N(position_ms.to_string()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }
}

/// 이번 라운드가 읽을 시작 시각을 정한다. **순수 함수.**
///
/// | 상황 | 결과 |
/// |---|---|
/// | 체크포인트 있음 | 그 지점부터 |
/// | 체크포인트 없음 (첫 실행) | `now − initial_lookback_ms` |
/// | 체크포인트가 너무 오래됨 | `now − max_lookback_ms` 로 **자른다** |
///
/// 마지막 항목이 중요하다. 며칠 멈춰 있던 워커가 깨어나면 며칠치 로그를 한 번에
/// 읽으려 하고, 그건 API 조절과 메모리 폭주를 동시에 일으킨다. 잘린 구간은
/// **로그로 알린다** — 조용히 건너뛰면 "왜 그때 지표가 없나" 를 추적할 수 없다.
pub fn resume_from(
    checkpoint_ms: Option<EpochMs>,
    now_ms: EpochMs,
    initial_lookback_ms: i64,
    max_lookback_ms: i64,
) -> (EpochMs, Option<i64>) {
    let floor = now_ms - max_lookback_ms;
    match checkpoint_ms {
        Some(cp) if cp < floor => (floor, Some(floor - cp)),
        Some(cp) => (cp, None),
        None => (now_ms - initial_lookback_ms, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: EpochMs = 1_787_147_400_000;
    const INITIAL: i64 = 5 * 60_000;
    const MAX: i64 = 60 * 60_000;

    #[test]
    fn job_name_follows_the_documented_rule() {
        let id = dbmon_core::ids::InstanceId::new("123456789012", "ap-northeast-2", "orders-01")
            .unwrap();
        assert_eq!(
            slowlog_job(&id),
            "cwlog#123456789012/ap-northeast-2/orders-01"
        );
    }

    /// 첫 실행은 최근 구간만 본다 — 전체 로그를 읽으면 기동이 멈춘다.
    #[test]
    fn the_first_run_looks_back_a_bounded_window() {
        let (from, skipped) = resume_from(None, NOW, INITIAL, MAX);
        assert_eq!(from, NOW - INITIAL);
        assert_eq!(skipped, None);
    }

    /// **체크포인트가 있으면 그 지점부터 — 구멍이 생기지 않는다.**
    ///
    /// 이게 없으면 중단 구간의 정확 지표가 영구히 없다.
    #[test]
    fn a_checkpoint_resumes_exactly_where_it_stopped() {
        let stopped = NOW - 12 * 60_000; // 12분 전에 멈췄다
        let (from, skipped) = resume_from(Some(stopped), NOW, INITIAL, MAX);
        assert_eq!(from, stopped, "재개 지점이 아니라 고정 창을 썼다");
        assert_eq!(skipped, None);
        // 고정 창(5분)만 봤다면 7분치를 잃었을 것이다.
        assert!(from < NOW - INITIAL);
    }

    /// **너무 오래된 체크포인트는 자른다 — 그리고 알린다.**
    ///
    /// 며칠 멈춘 워커가 며칠치를 한 번에 읽으면 API 조절과 메모리 폭주가 함께 온다.
    #[test]
    fn a_very_old_checkpoint_is_clamped_and_reported() {
        let ancient = NOW - 3 * 24 * 60 * 60_000; // 3일 전
        let (from, skipped) = resume_from(Some(ancient), NOW, INITIAL, MAX);
        assert_eq!(from, NOW - MAX);
        // 건너뛴 양을 반드시 보고한다.
        assert_eq!(skipped, Some(NOW - MAX - ancient));
        assert!(skipped.unwrap() > 0);
    }

    /// 미래 체크포인트(시계 역행)는 그대로 쓴다 — 읽을 것이 없어 무해하고,
    /// 값을 조작하면 원인 추적이 어려워진다.
    #[test]
    fn a_future_checkpoint_is_used_as_is() {
        let future = NOW + 60_000;
        let (from, skipped) = resume_from(Some(future), NOW, INITIAL, MAX);
        assert_eq!(from, future);
        assert_eq!(skipped, None);
    }
}
