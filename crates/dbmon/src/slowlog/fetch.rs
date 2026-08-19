//! 슬로우 로그 원문을 가져온다 (M2-7).
//!
//! # 두 소스, 하나의 파서
//!
//! | 소스 | 용도 | 검증 |
//! |---|---|---|
//! | CloudWatch Logs `FilterLogEvents` | 프로덕션 ([05 §8.3](../../../../docs/05-collector.md)) | AWS 필요 |
//! | 로컬 파일 | 개발 — SSO 가 만료돼도 백필 경로를 돌린다 | 로컬 |
//!
//! **파서는 하나다.** 소스가 텍스트를 주고 [`super::parse`] 가 판정한다 — 소스마다
//! 파싱을 따로 두면 한쪽만 고쳐지는 부류의 결함이 생긴다.
//!
//! # 체크포인트가 없으면 같은 구간을 영원히 다시 읽는다
//!
//! `FilterLogEvents` 는 시간 구간을 받는다. 마지막 처리 시각을 저장하지 않으면 매번
//! 처음부터 읽고, 그러면 API 레이트 리밋에 걸리면서도 새 데이터는 못 따라간다.
//! 병합이 멱등이라 **데이터는 안전하지만 비용과 지연이 커진다.**

use dbmon_core::error::Result;
use dbmon_core::ids::InstanceId;
use dbmon_core::time::EpochMs;

/// 가져온 로그 조각과 다음 시작점.
#[derive(Debug, Default)]
pub struct LogChunk {
    /// 슬로우 로그 텍스트. 여러 이벤트를 줄바꿈으로 이었다.
    pub text: String,
    /// 다음 호출의 시작 시각. **처리한 마지막 이벤트 시각 + 1ms** 다.
    ///
    /// `None` 이면 진전이 없었다 — 체크포인트를 옮기지 않는다.
    pub next_since_ms: Option<EpochMs>,
    /// 더 남았는가. 참이면 같은 주기에 이어서 호출해도 된다.
    pub has_more: bool,
}

/// 슬로우 로그 소스.
#[async_trait::async_trait]
pub trait SlowLogFetcher: Send + Sync {
    /// `since_ms` 이후의 로그를 가져온다.
    async fn fetch(&self, instance: &InstanceId, since_ms: EpochMs) -> Result<LogChunk>;
}

/// CloudWatch Logs 로그 그룹 이름 ([05 §8.3](../../../../docs/05-collector.md)).
///
/// RDS 가 만드는 이름 규칙이다. **인스턴스 식별자만 들어간다** — 계정·리전은 API
/// 호출의 자격증명·엔드포인트가 정한다.
pub fn slowquery_log_group(instance: &InstanceId) -> Result<String> {
    let (_, _, identifier) = split_instance(instance)?;
    Ok(format!("/aws/rds/instance/{identifier}/slowquery"))
}

fn split_instance(instance: &InstanceId) -> Result<(String, String, String)> {
    let mut it = instance.as_str().split('/');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(a), Some(r), Some(i), None) => Ok((a.into(), r.into(), i.into())),
        _ => Err(dbmon_core::error::DomainError::InvalidInput {
            field: "instance_id".into(),
            reason: format!("형식이 아니다: {}", instance.as_str()),
        }),
    }
}

/// CloudWatch Logs 에서 가져온다. **프로덕션 경로.**
pub struct CloudWatchFetcher {
    client: aws_sdk_cloudwatchlogs::Client,
    /// 한 번에 가져올 이벤트 상한. 레이트 리밋과 메모리를 함께 막는다.
    max_events: i32,
}

impl CloudWatchFetcher {
    pub fn new(client: aws_sdk_cloudwatchlogs::Client) -> Self {
        Self {
            client,
            max_events: 10_000,
        }
    }
}

