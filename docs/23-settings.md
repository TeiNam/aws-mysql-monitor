# 23. 운영 설정 (화면에서 바꾸는 값)

이 문서는 **재시작 없이 바뀌는 설정**을 다룬다. 배포 단위로 고정되는 값(`dbmon.toml`)은
[18 개발 환경](18-dev-environment.md)과 [14 인프라](14-infrastructure.md)에 있다.

| 저장 위치 | 무엇 | 반영 |
|---|---|---|
| `dbmon.toml` (파일) | 테이블 이름, 바인드 주소, 리터럴 정책, **인증 끄기 허용 여부** | 재시작 |
| DynamoDB `CFG/GLOBAL` | 알림, 탐색 범위, 로그인 방식, AI 모델 | **30초** |

두 곳으로 나눈 이유는 되돌리는 비용이 다르기 때문이다. 파일 설정이 틀리면 배포로
고치고, 운영 설정이 틀리면 화면에서 고친다. 후자를 파일에 두면 리전 하나를 추가하려고
배포 파이프라인을 돌려야 한다(FR-OPS-05).

## 1. 화면

`설정`(오른쪽 위 톱니) → 네 절.

- **읽기는 viewer, 쓰기는 admin.** 뷰어가 읽을 수 있어야 "왜 이 인스턴스가 목록에
  없는지"(탐색 범위 밖)를 알 수 있다.
- 편집은 **초안**이다. `저장` 을 누를 때 한 번에 간다 — 글자마다 저장하면 리전 목록을
  지우는 중간 상태가 그대로 적용된다.
- 두 관리자가 같은 화면을 열어 두고 각자 저장하면 **나중 저장이 거부된다**(409).
  화면이 "다시 읽어라" 를 말한다.

## 2. 알림 (Slack)

### 2.1 비밀은 설정에 담지 않는다

웹훅 URL·봇 토큰은 **Secrets Manager** 에 두고 설정에는 그 이름/ARN 만 적는다
([10 §3.4](10-alerting.md)). 이 설정 항목은 viewer 도 읽고 DynamoDB 백업·CloudTrail 로도
흐르므로, 값을 두면 통제할 수 없는 곳으로 퍼진다.

```bash
# 웹훅 방식
aws secretsmanager create-secret \
  --name dbmon/channel/slack \
  --secret-string '{"webhook_url":"https://hooks.slack.com/services/T000/B000/xxxx"}'

# 봇 방식
aws secretsmanager create-secret \
  --name dbmon/channel/slack \
  --secret-string '{"bot_token":"xoxb-…"}'
```

그 다음 화면에 `dbmon/channel/slack` 을 적는다. **URL·토큰을 직접 붙여넣으면 검증이
거부한다** — 실수로 붙여넣는 경로를 막는 것이 그 검사의 목적이다.

태스크 롤에는 접두어로 좁힌 읽기 권한이 필요하다
(`enable_notifications = true`, `channel_secret_prefix`).

### 2.2 문구

자리표시자: `{emoji}` `{severity}` `{env}` `{instance}` `{title}` `{detail}` `{link}` `{at}`

**목록에 없는 이름은 저장이 거부된다.** `{instnace}` 를 그대로 렌더하면 알림에 그 글자가
박혀 나가고, 아무도 그걸 "설정 오류" 로 읽지 않는다. 화면의 미리보기가 예시 값으로
렌더해 보여준다.

알림 본문에는 **정규화된 SQL 만** 담긴다(T-23). 리터럴은 Slack 같은 제3자 SaaS 로
나가지 않고, 딥링크 뒤의 인증·권한·감사가 걸린 화면에만 있다.

### 2.3 아직 없는 것

**규칙 평가와 발송이 배선되지 않았다.** 이 절은 채널과 문구를 저장한다 —
[10 알림](10-alerting.md)의 규칙 모델·상태 머신·억제는 별 마일스톤이다. 지금 저장한 값은
그 엔진이 붙을 때 그대로 쓰인다.

## 3. 탐색 범위 (리전 · 계정)

### 3.1 리전

비워 두면 **이 워커가 사는 리전 하나**만 본다(그것도 비면 `aws.target_regions`).

"전체 리전" 이 없는 이유: 리전마다 `DescribeDBInstances` 를 부르므로 30개 리전이면 매
탐색이 30배가 되고, 대부분은 RDS 가 하나도 없는 리전이다. 기본값이 "전체" 면 아무도 그
비용을 의도하지 않은 채 지불한다.

