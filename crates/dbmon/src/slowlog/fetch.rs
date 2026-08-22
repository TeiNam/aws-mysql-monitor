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
/// **`InstanceId` 의 접근자를 쓴다.** 문자열을 다시 파싱하면 검증 규칙이 두 곳이 된다 —
/// `validate_identifier` 가 이미 `/`·`:`·`#` 를 거부하므로 경로 조작은 불가능하다.
pub fn slowquery_log_group(instance: &InstanceId) -> Result<String> {
    Ok(format!(
        "/aws/rds/instance/{}/slowquery",
        instance.identifier()
    ))
}

/// 이 클라이언트로 이 인스턴스의 로그를 읽어도 되는가.
///
/// # 왜 순수 함수인가
///
/// 로그 그룹 이름에는 **계정도 리전도 없다**(`/aws/rds/instance/<id>/slowquery`).
/// 둘은 클라이언트의 자격증명·엔드포인트가 정하므로, 틀린 클라이언트로 조회하면
/// **같은 이름의 다른 DB** 로그를 읽어 엉뚱한 `instance_id` 로 저장한다 — 리터럴을
/// 포함한 오귀속이고 prd↔dev, 계정 경계를 넘을 수 있다.
///
/// 그 판정은 AWS 호출 없이 전부 검증할 수 있으므로 여기 둔다.
fn check_scope(instance: &InstanceId, region: &str, account: &str) -> Result<()> {
    if instance.region() != region {
        return Err(dbmon_core::error::DomainError::InvalidInput {
            field: "region".into(),
            reason: format!(
                "{}: 인스턴스 리전({})과 클라이언트 리전({})이 다르다",
                instance.as_str(),
                instance.region(),
                region
            ),
        });
    }
    // **크로스 계정 수집은 배선되지 않았다.** 탐색과 메트릭은 역할을 맡지만
    // (`sts:AssumeRole`) 슬로우로그 클라이언트는 기본 자격증명으로 돈다. 그대로
    // 조회하면 우리 계정에서 같은 이름을 찾고, 있으면 남의 로그를 저장한다.
    if instance.account() != account {
        return Err(dbmon_core::error::DomainError::Unsupported {
            what: "cross_account_slowlog".into(),
            reason: format!(
                "{}: 인스턴스 계정({})과 클라이언트 계정({})이 다르다 — 크로스 계정 \
                 슬로우로그 수집은 아직 지원하지 않는다",
                instance.as_str(),
                instance.account(),
                account
            ),
        });
    }
    Ok(())
}

/// CloudWatch Logs 에서 가져온다. **프로덕션 경로.**
///
/// # 리전별 클라이언트가 필요하다
///
/// 로그 그룹 이름에는 리전이 없다(`/aws/rds/instance/<id>/slowquery`) — 리전은
/// **클라이언트가** 정한다. 등록부는 `target_regions` 전체를 담으므로 홈 리전
/// 클라이언트 하나로 돌리면 두 가지가 깨진다:
///
/// 1. 타 리전 인스턴스는 `ResourceNotFound` 로 조용히 건너뛰어진다 → 정확 지표 영구 결측
/// 2. **같은 식별자가 홈 리전에도 있으면 다른 DB 의 슬로우로그를 파싱해 엉뚱한
///    `instance_id` 로 저장한다** — 원문 리터럴을 포함한 오귀속이고, prd↔dev 경계를
///    넘을 수 있다
///
/// 그래서 리전을 함께 들고 **불일치를 fail-closed** 로 거부한다.
pub struct CloudWatchFetcher {
    client: aws_sdk_cloudwatchlogs::Client,
    /// 이 클라이언트가 붙는 리전.
    region: String,
    /// 이 클라이언트의 자격증명이 속한 계정.
    ///
    /// 리전과 **같은 이유**로 들고 있다. 로그 그룹 이름에는 계정도 없으므로,
    /// 크로스 계정 인스턴스를 우리 계정 클라이언트로 조회하면 같은 이름의 **다른
    /// 계정 DB** 로그를 읽어 엉뚱한 `instance_id` 로 저장한다 — 리터럴을 포함한
    /// 오귀속이다. 리전 불일치를 fail-closed 로 막아 두고 계정은 열려 있었다
    /// (교차 리뷰가 잡았다).
    account: String,
    /// 한 번에 가져올 이벤트 상한. 레이트 리밋과 메모리를 함께 막는다.
    max_events: i32,
}

