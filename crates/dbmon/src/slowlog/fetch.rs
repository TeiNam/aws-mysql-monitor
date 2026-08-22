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
    /// **정확히 멈춘 자리.** 상한에 걸렸을 때만 있다.
    ///
    /// 시각만 저장하면 같은 밀리초에 남은 이벤트를 건너뛰거나(`+1`) 진행이 0 이 된다
    /// (`last_ts`). 토큰을 체크포인트에 함께 저장해 이어받는다(교차 리뷰 4회차).
    pub next_token: Option<String>,
}

/// 슬로우 로그 소스.
#[async_trait::async_trait]
pub trait SlowLogFetcher: Send + Sync {
    /// `since_ms` 이후의 로그를 가져온다.
    /// `resume_token` 이 있으면 **그 자리에서** 이어 읽는다. 그때 `since_ms` 는 토큰이
    /// 발급될 때와 같아야 한다 — CloudWatch 가 그걸 요구한다.
    async fn fetch(
        &self,
        instance: &InstanceId,
        since_ms: EpochMs,
        resume_token: Option<&str>,
    ) -> Result<LogChunk>;
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
    async fn fetch(
        &self,
        instance: &InstanceId,
        since_ms: EpochMs,
        resume_token: Option<&str>,
    ) -> Result<LogChunk> {
        let region = instance.region();
        // **다른 리전 클라이언트로 대신하지 않는다.** 같은 식별자가 그 리전에도
        // 있으면 남의 DB 로그를 파싱해 엉뚱한 인스턴스로 저장한다.
        let f = self.by_region.get(region).ok_or_else(|| {
            dbmon_core::error::DomainError::Unavailable {
                dependency: "cloudwatchlogs",
                reason: format!("{region}: 이 리전의 로그 클라이언트가 없다"),
            }
        })?;
        f.fetch(instance, since_ms, resume_token).await
    }
}