#[async_trait::async_trait]
impl SlowLogFetcher for CloudWatchFetcher {
    async fn fetch(&self, instance: &InstanceId, since_ms: EpochMs) -> Result<LogChunk> {
        let group = slowquery_log_group(instance)?;
        let out = self
            .client
            .filter_log_events()
            .log_group_name(&group)
            .start_time(since_ms)
            .limit(self.max_events)
            .send()
            .await
            .map_err(|e| dbmon_core::error::DomainError::Unavailable {
                dependency: "cloudwatchlogs",
                reason: crate::telemetry::scrub(&format!("{e:?}")),
            })?;

        let events = out.events();
        // **이벤트 메시지를 줄바꿈으로 잇는다.**
        //
        // RDS 는 슬로우 로그 엔트리 하나를 여러 이벤트로 쪼갤 수도, 한 이벤트에 담을
        // 수도 있다. 파서가 `# Time:` 을 경계로 자르므로 어느 쪽이든 동작한다 —
        // 그래서 이벤트 경계를 해석하지 않는다.
        let mut text = String::new();
        let mut last_ts: Option<EpochMs> = None;
        for e in events {
            if let Some(msg) = e.message() {
                text.push_str(msg.trim_end_matches('\n'));
                text.push('\n');
            }
            if let Some(ts) = e.timestamp() {
                last_ts = Some(last_ts.map_or(ts, |p: i64| p.max(ts)));
            }
        }

        Ok(LogChunk {
            text,
            // **처리한 마지막 이벤트 + 1ms.** 같은 이벤트를 다시 읽지 않는다.
            next_since_ms: last_ts.map(|t| t + 1),
            has_more: out.next_token().is_some(),
        })
    }
}

/// 로컬 파일에서 가져온다. **개발 전용.**
///
/// SSO 가 만료돼도 백필 경로를 끝까지 돌릴 수 있게 한다 — 이 프로젝트의 로컬 우선
/// 원칙이다. CloudWatch 를 흉내내지 않는다(그러면 목을 검증하게 된다).
pub struct FileFetcher {
    path: std::path::PathBuf,
}

impl FileFetcher {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait::async_trait]
impl SlowLogFetcher for FileFetcher {
    async fn fetch(&self, _instance: &InstanceId, _since_ms: EpochMs) -> Result<LogChunk> {
        // 파일 전체를 읽고 파서가 시간 필터를 하도록 둔다 — 로컬 파일은 작고,
        // 오프셋 추적을 흉내내면 프로덕션과 다른 코드를 검증하게 된다.
        let text = tokio::fs::read_to_string(&self.path).await.map_err(|e| {
            dbmon_core::error::DomainError::Unavailable {
                dependency: "slowlog_file",
                reason: format!("{}: {e}", self.path.display()),
            }
        })?;
        Ok(LogChunk {
            text,
            // 파일 소스는 체크포인트를 옮기지 않는다 — 병합이 멱등이라 안전하다.
            next_since_ms: None,
            has_more: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> InstanceId {
        InstanceId::new("123456789012", "ap-northeast-2", "orders-01").expect("id")
    }

    /// RDS 의 로그 그룹 이름 규칙 ([05 §8.3]).
    #[test]
    fn log_group_follows_the_rds_naming_rule() {
        assert_eq!(
            slowquery_log_group(&instance()).expect("이름"),
            "/aws/rds/instance/orders-01/slowquery"
        );
    }

    /// **계정·리전을 이름에 넣지 않는다.** 넣으면 존재하지 않는 그룹을 조회한다.
    #[test]
    fn log_group_contains_only_the_identifier() {
        let name = slowquery_log_group(&instance()).expect("이름");
        assert!(!name.contains("123456789012"), "{name}");
        assert!(!name.contains("ap-northeast-2"), "{name}");
    }

    #[tokio::test]
    async fn the_file_fetcher_reads_the_whole_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("dbmon-test-slow.log");
        let body = "# Time: 2026-08-19T13:48:54.659276Z\n";
        tokio::fs::write(&path, body).await.expect("쓰기");

        let chunk = FileFetcher::new(&path)
            .fetch(&instance(), 0)
            .await
            .expect("읽기");
        assert_eq!(chunk.text, body);
        assert!(!chunk.has_more);
        // 파일 소스는 체크포인트를 옮기지 않는다.
        assert_eq!(chunk.next_since_ms, None);
    }

    /// 파일이 없으면 **사유를 담아 실패한다** — 조용히 빈 텍스트를 주면 "백필이
    /// 왜 안 되나" 를 추적할 수 없다.
    #[tokio::test]
    async fn a_missing_file_is_an_error_not_an_empty_chunk() {
        let e = FileFetcher::new("/nonexistent/dbmon/slow.log")
            .fetch(&instance(), 0)
            .await
            .expect_err("없는 파일을 읽었다");
        assert!(format!("{e}").contains("slowlog_file"), "{e}");
    }
}