impl CloudWatchFetcher {
    pub fn new(
        client: aws_sdk_cloudwatchlogs::Client,
        region: impl Into<String>,
        account: impl Into<String>,
    ) -> Self {
        Self {
            client,
            region: region.into(),
            account: account.into(),
            max_events: 10_000,
        }
    }
}

/// 리전별 CloudWatch 페처 묶음.
pub struct RegionalFetchers {
    by_region: std::collections::BTreeMap<String, CloudWatchFetcher>,
}

impl RegionalFetchers {
    pub fn new(by_region: std::collections::BTreeMap<String, CloudWatchFetcher>) -> Self {
        Self { by_region }
    }
}

#[async_trait::async_trait]
impl SlowLogFetcher for RegionalFetchers {
    async fn fetch(&self, instance: &InstanceId, since_ms: EpochMs) -> Result<LogChunk> {
        let region = instance.region();
        // **다른 리전 클라이언트로 대신하지 않는다.** 같은 식별자가 그 리전에도
        // 있으면 남의 DB 로그를 파싱해 엉뚱한 인스턴스로 저장한다.
        let f = self.by_region.get(region).ok_or_else(|| {
            dbmon_core::error::DomainError::Unavailable {
                dependency: "cloudwatchlogs",
                reason: format!("{region}: 이 리전의 로그 클라이언트가 없다"),
            }
        })?;
        f.fetch(instance, since_ms).await
    }
}