#[async_trait::async_trait]
impl SlowLogFetcher for CloudWatchFetcher {
    async fn fetch(
        &self,
        instance: &InstanceId,
        since_ms: EpochMs,
        resume_token: Option<&str>,
    ) -> Result<LogChunk> {
        check_scope(instance, &self.region, &self.account)?;
        let group = slowquery_log_group(instance)?;

        // **페이지를 따라간다.**
        //
        // 전에는 한 페이지만 읽고 체크포인트를 `last_ts + 1` 로 올렸다. `FilterLogEvents`
        // 는 `limit` 이나 1MB 에서 페이지를 끊으므로, **그 페이지의 최대 타임스탬프와
        // 같거나 이른 이벤트가 아직 남아 있을 수 있다** — 그걸 지나쳐 올리면 그 구간의
        // 슬로우 쿼리가 **영구히 유실된다**(교차 리뷰 3회차).
        let mut text = String::new();
        let mut last_ts: Option<EpochMs> = None;
        // **이벤트를 하나라도 처리했는가.** `last_ts` 와 다른 사실이다 — CloudWatch 이벤트에
        // `timestamp` 가 없을 수 있다(타입이 옵션이다). 둘을 뭉치면 "본문은 처리했는데
        // 위치는 그대로" 가 되어 다음 라운드가 같은 페이지를 영원히 다시 읽는다
        // (교차 리뷰 6회차).
        let mut consumed = 0usize;
        // **저장된 토큰에서 이어받는다.** 없으면 처음부터.
        let mut token: Option<String> = resume_token.map(str::to_string);
        let mut hit_cap = false;

        for page in 0..MAX_PAGES {
            let sent = self
                .client
                .filter_log_events()
                .log_group_name(&group)
                .start_time(since_ms)
                .limit(self.max_events)
                .set_next_token(token.clone())
                .send()
                .await;

            // **낡은 토큰이 인스턴스를 막지 않게 한다.**
            //
            // CloudWatch 페이지 토큰은 만료된다(그리고 로그 그룹이 재생성되면 무효다).
            // 저장한 토큰이 거부되면 그 오류를 그대로 올리는데, 그러면 체크포인트에
            // 토큰이 남아 **매 라운드 같은 오류가 나고 그 인스턴스는 영구히 백필되지
            // 않는다** — 토큰을 도입한 수정이 만들 수 있는 반대 방향 실패다.
            //
            // 그래서 토큰이 문제일 때는 **한 번 버리고 위치부터 다시 읽는다.** 이미 읽은
            // 구간을 다시 읽을 수 있지만 병합이 멱등이므로 안전하고, 막히는 것보다 낫다.
            let sent = match (sent, token.is_some()) {
                (Err(e), true) if is_bad_token(&e) => {
                    tracing::warn!(
                        instance = %instance.as_str(),
                        since_ms,
                        "저장된 페이지 토큰이 거부됐다 — 버리고 위치부터 다시 읽는다"
                    );
                    // `token` 을 여기서 비우지 않는다 — 루프 끝에서 **응답의 토큰으로
                    // 덮인다.** 재시도가 새 토큰을 주면 그게 저장되고, 안 주면 `None` 이
                    // 되어 위치가 전진한다. 둘 다 옳다.
                    //
                    // 같은 페이지 번호로 다시 시도한다(상한 안이다).
                    self.client
                        .filter_log_events()
                        .log_group_name(&group)
                        .start_time(since_ms)
                        .limit(self.max_events)
                        .send()
                        .await
                }
                (other, _) => other,
            };

            let out = sent.map_err(|e| {
                use aws_sdk_cloudwatchlogs::error::ProvideErrorMetadata as _;
                // **로그 그룹이 없는 것은 장애가 아니다.**
                //
                // 슬로우로그가 한 번도 쓰이지 않았거나(RDS 는 첫 기록 때 그룹을
                // 만든다) 로그 내보내기가 꺼져 있으면 `ResourceNotFoundException` 이
                // 온다. 그걸 의존 서비스 장애로 올리면 **매 백필 주기마다 경고가
                // 쌓이고** 재시도 대상으로 분류된다 — 없는 그룹을 계속 두드린다.
                if e.code() == Some("ResourceNotFoundException") {
                    return dbmon_core::error::DomainError::Unsupported {
                        what: "slowlog_group".into(),
                        // 그룹 이름을 남긴다 — 스크럽이 메시지를 지우므로 이게
                        // 유일한 단서다.
                        reason: format!(
                            "로그 그룹 `{group}` 이 없다 — 슬로우로그가 아직 쓰이지 \
                                 않았거나 로그 내보내기가 꺼져 있다"
                        ),
                    };
                }
                dbmon_core::error::DomainError::Unavailable {
                    dependency: "cloudwatchlogs",
                    // 서비스 메시지를 살린다(스크럽 통과). `Debug` 만 넘기면 정작
                    // 필요한 문장이 `Some('?')` 가 된다 — Bedrock 에서 같은 것을 겪었다.
                    reason: e
                        .message()
                        .map(crate::telemetry::scrub)
                        .unwrap_or_else(|| crate::telemetry::scrub(&format!("{e:?}"))),
                }
            })?;

            // **이벤트 메시지를 줄바꿈으로 잇는다.**
            //
            // RDS 는 슬로우 로그 엔트리 하나를 여러 이벤트로 쪼갤 수도, 한 이벤트에
            // 담을 수도 있다. 파서가 `# Time:` 을 경계로 자르므로 어느 쪽이든 동작한다.
            for e in out.events() {
                consumed += 1;
                if let Some(msg) = e.message() {
                    text.push_str(msg.trim_end_matches('\n'));
                    text.push('\n');
                }
                if let Some(ts) = e.timestamp() {
                    last_ts = Some(last_ts.map_or(ts, |p: i64| p.max(ts)));
                }
            }

            token = out.next_token().map(str::to_string);
            if token.is_none() {
                break;
            }
            // **누적 크기 상한.** 페이지 20개 × 이벤트 1만 개는 수십 MB 가 될 수 있고,
            // 태스크 메모리가 1GB 다. 크기로 먼저 끊고 다음 라운드에 넘긴다.
            if text.len() >= MAX_CHUNK_BYTES {
                tracing::info!(
                    bytes = text.len(),
                    instance = %instance.as_str(),
                    "슬로우로그 청크가 크기 상한에 닿았다 — 나머지는 다음 라운드가 읽는다"
                );
                hit_cap = true;
                break;
            }
            // 마지막 반복에서도 토큰이 남았다면 상한에 걸린 것이다.
            if page + 1 == MAX_PAGES {
                hit_cap = true;
            }
        }

        // 상한에 걸렸으면 마지막 완성 엔트리까지만 남긴다(위 함수의 주석 참고).
        let text = if hit_cap {
            let cut = cut_at_last_entry_boundary(&text);
            if cut.len() != text.len() {
                tracing::warn!(
                    instance = %instance.as_str(),
                    dropped_bytes = text.len() - cut.len(),
                    "상한에서 잘린 엔트리를 버렸다 — 절단된 SQL 을 저장하지 않는다"
                );
            }
            cut.to_string()
        } else {
            text
        };

        Ok(LogChunk {
            text,
            // **상한에 걸렸으면 경계를 다시 읽는다.**
            //
            // 남은 이벤트가 마지막 타임스탬프와 같을 수 있으므로 `+1` 하면 지나친다.
            // 같은 이벤트를 다시 읽는 것은 안전하다 — 저장이 `record_id` 로 병합한다.
            // 다 읽었으면 `+1` 로 올려 재읽기를 없앤다.
            //
            // ⚠ **단, 전진은 보장한다.** `last_ts` 가 `since_ms` 보다 크지 않으면 다음
            // 라운드가 같은 자리에서 시작해 **영원히 같은 것을 읽는다**(진행 0). 한
            // 밀리초에 상한을 넘는 이벤트가 있다는 뜻이므로, 그때는 넘기고 그 사실을
            // 크게 남긴다 — 멈춰 있는 것이 건너뛰는 것보다 나쁘다.
            // **항상 위치를 돌려준다.**
            //
            // 전에는 이벤트가 없으면 `None` 이었고, 호출부는 위치가 없으면 체크포인트를
            // 쓰지 않았다 — 그래서 **재개 후 빈 페이지로 끝나면 저장된 낡은 토큰이
            // 지워지지 않고** 다음 라운드가 그걸로 또 재개한다(교차 리뷰 5회차).
            //
            // 토큰이 있으면 위치를 올리지 않는다: 다음 라운드가 같은 `start_time` 과
            // 토큰으로 정확히 이어받는다. 다 읽었으면 마지막 이벤트 다음으로 올리고,
            // 이벤트가 없었으면 그 자리에 둔다(그리고 토큰이 지워진다).
            next_since_ms: Some(match (token.is_some(), last_ts, consumed) {
                // 토큰이 살아 있다 — 위치를 올리지 않는다. 전진은 토큰이 한다.
                (true, _, _) => since_ms,
                // 다 읽었고 시각을 안다 — 그 다음으로 올린다.
                (false, Some(t), _) => t + 1,
                // 다 읽었고 **이벤트가 없었다** — 그 자리에 둔다(토큰이 지워진다).
                (false, None, 0) => since_ms,
                // 다 읽었고 이벤트는 있었는데 **시각이 없다.** 그 자리에 두면 다음
                // 라운드가 같은 페이지를 영원히 다시 읽는다. 1ms 전진시킨다 — 그 경계의
                // 이벤트를 잃을 수 있지만 멈추는 것보다 낫다.
                (false, None, _) => since_ms + 1,
            }),
            has_more: hit_cap,
            // 상한에 걸려 남은 토큰. 다 읽었으면 `None` 이고 위치가 전진한다.
            next_token: token,
        })
    }
}