탐색된 리전이 둘 이상이면 머리말에 **리전 선택기**가 생긴다. 고른 범위는 인스턴스를
다루는 화면 전부에 적용된다. 리전 코드 옆에 이름을 붙이는 이유는 `ap-northeast-1` 과
`-2` 가 한 글자 차이라서다.

### 3.2 다른 계정 (mgmt → 대상)

**목록 조회는 네트워크와 무관하다.** `sts:AssumeRole` + RDS API 이므로 VPC 피어링·TGW 가
없어도 목록이 뜬다. 피어링이 필요한 것은 그 다음 단계, **DB 에 붙어 수집할 때**다.
그래서 "목록에는 뜨는데 수집이 `unreachable`" 인 상태가 정상적으로 존재한다.

대상 계정에 만들 역할:

```json
// 신뢰 정책 — mgmt 계정의 태스크 롤만
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": { "AWS": "arn:aws:iam::<mgmt-account>:role/dbmon-task" },
    "Action": "sts:AssumeRole",
    "Condition": { "StringEquals": { "sts:ExternalId": "dbmon" } }
  }]
}
```

```json
// 권한 — 읽기만. DB 접속은 별 경로(rds-db:connect)다.
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": [
      "rds:DescribeDBInstances",
      "rds:DescribeDBClusters",
      "rds:ListTagsForResource",
      "cloudwatch:GetMetricData"
    ],
    "Resource": "*"
  }]
}
```

mgmt 쪽 태스크 롤에는 `discovery_account_ids` 를 채워 `sts:AssumeRole` 을 붙인다.
**화면의 계정 목록과 같아야 한다** — 여기 없는 계정을 화면에 넣으면 탐색이 AccessDenied 로
실패하고 목록에 뜨지 않는다.

역할 이름은 **이름만** 입력한다(ARN 아님). 계정 번호와 합쳐 서버가 ARN 을 만든다 —
임의 ARN 을 받으면 오타 하나로 남의 계정을 찌를 수 있다.

### 3.3 인스턴스 키에 계정이 들어간다

`InstanceId` 는 `계정/리전/식별자` 다. 크로스 계정 인스턴스를 우리 계정 키로 저장하면
같은 이름의 DB 가 두 계정에 있을 때 하나가 다른 하나를 덮어쓴다. 그래서 탐색기마다
자기 계정을 들고 다닌다.

## 4. 로그인

| 방식 | 상태 | 설명 |
|---|---|---|
| 공유 토큰 | **동작** | 배포 설정의 토큰. 로컬 개발은 루프백에서 토큰 없이 통과 |
| Cognito | **설정만** | 검증기(JWKS→서명 검증)가 아직 없다 — 고르면 토큰 방식으로 적용된다 |
| 인증 없음 | 두 곳 허용 시 동작 | VPN·사설 ALB 뒤의 내부 도구 |

### 4.1 인증 끄기는 두 곳이 허용해야 한다

인증을 끄는 화면은 인증 뒤에 있다. 끄는 순간 그 화면도 누구에게나 열리고, 되돌리려면
다시 켤 권한이 필요한데 그걸 판정할 근거가 사라진 상태다. 클릭 한 번으로 갈 수 있는
상태여서는 안 된다.

```toml
# dbmon.toml — 첫 번째 허용 (배포)
[http]
allow_auth_disable = true
```

그 다음 화면에서 `인증 없음` 을 고른다. 파일이 허용하지 않으면 화면의 그 선택은
**잠긴다**(고르게 해 두고 무시하면 "저장했는데 안 꺼진다" 로 보인다).

인증이 꺼지면 접근할 수 있는 누구나 **admin** 이다. 역할 놀이를 하지 않는 이유:
주체를 모르면 역할을 나눌 근거가 없고, viewer 로 떨어뜨리면 통제하는 것처럼 보이면서
정작 아무나 들어와 있는 상태가 된다. 대신 화면과 기동 로그가 크게 알린다.

### 4.2 Cognito

풀 ID·클라이언트 ID·리전·도메인은 **비밀이 아니다** — 로그인 전에 브라우저가 알아야
하는 공개 값이다. 클라이언트 시크릿을 쓰는 앱 클라이언트는 이 화면에 넣지 않는다
(SPA 는 시크릿을 쓰지 않는다).