#[async_trait::async_trait]
impl SlowLogFetcher for CloudWatchFetcher {
    async fn fetch(&self, instance: &InstanceId, since_ms: EpochMs) -> Result<LogChunk> {
        check_scope(instance, &self.region, &self.account)?;
        let group = slowquery_log_group(instance)?;
        let out = self
            .client
            .filter_log_events()
            .log_group_name(&group)
            .start_time(since_ms)
            .limit(self.max_events)
            .send()
            .await
            .map_err(|e| {
                use aws_sdk_cloudwatchlogs::error::ProvideErrorMetadata as _;
                // **로그 그룹이 없는 것은 장애가 아니다.**
                //
                // 슬로우로그가 한 번도 쓰이지 않았거나(RDS 는 첫 기록 때 그룹을 만든다)
                // 로그 내보내기가 꺼져 있으면 `ResourceNotFoundException` 이 온다.
                // 그걸 의존 서비스 장애로 올리면 **매 백필 주기마다 경고가 쌓이고**
                // 재시도 대상으로 분류된다 — 존재하지 않는 그룹을 계속 두드린다.
                //
                // `Unsupported` 로 구분하면 호출부가 "이 인스턴스는 원천이 없다" 로
                // 한 번만 말하고 넘어갈 수 있다.
                if e.code() == Some("ResourceNotFoundException") {
                    return dbmon_core::error::DomainError::Unsupported {
                        what: "slowlog_group".into(),
                        // 그룹 이름을 남긴다 — 스크럽이 메시지를 지우므로 이게 유일한 단서다.
                        reason: format!(
                            "로그 그룹 `{group}` 이 없다 — 슬로우로그가 아직 쓰이지 않았거나 \
                             로그 내보내기가 꺼져 있다"
                        ),
                    };
                }
                dbmon_core::error::DomainError::Unavailable {
                    dependency: "cloudwatchlogs",
                    // 서비스 메시지를 살린다(스크럽 통과). `Debug` 만 넘기면 정작 필요한
                    // 문장이 `Some('?')` 가 된다 — Bedrock 에서 같은 것을 겪었다.
                    reason: e
                        .message()
                        .map(crate::telemetry::scrub)
                        .unwrap_or_else(|| crate::telemetry::scrub(&format!("{e:?}"))),
                }
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

/// 로컬 슬로우로그 파일 크기 상한 (32MB). 넘으면 거부한다.
const MAX_LOCAL_FILE_BYTES: u64 = 32 * 1024 * 1024;

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
        let fail = |reason: String| dbmon_core::error::DomainError::Unavailable {
            dependency: "slowlog_file",
            reason: crate::telemetry::scrub(&reason),
        };

        // **일반 파일인지 확인한다.** FIFO(`mkfifo`)나 `/dev/stdin` 을 주면
        // `read_to_string` 이 영구 블록되고 **리더 루프 전체가 멈춘다** —
        // 고아 스윕·탐색·백필이 함께 죽는다.
        let meta = tokio::fs::metadata(&self.path)
            .await
            .map_err(|e| fail(format!("{}: {e}", self.path.display())))?;
        if !meta.is_file() {
            return Err(fail(format!(
                "{}: 일반 파일이 아니다 (FIFO·장치 파일은 루프를 멈춘다)",
                self.path.display()
            )));
        }
        if meta.len() > MAX_LOCAL_FILE_BYTES {
            return Err(fail(format!(
                "{}: 파일이 너무 크다 ({} 바이트, 상한 {MAX_LOCAL_FILE_BYTES})",
                self.path.display(),
                meta.len()
            )));
        }

        // 파일 전체를 읽고 파서가 시간 필터를 하도록 둔다 — 로컬 파일은 작고,
        // 오프셋 추적을 흉내내면 프로덕션과 다른 코드를 검증하게 된다.
        let text = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::fs::read_to_string(&self.path),
        )
        .await
        .map_err(|_| fail(format!("{}: 읽기 시간 초과", self.path.display())))?
        .map_err(|e| fail(format!("{}: {e}", self.path.display())))?;
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

    /// **리전이 다르면 조회하지 않는다.**
    ///
    /// 로그 그룹 이름에 리전이 없으므로 틀린 리전 클라이언트로 조회하면 같은 이름의
    /// 다른 DB 로그를 읽어 엉뚱한 인스턴스로 저장한다.
    #[test]
    fn a_region_mismatch_is_refused() {
        let e = check_scope(&instance(), "us-east-1", "123456789012").expect_err("거부");
        assert!(matches!(
            e,
            dbmon_core::error::DomainError::InvalidInput { .. }
        ));
    }

    /// **계정이 다르면 조회하지 않는다.**
    ///
    /// 크로스 계정 수집은 배선되지 않았다 — 클라이언트는 우리 계정 자격증명으로 돈다.
    /// 그대로 조회하면 우리 계정에서 같은 이름을 찾고, 있으면 **남의 DB 슬로우로그를
    /// 그 인스턴스의 것으로 저장한다.** 리터럴을 포함한 오귀속이다.
    #[test]
    fn a_cross_account_instance_is_refused() {
        let other = InstanceId::new("999988887777", "ap-northeast-2", "orders-01").expect("id");
        let e = check_scope(&other, "ap-northeast-2", "123456789012").expect_err("거부");
        match e {
            dbmon_core::error::DomainError::Unsupported { what, reason } => {
                assert_eq!(what, "cross_account_slowlog");
                // 사유가 두 계정을 모두 말해야 한다 — 어느 쪽이 틀렸는지 알 수 있게.
                assert!(reason.contains("999988887777"), "{reason}");
                assert!(reason.contains("123456789012"), "{reason}");
            }
            other => panic!("예상과 다르다: {other:?}"),
        }
    }

    /// 같은 계정·리전이면 통과한다 (가드가 정상 경로를 막지 않는다).
    #[test]
    fn the_same_scope_passes() {
        check_scope(&instance(), "ap-northeast-2", "123456789012").expect("통과");
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