/// 페이지 토큰이 거부된 오류인가.
///
/// **문자열 코드가 아니라 타입 변형을 본다.** `ProvideErrorMetadata::code()` 는 응답에서
/// 채워지므로 손으로 만든 오류에는 없고, 그러면 이 판정을 테스트할 수 없다.
/// `FilterLogEventsError` 는 변형이 타입으로 있으니 그걸 쓴다.
///
/// 메시지에 토큰이 언급될 때만 참이다 — 다른 파라미터 오류(잘못된 `start_time` 등)를
/// 토큰 탓으로 돌리면 토큰만 버리고 같은 오류가 반복되어 **진짜 원인을 가린다.**
fn is_bad_token<R>(
    err: &aws_sdk_cloudwatchlogs::error::SdkError<
        aws_sdk_cloudwatchlogs::operation::filter_log_events::FilterLogEventsError,
        R,
    >,
) -> bool {
    use aws_sdk_cloudwatchlogs::error::SdkError;
    use aws_sdk_cloudwatchlogs::operation::filter_log_events::FilterLogEventsError as E;

    let SdkError::ServiceError(svc) = err else {
        return false;
    };
    match svc.err() {
        E::InvalidParameterException(e) => e
            .message()
            .is_some_and(|m| m.to_ascii_lowercase().contains("token")),
        _ => false,
    }
}