리전을 비우면 풀 ID 접두어(`ap-northeast-2_…`)에서 읽는다.

## 5. AI 튜닝 (Bedrock)

### 5.1 무엇을 하는가

계획 화면(`Plan` → 레코드 선택)의 `Query Execution Plan` 카드에 **Tuning** 버튼이 있다.
누르면:

1. 실행계획의 참조 테이블을 뽑는다(최대 10개)
2. 대상 DB 에서 명세를 읽는다 — `SHOW CREATE TABLE`,
   `information_schema.STATISTICS`(인덱스 컬럼 순서·카디널리티),
   `information_schema.TABLES`(행수·크기·통계 갱신 시각)
3. 정규화된 SQL + 마스킹된 실행계획 + 명세를 Bedrock 에 보낸다
4. 응답을 검증해 계획 카드 아래에 표시하고 저장한다
5. `마크다운 다운로드` 에 그 절이 함께 들어간다

### 5.2 경계

- **리터럴을 보내지 않는다**(FR-AI-04). 저장된 원문이 있어도 다시 정규화해서 보내고,
  마스킹을 신뢰할 수 없으면(인용부호 미종료) SQL 을 통째로 뺀다.
- **DDL 을 실행하지 않는다**(FR-AI-09). 복사할 수 있는 문장까지가 경계다.
- `DROP INDEX` 제안은 **버린다**(T-34) — 그 인덱스를 쓰는 다른 쿼리를 우리는 보지 못한다.
  버렸다는 사실은 주의사항에 남는다.
- 컨텍스트에 **없는 테이블**에 인덱스를 걸라는 제안도 버린다(모델이 이름을 지어낸 경우).
- `ANALYZE TABLE` 을 실행하지 않는다(FR-AI-10). 통계가 낡아 보이면 그 사실이 주의사항에
  적힌다.

### 5.3 비용과 부하

- **사람이 누를 때만** 돈다. 화면을 열면 저장된 결과만 읽는다 — 방문마다 만들면
  청구서가 방문 수에 비례한다.
- 생성은 `operator` 이상이다. 대상 DB 에 쿼리 12회(테이블 10개 기준)를 던지고
  토큰을 쓰는 조작이다.
- 결과는 레코드에 딸려 **35일 뒤 함께 사라진다**(TTL).

### 5.4 모델

```toml
# terraform (40-compute)
enable_ai_tuning  = true
bedrock_model_ids = ["global.anthropic.claude-sonnet-5"]
```

화면에서 고른 모델이 이 목록에 없으면 호출이 AccessDenied 다. 모델을 `*` 로 열지 않는
이유: 계정의 모든 모델(단가가 전혀 다른 이미지·비디오 모델 포함)을 부를 수 있게 된다.

**모델 ID 를 코드에 박지 않는다**([11 §7](11-ai-advisor.md)) — 교체가 배포 없이 되어야
하고, 리전마다 쓸 수 있는 프로파일이 다르다. 결과에는 실제로 쓴 모델 ID 가 함께
저장되므로 "이 권고는 어느 모델이 낸 것인가" 에 답할 수 있다.

계정에서 쓸 수 있는 프로파일 확인:

```bash
aws bedrock list-inference-profiles \
  --query 'inferenceProfileSummaries[?contains(inferenceProfileId,`claude`)].inferenceProfileId'
```

### 5.5 실측에서 배운 것

- **`temperature` 를 보내지 않는다.** Claude 5 계열이 그 파라미터를 거부한다
  (`ValidationException: temperature is deprecated for this model`). 모델 ID 가 설정으로
  바뀌므로 "어떤 모델이 무엇을 받는가" 를 코드가 알 수 없어, 공통으로 받는 것만 보낸다.
- **인덱스가 없는 컬럼의 카디널리티는 얻을 수 없다.** `information_schema.STATISTICS` 는
  인덱스 컬럼만 담는다. `SELECT COUNT(DISTINCT col)` 은 대상 DB 풀스캔이라 하지 않는다 —
  모델이 그 한계를 주의사항에 적는다(실측으로 그렇게 나왔다).
- **테이블 이름이 별칭일 수 있다.** 실행계획의 `table_name` 은 `FROM orders o` 에서 `o`
  다. 그런 이름은 `information_schema` 에 없어 명세가 비고, 권고에 "스키마 없이
  분석했다" 가 적힌다. SQL 파서 폴백은 [17](17-roadmap-tasks.md) M10-2 다.