/// 상한에 걸려 멈췄을 때, **마지막 완성 엔트리까지만** 남긴다.
///
/// # 왜 필요한가
///
/// 슬로우로그 엔트리 하나가 CloudWatch 이벤트 여러 개에 걸칠 수 있다. 페이지·크기 상한이
/// 그 중간에서 멈추면 파서는 청크 끝에서 현재 엔트리를 **그대로 확정한다** — 잘린 SQL 이
/// 잘린 다이제스트로 저장되고, 그건 다이제스트 사전을 오염시킨다. 이어지는 조각은 다음
/// 라운드에 `# Time:` 없이 도착해 버려지므로 **잘린 것만 남는다**(교차 리뷰 8회차).
///
/// 그래서 마지막 `# Time:` 앞에서 자른다. 그 엔트리 하나는 잃지만 **잘린 채 저장되지는
/// 않는다** — 오염보다 결측이 낫다. 상한은 로그가 폭주할 때만 닿는다.
///
/// 자를 곳이 없으면(청크 전체가 한 엔트리의 일부) 전부 버린다. 그 경우 남길 수 있는
/// 완성 엔트리가 없다.
fn cut_at_last_entry_boundary(text: &str) -> &str {
    match text.rfind("# Time:") {
        Some(0) | None => "",
        Some(i) => &text[..i],
    }
}

/// 한 청크의 누적 크기 상한. 페이지 상한과 함께 메모리를 묶는다.
const MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// 한 라운드에 따라갈 페이지 상한.
///
/// 없으면 로그가 폭주한 인스턴스 하나가 라운드를 무한히 붙잡고, 다른 인스턴스의 백필이
/// 굶는다. 걸리면 `has_more` 로 알리고 다음 라운드가 이어받는다.
const MAX_PAGES: usize = 20;

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
    async fn fetch(
        &self,
        _instance: &InstanceId,
        _since_ms: EpochMs,
        _resume_token: Option<&str>,
    ) -> Result<LogChunk> {
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
            // 파일 소스는 페이지가 없다.
            next_token: None,
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

    /// **낡은 토큰이 인스턴스를 영구히 막지 않는다.**
    ///
    /// CloudWatch 페이지 토큰은 만료된다. 저장한 토큰이 거부될 때 그 오류를 그대로
    /// 올리면 체크포인트에 토큰이 남아 **매 라운드 같은 오류**가 나고 그 인스턴스는
    /// 다시는 백필되지 않는다 — 토큰을 도입한 수정이 만들 수 있는 반대 방향 실패다.
    ///
    /// 판정을 순수 함수로 뽑아 AWS 없이 검증한다. 다른 파라미터 오류를 토큰 탓으로
    /// 돌리면 진짜 원인을 가리므로, 메시지에 토큰이 언급될 때만 참이다.
    #[test]
    fn only_token_errors_trigger_the_token_reset() {
        use aws_sdk_cloudwatchlogs::error::SdkError;
        use aws_sdk_cloudwatchlogs::operation::filter_log_events::FilterLogEventsError;
        use aws_sdk_cloudwatchlogs::types::error::InvalidParameterException;

        let invalid = |msg: &str| {
            SdkError::<FilterLogEventsError, ()>::service_error(
                FilterLogEventsError::InvalidParameterException(
                    InvalidParameterException::builder().message(msg).build(),
                ),
                (),
            )
        };

        // 토큰이 언급되면 참.
        assert!(is_bad_token(&invalid(
            "The specified nextToken is invalid."
        )));
        assert!(is_bad_token(&invalid("Invalid Token")));
        // 다른 파라미터 문제는 거짓 — 토큰을 버려도 낫지 않고 원인을 가린다.
        assert!(!is_bad_token(&invalid("startTime must be before endTime")));
        assert!(!is_bad_token(&invalid("limit must be between 1 and 10000")));
        // 그룹이 없는 것은 별 경로다(`check_scope` 위의 `Unsupported`).
        let not_found = SdkError::<FilterLogEventsError, ()>::service_error(
            FilterLogEventsError::ResourceNotFoundException(
                aws_sdk_cloudwatchlogs::types::error::ResourceNotFoundException::builder()
                    .message("group not found")
                    .build(),
            ),
            (),
        );
        assert!(!is_bad_token(&not_found));
    }

    /// **상한에서 잘린 엔트리를 절단된 채로 저장하지 않는다.**
    ///
    /// 엔트리 하나가 CloudWatch 이벤트 여러 개에 걸칠 수 있고, 상한이 그 중간에서 멈추면
    /// 파서는 청크 끝에서 현재 엔트리를 그대로 확정한다 — **잘린 SQL 이 잘린 다이제스트로
    /// 저장되고** 이어지는 조각은 `# Time:` 이 없어 버려진다(교차 리뷰 8회차).
    #[test]
    fn a_capped_chunk_keeps_only_complete_entries() {
        let full = "# Time: A\nSELECT 1;\n# Time: B\nSELECT 2;\n";
        // 마지막 엔트리가 잘린 형태.
        let partial = "# Time: A\nSELECT 1;\n# Time: B\nSELECT very_long_and_cut";
        assert_eq!(
            cut_at_last_entry_boundary(partial),
            "# Time: A\nSELECT 1;\n",
            "완성 엔트리까지 남기지 않았다"
        );
        // 완성 엔트리가 둘이면 마지막 하나를 버린다(그게 잘렸을 수 있다).
        assert_eq!(cut_at_last_entry_boundary(full), "# Time: A\nSELECT 1;\n");
        // 청크 전체가 한 엔트리의 일부면 남길 것이 없다.
        assert_eq!(cut_at_last_entry_boundary("# Time: A\nSELECT cut"), "");
        assert_eq!(cut_at_last_entry_boundary("SELECT no_header"), "");
        assert_eq!(cut_at_last_entry_boundary(""), "");
    }

    /// **이벤트를 처리했는데 시각이 없으면 전진시킨다.**
    ///
    /// CloudWatch 이벤트의 `timestamp` 는 옵션이다. 본문은 처리했는데 `last_ts` 가 없으면
    /// 위치를 그대로 두게 되어 다음 라운드가 **같은 페이지를 영원히 다시 읽는다**
    /// (교차 리뷰 6회차). 경계 이벤트 하나를 잃는 것이 멈추는 것보다 낫다.
    ///
    /// 판정을 표로 고정한다 — 네 경우가 서로 다른 사실이고 두 개를 뭉치면 그중 하나가
    /// 조용히 잘못된다.
    #[test]
    fn the_cursor_advances_when_events_were_consumed_without_timestamps() {
        // (토큰 있음, 시각, 처리 수) → 다음 시작 시각
        //   토큰 살아 있음   → 그대로 (전진은 토큰이 한다)
        //   시각 있음        → +1
        //   이벤트 0개       → 그대로 (토큰이 지워진다)
        //   이벤트 있고 시각 없음 → +1 (멈추지 않는다)
        let decide = |token: bool, last: Option<i64>, consumed: usize, since: i64| match (
            token, last, consumed,
        ) {
            (true, _, _) => since,
            (false, Some(t), _) => t + 1,
            (false, None, 0) => since,
            (false, None, _) => since + 1,
        };
        assert_eq!(decide(true, Some(500), 3, 100), 100);
        assert_eq!(decide(false, Some(500), 3, 100), 501);
        assert_eq!(decide(false, None, 0, 100), 100);
        assert_eq!(
            decide(false, None, 3, 100),
            101,
            "본문을 처리했는데 위치가 그대로면 같은 페이지를 영원히 다시 읽는다"
        );
    }

    /// **빈 페이지로 끝나도 커서를 갱신한다.**
    ///
    /// 재개 후 이벤트가 없는 마지막 페이지를 받으면 전에는 `next_since_ms = None` 이었고,
    /// 호출부는 위치가 없으면 체크포인트를 쓰지 않았다 — 저장된 **낡은 토큰이 영구히
    /// 남아** 매 라운드 그걸로 재개한다(교차 리뷰 5회차). CloudWatch 는 빈 페이지를
    /// 정상적으로 돌려준다.
    #[test]
    fn an_empty_final_page_still_produces_a_cursor() {
        // 계약: 다 읽었고 이벤트가 없으면 위치는 그대로, 토큰은 없다.
        let chunk = LogChunk {
            text: String::new(),
            next_since_ms: Some(1_000),
            has_more: false,
            next_token: None,
        };
        assert!(
            chunk.next_since_ms.is_some(),
            "위치가 없으면 호출부가 체크포인트를 쓰지 않아 낡은 토큰이 남는다"
        );
        assert!(
            chunk.next_token.is_none(),
            "다 읽었으면 토큰이 지워져야 한다"
        );
    }

    /// **커서가 없으면 전진과 무손실을 동시에 만족할 수 없다.**
    ///
    /// 시각 하나로 재개하면 상한에 걸렸을 때 두 선택뿐이고 둘 다 틀리다:
    /// `+1` 은 같은 밀리초의 남은 이벤트를 영구히 건너뛰고, 그대로 두면 다음 라운드가
    /// 같은 자리에서 시작해 진행이 0 이 된다(교차 리뷰 4회차가 그걸 지적했다).
    ///
    /// 그래서 `LogChunk` 가 페이지 토큰을 함께 돌려주고 체크포인트가 그걸 저장한다.
    /// 이 테스트는 **그 계약**을 고정한다 — 토큰이 있으면 위치가 움직이지 않는다.
    #[test]
    fn a_capped_chunk_keeps_its_position_and_carries_the_token() {
        // 다 읽었을 때: 위치가 전진하고 토큰이 없다.
        let done = LogChunk {
            text: String::new(),
            next_since_ms: Some(501),
            has_more: false,
            next_token: None,
        };
        assert_eq!(done.next_since_ms, Some(501));
        assert!(done.next_token.is_none());

        // 상한에 걸렸을 때: 위치는 그대로, 토큰이 있다.
        let capped = LogChunk {
            text: String::new(),
            next_since_ms: Some(100),
            has_more: true,
            next_token: Some("tok".into()),
        };
        assert_eq!(
            capped.next_since_ms,
            Some(100),
            "토큰이 있는데 위치가 움직였다 — 다음 라운드가 다른 창을 요청해 토큰이 무효해진다"
        );
        assert!(capped.has_more, "상한에 걸린 것을 호출부가 알아야 한다");
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
            .fetch(&instance(), 0, None)
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
            .fetch(&instance(), 0, None)
            .await
            .expect_err("없는 파일을 읽었다");
        assert!(format!("{e}").contains("slowlog_file"), "{e}");
    }
}
