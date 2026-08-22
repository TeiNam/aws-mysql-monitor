# 08. 보안과 인증

## 1. 위협 모델

| # | 위협 | 영향 | 대응 |
|---|---|---|---|
| T-01 | 인증 없이 웹 접근 | 운영 SQL·데이터 리터럴 노출 | Cognito 필수, `/healthz` 외 전 엔드포인트 인증 |
| T-02 | 저장된 SQL 텍스트의 운영 데이터 리터럴 유출 | PII·기밀 노출 | `masked` 정책, 환경별 접근 권한 분리, KMS 암호화 |
| T-03 | 마스터 자격증명 유출 | 대상 DB 전체 장악 | 저장하지 않음, 타입으로 로깅 차단, 부트스트랩 후 즉시 폐기 |
| T-04 | 모니터링 계정 오용으로 운영 데이터 조회 | 데이터 유출 | 최소 권한 모드 기본, `rds-db:connect`를 앱에만 부여 |
| T-05 | Bedrock으로 운영 데이터 전송 | 외부 유출 | 리터럴 미전송 기본, Guardrail, 옵트인 + 감사 |
| T-06 | 앱의 IAM Role 탈취 | RDS 변경, 시크릿 열람 | `ModifyDB*` 기본 미부여, 읽기 전용 위주, 권한 경계 |
| T-07 | Athena SQL 인젝션 | 다른 테이블 조회, 데이터 유출 | 파라미터 바인딩 + 식별자 화이트리스트 |
| T-08 | `EXPLAIN FOR CONNECTION` 문장에 인젝션 | 대상 DB에서 임의 SQL 실행 | 연결 ID를 `u64` 타입으로만 받아 포맷 |
| T-09 | 알림 채널 웹훅 URL 유출 | 스팸·피싱 | Secrets Manager 저장, UI 재표시 금지 |
| T-10 | 권한 상승 (viewer → admin) | 설정 변조, 부트스트랩 실행 | 서버 측 RBAC, 클라이언트 신뢰 금지 |
| T-11 | 로그·에러 메시지의 민감정보 | 유출 | 로그 마스킹 계층, 에러 메시지에서 SQL 리터럴 제거 |
| T-12 | 웹훅/딥링크를 통한 SSRF | 내부 메타데이터 서비스 접근 | 알림 대상 URL 도메인 허용목록, IMDSv2 강제, 링크로컬 차단 |
| T-13 | WebSocket 인증 우회 | 실시간 데이터 무단 수신 | 연결 후 첫 메시지 토큰 검증, 미인증 연결 5초 후 종료 |
| T-14 | 관측 대상 DB에 부하 유발 (내부 오류) | 서비스 영향 | 쿼리 타임아웃, `max_execution_time`, 레이트 리밋, 서킷 브레이커 |
| T-15 | 부트스트랩 오작동으로 프로덕션 DB 변경 | 서비스 영향 | dry-run, SQL 전문 표시, 타이핑 확인, 감사 로그 |
| **T-16** | **실행계획 JSON의 `attached_condition` 등에 리터럴이 남아 `masked` 정책을 우회** | PII 유출 (DynamoDB·S3·Iceberg·Bedrock·브라우저 캐시 전 경로) | 플랜 정규화 단계에서 값 보유 필드 마스킹. [§6.2](#62-실행계획-리터럴-마스킹-t-16) |
| T-17 | Athena 조회 경로(`/api/queries`)가 직렬화 마스킹·환경 스코프·역할을 우회 | 감사 로그·전체 SQL 텍스트 무단 조회 | 쿼리 타입별 권한 표, 결과에도 마스킹·스코프 적용, `execution_id`를 요청자에 바인딩 |
| T-18 | `monitor_user` 등 부트스트랩 식별자를 통한 SQL 인젝션 (마스터 권한으로 실행) | 대상 DB 장악 | 식별자 화이트리스트 + 다중 문장 검사. [§7](#7-입력-검증-nfr-s-08) |
| T-19 | 폴백 비밀번호가 plan 응답·감사 로그의 SQL 전문에 평문 노출 | 자격증명 유출 (3년 보관) | 렌더 시 `IDENTIFIED BY '<redacted>'` 정규화 |
| T-20 | IdP 속성·Pre-Token Lambda·매핑 표 조작으로 admin 승격 | 전체 권한 탈취 | 토큰 클레임 ∩ 서버 측 USER 레코드. `custom:groups` 쓰기 금지. 매핑 표 쓰기 Deny |
| T-21 | 앱 Role로 감사 로그 삭제·위조 | 내부자 대응 증거 소멸 | `AUDIT#*` 키 범위 Deny, 해시 체인, S3 Object Lock |
| T-22 | WebSocket이 직렬화 마스킹 계층을 우회. 토픽에 env 차원 없음 | 리터럴·타 환경 데이터 실시간 유출 | 브로드캐스트도 `Redactable` 통과. 토픽 인가 표 |
| T-23 | 알림 채널(Slack/Telegram)이 리터럴을 조직 밖으로 전송 | 외부 유출 (감사 없음) | 알림 본문은 `digest_text`만. 리터럴은 딥링크 뒤 |
| T-24 | 리포트 presigned URL이 무자격 bearer. S3는 CSP 헤더를 전달하지 않아 저장형 XSS | 데이터 유출 + XSS | 앱 프록시 + 인증·스코프·감사. autoescape 강제 |
| T-25 | S3 `dbmon-*` 와일드카드 → 타 계정 버킷 쓰기 가능. 로그·리포트 삭제 가능 | 유출 채널 + 증거 파괴 | 버킷 ARN 열거 + `aws:ResourceAccount` 조건. 로그 버킷 Delete 제외 |
| T-26 | `glue:*` + `Resource: "*"` → 타 팀 카탈로그 변조 | 데이터 레이크 오염 | `database/dbmon`, `table/dbmon/*`로 축소 |
| T-27 | `dbmon-auth-admin`이 웹 서버와 같은 Role → 서버 침해 시 정상 로그인 경로로 재진입 | 권한 상승 | 별도 Role + `AssumeRole` (핸들러 한정) |
| T-28 | 부트스트랩 plan→apply 사이에 `dbmon` 계정 선점 (`IF NOT EXISTS`가 통과) | 공격자가 아는 비밀번호 계정에 권한 부여 | 존재 여부·플러그인 검증 후 진행 차단. [07 §2.6](07-credentials-bootstrap.md) |
| T-29 | env 태그가 대상 팀(신뢰 경계 밖) 소유인데 리터럴 정책·접근 스코프를 결정 | 운영 리터럴 저장 시작, 잘못된 스코프 노출 | env 변경은 완화 방향으로 자동 적용하지 않는다. `unknown`은 prd로 취급 |
| T-30 | 마스터 자격증명이 WAF 로그·코어 덤프·serde 에러 메시지·힙에 잔류 | 자격증명 유출 | 부트스트랩 경로 WAF 본문 로깅 제외, `LimitCORE=0`, 커스텀 `Deserialize` |
| T-31 | Guardrail이 호출 파라미터로만 적용 → 코드 변경 한 번으로 PII 필터 없이 전송 | 외부 유출 | IAM `Condition`으로 Guardrail 강제 |
| T-32 | 커서가 서명되지 않은 `LastEvaluatedKey` → 위조·공유로 스코프 우회 | 무단 조회 + 내부 키 구조 노출 | HMAC(서버키, sub + 필터 + 키) + 만료 |
| T-33 | 토큰 폐기 경로 없음. WS는 최초 1회만 인증 → 강등·비활성이 즉시 반영 안 됨 | 퇴사자·오용 차단 실패 | `claims_version` 대조, WS 주기적 재검증 |
| T-34 | 어드바이저 프롬프트 인젝션 (DDL COMMENT·리터럴이 원천). `drop_index`가 허용 목록에 있음 | 사용 중 인덱스 삭제 권고 → 프로덕션 사고 | DB 유래 텍스트를 데이터로 구분, `drop_index` 기본 비활성 + 사용 통계 교차 검증 |
| T-35 | viewer가 Athena 일일 스캔 한도를 소진해 리포트·과거 조회 마비 | 가용성 (비용 DoS) | 사용자별 일일 스캔 예산, 리포트 워크그룹 분리 |
| T-36 | `masked` 정확성이 정규화기 에뮬레이션에 의존. 파싱 실패 시 원문 로깅 | PII 유출 | 저장 전 "리터럴 잔존 0" 후조건 assert. 실패 시 원문 미로깅 |
| **T-37** | **개발계 앱이 같은 계정의 프로덕션 RDS를 탐색·접속** — `rds:DescribeDBInstances`는 리소스 조건을 못 걸고, `rds-db:connect` on `dbuser:*/dbmon`은 전 인스턴스를 커버한다 | dev 자격증명으로 prd 데이터 접근 | 3중: ① VPC 격리(피어링 없음, 1차) ② 탐색 필터(`allowed_vpc_ids` + 태그 + 이름 거부, AND) ③ dev는 `rds-db:connect`에 `DbiResourceId` 열거 ([18 §6](18-dev-environment.md)) |
| T-38 | dev 배포가 계정 전역 설정을 변경해 prd에 영향 | 서비스 영향 | 계정 레벨 리소스(S3 퍼블릭 차단, GuardDuty, Config, default VPC, CloudTrail, SCP)를 Terraform 관리 대상에서 제외 ([18 §6.5](18-dev-environment.md)) |
| T-39 | dev 앱이 퍼블릭 서브넷 + 퍼블릭 IP를 갖는다 | 직접 공격 표면 | 보안 그룹 인바운드 **규칙 0개**. SSM 포트 포워딩만. IMDSv2 강제(퍼블릭 IP가 있으므로 더 중요) |

**신뢰 경계** — 단일 조직이지만 다음은 경계 밖이다:
- **대상 RDS·태그·보안그룹을 소유한 다른 팀** → T-29, T-28, 호스트 와일드카드
- **Slack / Telegram / Bedrock / S3 presigned 수신자** → T-23, T-24, T-31
- **환경 스코프와 `can_see_literals`는 내부 데이터 경계다** → T-17, T-22, T-32

멀티 테넌시 관련 위협(테넌트 격리, 테넌트별 키, 요금 격리)은 범위 밖이다.

## 2. Cognito 구성

### 2.1 User Pool

| 설정 | 값 | 근거 |
|---|---|---|
| 자가 가입 | **비활성** | 관리자 초대만 (FR-AUT-01) |
| 로그인 식별자 | 이메일 | |
| 비밀번호 정책 | 최소 12자, 대·소문자·숫자·특수문자 필수 | FR-AUT-02 |
| 임시 비밀번호 유효기간 | 3일 | |
| MFA | TOTP, 기본 `OPTIONAL` → 설정으로 `REQUIRED` 전환 | FR-AUT-05 |
| 계정 복구 | 이메일만 | SMS는 SIM 스와핑 위험 |
| 고급 보안 | 활성 (자격증명 유출 탐지, 적응형 인증) | |
| 삭제 방지 | 활성 | |
| 사용자 존재 오류 방지 | 활성 (계정 열거 방지) | |

### 2.2 App Client (SPA)

| 설정 | 값 |
|---|---|
| 클라이언트 타입 | **퍼블릭 (시크릿 없음)** |
| 인증 플로우 | Authorization Code + PKCE만. Implicit 비활성 |
| 스코프 | `openid`, `email`, `profile` |
| 콜백 URL | `https://<도메인>/auth/callback` (프로덕션), `http://localhost:5173/auth/callback` (개발) |
| 로그아웃 URL | `https://<도메인>/` |
| 액세스 토큰 유효기간 | 60분 |
| ID 토큰 유효기간 | 60분 |
| 리프레시 토큰 유효기간 | 8시간 (설정 가능) |
| 토큰 회전 | 활성 (리프레시 토큰 재사용 감지) |
| 명시적 인증 플로우 | `ALLOW_REFRESH_TOKEN_AUTH`만. `ALLOW_USER_PASSWORD_AUTH` 비활성 (Hosted UI 경유 강제) |

`ALLOW_USER_PASSWORD_AUTH`를 끄는 이유: 앱이 비밀번호를 직접 받으면 그 코드 경로가
피싱·로깅 위험을 만든다. Hosted UI(Managed Login)에서만 비밀번호를 입력하게 한다.

### 2.3 IdP 페더레이션 (FR-AUT-03, FR-AUT-04)

**SAML 2.0**

```
CreateIdentityProvider(
  UserPoolId, ProviderName="okta", ProviderType="SAML",
  ProviderDetails={ MetadataURL 또는 MetadataFile, IDPSignout: "true" },
  AttributeMapping={ email: "...emailaddress", given_name: "...", family_name: "...",
                     custom:groups: "..." },
  IdpIdentifiers=["okta"])
```

**OIDC**

```
CreateIdentityProvider(
  UserPoolId, ProviderName="google-workspace", ProviderType="OIDC",
  ProviderDetails={ client_id, client_secret, attributes_request_method: "GET",
                    oidc_issuer, authorize_scopes: "openid email profile",
                    attributes_url_add_attributes: "false" },
  AttributeMapping={ email: "email", ... })
```

등록 후 `UpdateUserPoolClient`의 `SupportedIdentityProviders`에 추가해야 로그인 화면에
버튼이 나타난다. UI는 이 두 단계를 한 동작으로 처리한다.

**IdP 그룹 → Cognito 그룹 매핑** — IdP가 보내는 그룹 클레임을 커스텀 속성으로 받고,
Pre Token Generation Lambda 트리거에서 매핑 규칙에 따라 `cognito:groups`를 주입한다.
매핑 규칙은 `dbmon-config`에 저장한다.

- 이 Lambda는 이 프로젝트가 제공한다(Terraform 모듈 포함). 로직은 단순 매핑 테이블 조회.
- Lambda 없이 하려면 IdP 사용자를 Cognito 그룹에 수동 추가해야 한다 → 사용자가 늘면 불가.
- Lambda 실패 시 **그룹 없음으로 처리**한다(fail-closed). 권한 없는 사용자가 되며 로그인은 되지만
  아무것도 못 본다. 열린 실패보다 낫다.

**필요 IAM 권한 (관리자 UI용)**
```json
{
  "Effect": "Allow",
  "Action": [
    "cognito-idp:ListIdentityProviders", "cognito-idp:DescribeIdentityProvider",
    "cognito-idp:CreateIdentityProvider", "cognito-idp:UpdateIdentityProvider",
    "cognito-idp:DeleteIdentityProvider",
    "cognito-idp:DescribeUserPoolClient", "cognito-idp:UpdateUserPoolClient",
    "cognito-idp:ListUsers", "cognito-idp:AdminCreateUser", "cognito-idp:AdminDisableUser",
    "cognito-idp:AdminEnableUser", "cognito-idp:AdminAddUserToGroup",
    "cognito-idp:AdminRemoveUserFromGroup", "cognito-idp:ListGroups",
    "cognito-idp:AdminGetUser", "cognito-idp:AdminResetUserPassword"
  ],
  "Resource": "arn:aws:cognito-idp:*:<account>:userpool/<pool-id>"
}
```

`AdminDeleteUser`는 부여하지 않는다. 비활성화만 허용한다(감사 추적 보존).

## 2.9 지금 구현된 것 — Cognito 검증기가 들어왔다

**이 절 아래(§3)는 이제 구현이다.** `crates/dbmon/src/api/cognito.rs` 가 JWKS 조회 →
`kid` 캐시 → RS256 서명 → 클레임 검증 → `USER` 레코드 교집합을 수행한다. 다섯 가지
위조(`alg: none`, `alg: HS256` 혼동, 타 사용자 풀, ID 토큰 오용, 타 클라이언트)를
각각 테스트로 고정했다.

배선 여부는 **런타임 사실**이다 — `ApiState::cognito` 가 `Some` 인가로 판정한다.
`COGNITO_READY` 상수는 없앴다: 상수로 두면 배선을 끝내고도 상수를 안 바꿔서 꺼져 있는
상태가 가능하고, 실제로 그 상태가 한동안 있었다.

토큰 수단(아래 표)은 **그대로 살아 있다.** 순서는 `off` → 토큰 → Cognito 이고, 토큰이
앞인 이유는 전환 중에 화면이 닫히지 않아야 하기 때문이다.

| 모드 | 자격증명 | 조건 | `subject` |
|---|---|---|---|
| `local-dev` | 없음 | `dev` **∧** 루프백 바인드 | `local-dev` |
| `local-token` | 기동 로그의 무작위 토큰 | `dev` ∧ 비루프백 ∧ 비ECS | `local-dev` |
| `shared-token` | `http.auth_token` | 설정돼 있으면 어느 환경이든 | `shared-token` |
| `off` | 없음 | `allow_auth_disable` **∧** `auth.mode=off` | `anonymous` |

**전부 `admin` 이다.** 주체가 하나뿐인 자격증명으로 역할을 나눌 근거가 없다 — 아래 §5
의 역할 표는 Cognito 가 들어온 뒤에야 의미를 갖는다. 그때까지 T-20(클레임 단독 승격)과
T-33(폐기 반영)은 **적용 대상이 없다**: 나눌 역할도, 폐기할 세션도 없다.

`shared-token` 이 없던 동안 `token` 모드는 이름만 있었다 — `authenticate` 가
`dev_token` 만 비교했고 그건 dev 에서만 채워지므로, ECS 배포는 `off` 를 켜지 않는 한
**모든 요청이 401** 이었다. 설정 화면이 제공하는 모드로 들어올 수단이 코드에 없었던
것이고, 그게 이 절을 쓰게 만든 결함이다.

남은 일은 §3 그대로다. `COGNITO_READY` 가 `true` 가 되는 조건: JWKS 조회 → `kid` 캐시 →
RS256 서명 → 클레임 검증 → `AuthContext::intersect`. `CognitoSettings` 는 그 단계가
필요한 값(풀 ID·클라이언트 ID·리전·도메인)을 이미 담고 있다.

## 3. 토큰 검증 (FR-AUT-07)

```
1. Authorization: Bearer <access_token>  헤더에서 추출
2. JWT 헤더의 kid 로 JWKS에서 공개키 조회 (캐시)
3. 서명 검증 (RS256)
4. 클레임 검증:
     iss       == https://cognito-idp.<region>.amazonaws.com/<pool-id>
     token_use == "access"
     client_id == <app client id>          (access token 은 aud 없음, client_id 사용)
     exp       > now  (허용 오차 0. 시계 오차는 서버 NTP로 해결)
     nbf/iat   검증
5. sub → dbmon-config/USER/<sub> 조회 (서버 측 사용자 레코드)
     레코드 없음 → 권한 없음 (fail-closed)
     revoked_after_ms > token.iat  → 401 token_revoked
     claims_version != token 발급 시점 버전 → 401 token_stale (재로그인 유도)
6. 최종 역할 = min(토큰의 cognito:groups 로 유도한 역할,
                   USER 레코드의 role)          ← 교집합. 토큰만으로 승격 불가
7. 환경 스코프·can_see_literals = USER 레코드 값
```

**토큰 클레임을 단독 권한 근거로 쓰지 않는다 (T-20)** — `cognito:groups`는 IdP 속성 매핑과
Pre Token Generation Lambda를 거쳐 온다. 그 경로 중 하나라도 오작동·침해되면 임의의 그룹이
클레임에 들어올 수 있다. 그래서 **서버 측 `dbmon-config/USER/<sub>` 레코드와 교집합**한다.
토큰이 `dbmon-admin`을 주장해도 서버 레코드가 `viewer`면 viewer다.

- `USER` 레코드는 admin이 명시적으로 만든다. IdP로 처음 로그인한 사용자는 레코드가 없으므로
  **권한 없음**이며, admin이 승인해야 한다(승인 대기 목록을 UI에 표시).
- 매핑 표(`dbmon-config`의 IdP 그룹 매핑)와 `USER` 레코드 키 범위에 대해 앱 Role의 쓰기를
  **IAM에서 Deny**한다([§5.2](#52-dbmon-core-정책)). 이 두 곳은 별도 관리 경로
  (admin 전용 핸들러가 `AssumeRole`로 승격)로만 쓴다. 앱이 침해돼도 조용한 admin 승격이
  불가능해진다.
- IdP 속성 `custom:groups`를 App Client의 **쓰기 가능 속성에서 제외**하고 `mutable=false`로
  둔다. 사용자가 `UpdateUserAttributes`로 자기 그룹 소스를 바꿀 수 없게 한다.
- IdP 그룹 매핑은 **정확 일치 화이트리스트**만. 접두 일치·패스스루 금지.

**토큰 폐기 (T-33)** — `AdminDisableUser`·그룹 변경·스코프 축소 후에도 기존 access token은
최대 60분 유효하다. `USER` 레코드에 `revoked_after_ms`와 `claims_version`을 두고 검증 단계
5에서 대조한다. 이미 `USER` 레코드를 읽으므로 추가 비용이 없다.

- 권한 변경 시 `claims_version`을 올린다 → 기존 토큰이 즉시 무효.
- 비활성화 시 `revoked_after_ms = now` → 즉시 차단.
- **WebSocket은 5분마다 토큰·권한을 재검증**한다(최초 1회 인증만 하면 사실상 무기한이다).
  실패 시 연결을 종료하고 클라이언트가 재인증한다.
- 권한 변경 시 해당 사용자의 WS 연결을 강제 종료한다.

**JWKS 캐시** — 메모리 캐시, TTL 1시간. `kid`가 캐시에 없으면 즉시 1회 갱신
(키 회전 대응). 갱신 실패 시 캐시된 키로 계속 검증한다(Cognito 장애 내성).
갱신 요청은 `kid`당 1분에 1회로 레이트 리밋(캐시 미스 폭주 방어).

**access token vs id token** — 인증에는 **access token**을 쓴다. id token은 사용자 정보용이며
API 인증에 쓰면 만료·스코프 의미가 어긋난다. `email`은 id token에서 읽어 프론트가 표시만 한다.

### 3.1 `USER` 레코드 — 실제 항목 형식

**이 레코드가 없으면 토큰이 유효해도 못 들어온다** (fail-closed). Cognito 를 켜고
처음 로그인했을 때 401 이 나는 이유가 대개 이것이다.

`dbmon-config` 테이블:

| 속성 | 타입 | 값 |
|---|---|---|
| `PK` | S | `USER#<sub>` — Cognito 의 `sub`(UUID) |
| `SK` | S | `PROFILE` |
| `role` | S | `admin` / `operator` / `viewer` |
| `env_scope` | SS | `["dev","stg","prd"]` 중 볼 수 있는 것 |
| `can_see_literals` | BOOL | 리터럴 열람 |
| `claims_version` | N | 권한을 바꿀 때마다 올린다 |
| `revoked_after_ms` | N | 이 시각 **이전에 발급된** 토큰을 무효화 |
| `disabled` | BOOL | 참이면 즉시 거부 |

**없는 속성은 가장 낮은 권한으로 읽힌다** (`role` → `viewer`, `can_see_literals` →
거짓). `env_scope` 는 예외로 **비어 있으면 아무것도 못 본다** — 전체 허용으로 접으면
스코프를 지정하지 않은 레코드가 전 환경을 보게 된다.

```bash
# sub 확인
SUB=$(aws cognito-idp admin-get-user --user-pool-id <pool> --username <email> \
  --query 'UserAttributes[?Name==`sub`].Value' --output text)

# 레코드 생성 (앱이 아니라 사람이 만든다 — 앱 Role 은 이 키 범위에 쓰기 Deny 다)
aws dynamodb put-item --table-name dbmon-config-dev --item "{
  \"PK\": {\"S\": \"USER#$SUB\"},
  \"SK\": {\"S\": \"PROFILE\"},
  \"role\": {\"S\": \"admin\"},
  \"env_scope\": {\"SS\": [\"dev\", \"stg\", \"prd\"]},
  \"can_see_literals\": {\"BOOL\": true},
  \"claims_version\": {\"N\": \"0\"},
  \"disabled\": {\"BOOL\": false}
}"
```

#### `claims_version` 은 왜 토큰에 없는가

문서 §3 5단계는 "토큰 발급 시점 버전과 대조" 라고 적었다. 그러려면 Pre Token
Generation Lambda 가 버전을 클레임에 심어야 하는데, **그 Lambda 를 만들지 않기로
했다** — `AuthContext::intersect` 가 토큰 그룹 없이도 동작하도록 설계했으므로 Lambda
는 편의 기능이고 권한의 근거가 아니다(`infra/layers/30-identity/cognito.tf` 주석).

그러면 버전 검사가 자동으로 통과하고 T-33 의 절반이 무력해진다. **남는 방어선은
`revoked_after_ms` 다.** 그래서 운영 규칙이 하나 생긴다:

> 권한을 바꿀 때는 `claims_version` 과 함께 **`revoked_after_ms = now` 를 세운다.**
> 그것이 기존 액세스 토큰(최대 60분)을 즉시 무효화하는 유일한 수단이다.

```bash
# 강등 + 기존 토큰 폐기
NOW=$(python3 -c 'import time;print(int(time.time()*1000))')
aws dynamodb update-item --table-name dbmon-config-dev \
  --key "{\"PK\":{\"S\":\"USER#$SUB\"},\"SK\":{\"S\":\"PROFILE\"}}" \
  --update-expression "SET #r = :role, revoked_after_ms = :now ADD claims_version :one" \
  --expression-attribute-names '{"#r":"role"}' \
  --expression-attribute-values "{\":role\":{\"S\":\"viewer\"},\":now\":{\"N\":\"$NOW\"},\":one\":{\"N\":\"1\"}}"
```

## 4. RBAC

### 4.1 역할 정의

| 역할 | Cognito 그룹 | 권한 |
|---|---|---|
| `admin` | `dbmon-admin` | 전부 |
| `operator` | `dbmon-operator` | 조회 + 알림 규칙 + 리포트 생성 + 어드바이저 실행 + 수집 설정 |
| `viewer` | `dbmon-viewer` | 조회만 (환경 스코프 제한 가능) |

그룹이 없으면 권한 없음(fail-closed). 여러 그룹에 속하면 가장 높은 권한.

**이것이 코드의 동작이다** (`AuthContext::intersect`). 한동안 그렇지 않았다 — 빈 그룹을
"토큰이 좁히지 않는다" 로 읽어 서버 역할을 그대로 썼고, 그러면 사용자를 Cognito
그룹에서 빼도 여전히 admin 이었다(교차 리뷰 31라운드). Cognito 는 **네이티브 사용자의
그룹 멤버십을 `cognito:groups` 에 자동으로 넣으므로**(트리거가 필요 없다) 빈 목록은
"정보 없음" 이 아니라 "그룹이 없다" 다.

⚠ **IdP 페더레이션을 붙일 때** 는 Pre Token Generation 트리거로 그룹을 주입해야 한다
(§2.3). 그러지 않으면 IdP 사용자는 그룹 클레임이 비어 권한을 받지 못한다. 이 프로젝트는
아직 IdP 를 배선하지 않았다.

### 4.2 권한 매트릭스

| 동작 | admin | operator | viewer |
|---|---|---|---|
| 슬로우 쿼리·다이제스트 조회 | ✓ | ✓ | ✓ (스코프 내) |
| SQL 원문(리터럴) 조회 | ✓ | ✓ | 설정에 따름 |
| 실행계획 조회 | ✓ | ✓ | ✓ |
| CloudWatch 메트릭 조회 | ✓ | ✓ | ✓ |
| 리포트 조회 | ✓ | ✓ | ✓ |
| 리포트 생성 | ✓ | ✓ | ✗ |
| 어드바이저 실행 | ✓ | ✓ | ✗ |
| 알림 규칙 CRUD | ✓ | ✓ | ✗ |
| 알림 확인(ack)·음소거 | ✓ | ✓ | ✗ |
| 수집 설정 변경(주기·임계값·제외) | ✓ | ✓ | ✗ |
| 인스턴스 수집 활성/비활성 | ✓ | ✓ | ✗ |
| 환경 수동 지정 | ✓ | ✗ | ✗ |
| AWS 설정(리전·태그 매핑) | ✓ | ✗ | ✗ |
| 부트스트랩 실행 | ✓ | ✗ | ✗ |
| 채널 자격증명 등록 | ✓ | ✗ | ✗ |
| IdP 관리·사용자 관리 | ✓ | ✗ | ✗ |
| 리터럴 정책 변경 | ✓ | ✗ | ✗ |
| SQL 원문 조회 (`full_restricted` 인스턴스) | ✓ | ✓ | 개별 부여 시 |
| Athena `audit_search` 쿼리 | ✓ | ✗ | ✗ |
| Athena 그 외 쿼리 타입 | ✓ | ✓ | ✓ (스코프 내) |
| AI 예산 설정 | ✓ | ✗ | ✗ |
| 감사 로그 조회 | ✓ | ✗ | ✗ |

### 4.3 구현

```rust
// axum 미들웨어에서 AuthContext 를 확장으로 주입
struct AuthContext {
    sub: String,
    email: String,
    role: Role,                 // Admin | Operator | Viewer
    env_scope: Option<Vec<Env>>, // None = 전체
    can_see_literals: bool,
}

// 핸들러는 타입으로 요구한다
async fn bootstrap_apply(AdminOnly(ctx): AdminOnly, ...) { }
async fn list_slow_queries(Authed(ctx): Authed, Query(q): Query<...>) {
    let envs = ctx.scope_filter(q.env)?;   // 스코프 밖 요청은 여기서 403
}
```

**환경 스코프 강제 지점** — 조회 파라미터를 신뢰하지 않는다. `AuthContext`의 스코프와
요청 파라미터를 교집합해 실제 조회 조건을 만든다. 교집합이 비면 403이 아니라 **빈 결과**를
반환한다(존재 여부 노출 방지). 단 명시적으로 스코프 밖 환경을 지정한 요청은 403.

**리터럴 마스킹 강제 지점** — `can_see_literals == false`면 응답 직렬화 계층에서
`sql_text`를 `digest_text`로, `plan_json`을 `plan_normalized`로 치환한다.
핸들러가 아니라 직렬화에서 하는 이유: 새 엔드포인트를 추가할 때 빼먹지 않게 하기 위함.
DTO 타입에 `Redactable` trait를 구현하고 응답 래퍼가 자동 적용한다.

**직렬화 계층을 타지 않는 3개 경로에 같은 처리를 반복해야 한다** — 이게 실수의 원천이다.

| 경로 | 문제 | 대응 |
|---|---|---|
| WebSocket 브로드캐스트 | 수집기가 만든 메시지가 응답 래퍼를 거치지 않는다 (T-22) | **브로드캐스트는 항상 마스킹본만** 보낸다. 리터럴이 필요하면 클라이언트가 HTTP로 상세를 조회한다 |
| Athena 결과 | 행 배열이라 DTO가 아니다 (T-17) | Athena 결과 후처리 계층에서 컬럼 단위 마스킹 + env 스코프 필터를 강제 |
| 알림 채널 메시지 | 외부로 나간다 (T-23) | 알림 본문은 `digest_text`만 사용. 리터럴은 딥링크 뒤에 둔다 |

**권한은 필드 단위로도 강제한다 (T-10 확장)** — `PATCH /api/instances/{id}`는 operator가
호출할 수 있지만, 본문의 `literal_policy` 필드는 admin만 바꿀 수 있다.
엔드포인트 단위 권한만 검사하면 operator가 prd 인스턴스를 `full`로 바꿀 수 있다
(그리고 저장된 리터럴은 소급 삭제 외에 되돌릴 수 없다).

→ `literal_policy`는 **별도 엔드포인트**(`PUT /api/instances/{id}/literal-policy`, admin)로
분리한다. 권한 매트릭스 테스트에 필드 단위 케이스를 포함한다.

## 5. IAM — 앱의 Instance Profile

### 5.1 정책 분리

권한을 여러 정책으로 쪼개서, 필요 없는 것은 붙이지 않을 수 있게 한다.

| 정책 | 내용 | 기본 부여 |
|---|---|---|
| `dbmon-core` | RDS Describe, CloudWatch 조회, DynamoDB, S3, Athena, Glue, STS, SSM | ✓ |
| `dbmon-connect` | `rds-db:connect` | ✓ |
| **`dbmon-secrets-runtime`** | Secrets Manager 읽기 **`dbmon/channel/*`, `dbmon/target/*` 만** | ✓ (알림 발송·폴백 연결에 **상시 필요**) |
| `dbmon-bootstrap` | Secrets Manager 읽기 **`rds!*`** + Secrets 생성/로테이션 설정 | **✗** (부트스트랩 시에만) |
| `dbmon-rds-modify` | `rds:ModifyDBInstance`, `rds:ModifyDBCluster` | **✗** |
| `dbmon-ai` | Bedrock Converse (Guardrail 조건 필수) | AI 기능 사용 시 |
| `dbmon-auth-admin` | Cognito IdP·사용자 관리 | **✗ — 별도 Role로 분리** (아래) |

**`dbmon-secrets-runtime`을 `dbmon-bootstrap`에서 분리한 이유** — 초기 설계는 둘을 한 정책에
묶어 "부트스트랩 시에만 부여"라고 했다. 그런데 앱은 **알림 발송(`dbmon/channel/*`)과
비밀번호 폴백 연결(`dbmon/target/*`)을 위해 상시 시크릿 읽기가 필요하다.**
결과는 둘 중 하나였다: 알림이 조용히 깨지거나, 운영자가 **모든 RDS 마스터 시크릿 읽기
권한(`rds!*`)을 영구히 붙인다.** 후자가 현실적으로 일어난다.

**`dbmon-rds-modify`를 기본으로 붙이지 않는 이유** — 앱이 프로덕션 RDS 구성을 바꿀 수 있는
권한을 상시 갖고 있을 이유가 없다. IAM DB Auth 활성화가 필요하면 수동 경로
([07 §5](07-credentials-bootstrap.md))를 쓴다.

**`dbmon-auth-admin`을 별도 Role로 분리하는 이유 (T-27)** — 이 권한들
(`AdminCreateUser`, `AdminAddUserToGroup`, `AdminResetUserPassword`, `UpdateUserPoolClient`)이
웹 서버와 같은 Instance Profile에 있으면, **SSRF·RCE·의존성 침해 하나로 공격자가 사용자를
만들어 `dbmon-admin` 그룹에 넣고 정상 로그인 경로로 재진입**할 수 있다.
MFA가 `OPTIONAL`이면 더 쉽다. `UpdateUserPoolClient`로 콜백 URL을 추가해 토큰 탈취
리다이렉트를 심는 것도 가능하다.

```
dbmon-auth-admin-role
  신뢰 정책: 앱 Role만 AssumeRole. 조건에 sts:ExternalId + 세션 태그
  세션 만료: 15분
  사용: /api/auth/* 핸들러에서만 AssumeRole → 임시 자격증명으로 호출 → 즉시 폐기
  감사: AssumeRole 자체가 CloudTrail에 남는다 (Instance Profile 직접 호출은 구분이 안 된다)
```

추가로:
- **`UpdateUserPoolClient`는 Terraform 관리로 넘기는 것을 우선 검토한다.** IdP 추가 시
  `SupportedIdentityProviders`만 바꾸면 되는데, 이 API 하나 때문에 App Client 전체를
  변경할 수 있는 권한을 주게 된다. IdP 추가가 드물면 Terraform이 낫다
  → [OPEN-Q-19](OPEN-QUESTIONS.md).
- **`admin` 그룹에는 MFA를 `REQUIRED`로 강제한다.**

### 5.2 `dbmon-core` 정책 (요약)

> ⚠ **이 JSON 은 설계 시점의 것이고 정본이 아니다.**
>
> 정본은 [`infra/layers/40-compute/iam.tf`](../infra/layers/40-compute/iam.tf) 이고,
> 그쪽이 더 좁다. 이 절을 그대로 적용하면 코드가 쓰지 않는 권한이 붙는다
> (교차 리뷰 2회차가 지적).
>
> | 이 절 | 실제 | 왜 |
> |---|---|---|
> | `cloudwatch:ListMetrics` | **없다** | 지표 카탈로그가 코드에 있다 (`dbmon_core::cw_metrics`) |
> | `cloudwatch:GetMetricData` 조건 없음 | `cloudwatch:namespace = AWS/RDS` 조건 | 없으면 계정의 모든 지표(비용·보안 포함)를 읽는다 |
> | `logs:DescribeLogGroups`·`DescribeLogStreams`·`GetLogEvents` | **`FilterLogEvents` 만** | 나머지를 부르는 코드가 없다 |
> | 로그 자원 `/aws/rds/*` | `/aws/rds/instance/*/slowquery` | 앞쪽은 **audit 로그**(모든 문장)까지 포함한다 |
> | `S3ReadWrite`(raw·athena·plans) | **없다** | 콜드 티어는 철회, 플랜 오프로드는 미구현 |
> | `DynamoExport` | **없다** | 증분 내보내기 경로가 없다 |
> | — | `Deny dynamodb:Scan` | 실수로 전체 스캔하는 것을 권한으로 막는다 |
>
> 아래 JSON 은 "무엇을 하려 했는가" 의 기록으로 남긴다.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Sid": "RdsDiscovery", "Effect": "Allow",
      "Action": ["rds:DescribeDBInstances","rds:DescribeDBClusters",
                 "rds:DescribeDBParameters","rds:DescribeDBClusterParameters",
                 "rds:DescribeDBParameterGroups","rds:DescribeDBLogFiles",
                 "rds:DescribePendingMaintenanceActions","rds:DescribeDBEngineVersions",
                 "rds:ListTagsForResource"],
      "Resource": "*" },

    { "Sid": "Metrics", "Effect": "Allow",
      "Action": ["cloudwatch:GetMetricData","cloudwatch:ListMetrics"],
      "Resource": "*" },

    { "Sid": "SlowQueryLogs", "Effect": "Allow",
      "Action": ["logs:DescribeLogGroups","logs:DescribeLogStreams",
                 "logs:FilterLogEvents","logs:GetLogEvents"],
      "Resource": "arn:aws:logs:*:${ACCOUNT}:log-group:/aws/rds/*" },

    { "Sid": "OwnLogs", "Effect": "Allow",
      "Action": ["logs:CreateLogStream","logs:PutLogEvents","logs:DescribeLogStreams"],
      "Resource": "arn:aws:logs:*:${ACCOUNT}:log-group:/dbmon/*" },

    { "Sid": "Dynamo", "Effect": "Allow",
      "Action": ["dynamodb:GetItem","dynamodb:BatchGetItem","dynamodb:Query",
                 "dynamodb:PutItem","dynamodb:UpdateItem","dynamodb:BatchWriteItem",
                 "dynamodb:DeleteItem","dynamodb:DescribeTable"],
      "Resource": ["arn:aws:dynamodb:*:${ACCOUNT}:table/dbmon-*",
                   "arn:aws:dynamodb:*:${ACCOUNT}:table/dbmon-*/index/*"] },

    { "Sid": "DynamoExport", "Effect": "Allow",
      "Action": ["dynamodb:ExportTableToPointInTime","dynamodb:DescribeExport",
                 "dynamodb:ListExports","dynamodb:DescribeContinuousBackups"],
      "Resource": "arn:aws:dynamodb:*:${ACCOUNT}:table/dbmon-*" },

    { "Sid": "S3ReadWrite", "Effect": "Allow",
      "Action": ["s3:GetObject","s3:PutObject","s3:DeleteObject","s3:ListBucket",
                 "s3:GetBucketLocation","s3:AbortMultipartUpload"],
      "Resource": [
        "arn:aws:s3:::dbmon-raw-${ACCOUNT}",       "arn:aws:s3:::dbmon-raw-${ACCOUNT}/*",
        "arn:aws:s3:::dbmon-athena-results-${ACCOUNT}", "arn:aws:s3:::dbmon-athena-results-${ACCOUNT}/*",
        "arn:aws:s3:::dbmon-plans-${ACCOUNT}",     "arn:aws:s3:::dbmon-plans-${ACCOUNT}/*"],
      "Condition": { "StringEquals": { "aws:ResourceAccount": "${ACCOUNT}" } } },

    { "Sid": "S3ReportsNoDelete", "Effect": "Allow",
      "Action": ["s3:GetObject","s3:PutObject","s3:ListBucket","s3:GetBucketLocation"],
      "Resource": ["arn:aws:s3:::dbmon-reports-${ACCOUNT}",
                   "arn:aws:s3:::dbmon-reports-${ACCOUNT}/*"],
      "Condition": { "StringEquals": { "aws:ResourceAccount": "${ACCOUNT}" } } },

    { "Sid": "S3ArtifactsReadOnly", "Effect": "Allow",
      "Action": ["s3:GetObject","s3:ListBucket"],
      "Resource": ["arn:aws:s3:::dbmon-artifacts-${ACCOUNT}",
                   "arn:aws:s3:::dbmon-artifacts-${ACCOUNT}/*"],
      "Condition": { "StringEquals": { "aws:ResourceAccount": "${ACCOUNT}" } } },

    { "Sid": "AthenaQuery", "Effect": "Allow",
      "Action": ["athena:StartQueryExecution","athena:GetQueryExecution",
                 "athena:GetQueryResults","athena:StopQueryExecution",
                 "athena:GetWorkGroup","athena:GetDataCatalog"],
      "Resource": ["arn:aws:athena:*:${ACCOUNT}:workgroup/dbmon",
                   "arn:aws:athena:*:${ACCOUNT}:datacatalog/*"] },

    { "Sid": "GlueCatalogRead", "Effect": "Allow",
      "Action": ["glue:GetDatabase","glue:GetDatabases","glue:GetTable","glue:GetTables",
                 "glue:GetPartition","glue:GetPartitions"],
      "Resource": ["arn:aws:glue:*:${ACCOUNT}:catalog",
                   "arn:aws:glue:*:${ACCOUNT}:database/dbmon*",
                   "arn:aws:glue:*:${ACCOUNT}:table/dbmon*/*"] },

    { "Sid": "GlueCatalogWrite", "Effect": "Allow",
      "Action": ["glue:CreateTable","glue:UpdateTable","glue:BatchCreatePartition",
                 "glue:DeletePartition"],
      "Resource": ["arn:aws:glue:*:${ACCOUNT}:catalog",
                   "arn:aws:glue:*:${ACCOUNT}:database/dbmon*",
                   "arn:aws:glue:*:${ACCOUNT}:table/dbmon*/*"] },

    { "Sid": "S3Tables", "Effect": "Allow",
      "Action": ["s3tables:GetTable","s3tables:GetTableData","s3tables:PutTableData",
                 "s3tables:GetTableMetadataLocation","s3tables:UpdateTableMetadataLocation",
                 "s3tables:GetNamespace","s3tables:ListTables","s3tables:ListNamespaces"],
      "Resource": "arn:aws:s3tables:*:${ACCOUNT}:bucket/dbmon/*" },

    { "Sid": "Kms", "Effect": "Allow",
      "Action": ["kms:Decrypt","kms:GenerateDataKey","kms:DescribeKey"],
      "Resource": "arn:aws:kms:*:${ACCOUNT}:key/${CMK_ID}",
      "Condition": { "StringLike": { "kms:ViaService": [
          "dynamodb.*.amazonaws.com","s3.*.amazonaws.com",
          "secretsmanager.*.amazonaws.com","logs.*.amazonaws.com",
          "athena.*.amazonaws.com","glue.*.amazonaws.com",
          "s3tables.*.amazonaws.com"] } } },

    { "Sid": "SelfIdentity", "Effect": "Allow",
      "Action": ["sts:GetCallerIdentity","iam:ListAccountAliases","ec2:DescribeRegions"],
      "Resource": "*" },

    { "Sid": "IamSimulateSelfOnly", "Effect": "Allow",
      "Action": ["iam:SimulatePrincipalPolicy","iam:GetRole","iam:ListAttachedRolePolicies"],
      "Resource": "arn:aws:iam::${ACCOUNT}:role/dbmon-instance-role" },

    { "Sid": "AssumeAuthAdmin", "Effect": "Allow",
      "Action": "sts:AssumeRole",
      "Resource": "arn:aws:iam::${ACCOUNT}:role/dbmon-auth-admin-role" },

    { "Sid": "SelfHealth", "Effect": "Allow",
      "Action": ["autoscaling:SetInstanceHealth","autoscaling:CompleteLifecycleAction",
                 "autoscaling:RecordLifecycleActionHeartbeat",
                 "autoscaling:DescribeAutoScalingInstances"],
      "Resource": "*" },

    { "Sid": "DenyAuditMutation", "Effect": "Deny",
      "Action": ["dynamodb:DeleteItem","dynamodb:UpdateItem"],
      "Resource": ["arn:aws:dynamodb:*:${ACCOUNT}:table/dbmon-config"],
      "Condition": { "ForAllValues:StringLike": {
          "dynamodb:LeadingKeys": ["AUDIT#*"] } } },

    { "Sid": "DenyRbacMutation", "Effect": "Deny",
      "Action": ["dynamodb:PutItem","dynamodb:UpdateItem","dynamodb:DeleteItem",
                 "dynamodb:BatchWriteItem"],
      "Resource": ["arn:aws:dynamodb:*:${ACCOUNT}:table/dbmon-config"],
      "Condition": { "ForAllValues:StringEquals": {
          "dynamodb:LeadingKeys": ["USER","IDPMAP"] } } }
  ]
}
```

**정정한 것 4개**

| # | 문제 | 수정 |
|---|---|---|
| 1 | `kms:ViaService`에 와일드카드 값을 쓰면서 `StringEquals` 사용 → **절대 매치되지 않는다** | `StringLike`로 변경 + `athena`/`glue`/`s3tables` 추가 |
| 2 | `arn:aws:s3:::dbmon-*` — S3 버킷 이름은 **전역 네임스페이스**라 다른 계정의 `dbmon-xxx`에도 쓰기 허용 (T-25) | 버킷 ARN 열거 + `aws:ResourceAccount` 조건. 로그·리포트 버킷은 `DeleteObject` 제외 |
| 3 | `glue:*` + `Resource: "*"` → 계정 내 **모든 Glue 카탈로그** 변조 가능 (T-26). Athena를 워크그룹으로 좁혔는데 카탈로그가 열려 있어 무의미 | `catalog` / `database/dbmon*` / `table/dbmon*/*`로 축소 |
| 4 | 감사 로그 불변성이 **선언만** 있고 정책은 반대(전면 쓰기 허용) (T-21) | `AUDIT#*` 키 범위 `Deny`. `USER`/`IDPMAP` 쓰기도 Deny (T-20) |

**`USER`/`IDPMAP` 쓰기를 Deny하는 이유** — 앱 Role이 이 두 키에 쓸 수 있으면
**앱 침해 = 조용한 admin 승격**이며, IAM 변경이 아니라 데이터 쓰기라 CloudTrail에서 잘 보이지
않는다. 사용자·그룹 매핑 변경은 `dbmon-auth-admin-role`을 통해서만 한다.

**감사 로그의 위조 방지** — Deny만으로는 **삽입 위조**를 막지 못한다(앱이 감사 레코드를 직접
작성하므로). 두 가지를 추가한다.
1. **해시 체인** — 각 감사 레코드에 `prev_hash`를 담아 연쇄를 만든다. 중간 삭제·수정이
   체인 검증에서 드러난다.
2. **외부 앵커** — 일 1회 체인의 최신 해시를 별도 경로에 기록한다
   (CloudWatch Logs 전용 로그 그룹 또는 S3 Object Lock 버킷). 앱 Role이 앵커를 수정할 수
   없어야 한다. 아카이브 시 `audit_log` Iceberg 테이블은 **별도 쓰기 전용 프린시펄**을 쓰고
   S3 Object Lock(Compliance 모드)을 적용한다.

**S3 Tables 액션 이름·ARN 형식과 Glue 카탈로그 연동 방식은 실제 API로 검증이 필요하다**
→ [OPEN-Q-03](OPEN-QUESTIONS.md). 특히 `arn:aws:s3tables:...:bucket/dbmon/*`의 `dbmon`은
**네임스페이스가 아니라 테이블 버킷 이름**이어야 한다.

### 5.3 IAM 진단 화면 (FR-UI-09)

앱이 자신의 권한을 스스로 점검한다.

```
sts:GetCallerIdentity        → 현재 Role ARN 표시
iam:SimulatePrincipalPolicy  → 필요 액션 목록에 대해 허용/거부 판정
```

`iam:SimulatePrincipalPolicy` 권한이 없으면 **실제 호출을 시도해 판정**한다
(읽기 전용 API만, 예: `DescribeDBInstances`를 `MaxRecords=1`로 호출).
결과를 "필요 권한 / 현재 상태 / 부족한 것" 표로 보여준다.

권한 부족으로 기능이 안 되는 상황에서 사용자가 원인을 찾을 수 있어야 한다.
1세대에서 "왜 아무것도 안 보이나요"의 절반이 IAM 문제였다.

### 5.4 EC2 인스턴스 보안

- **IMDSv2 강제** (`HttpTokens=required`, `HttpPutResponseHopLimit=1`).
  T-12(SSRF로 자격증명 탈취) 방어의 핵심.
- 보안 그룹: 인바운드는 ALB 보안그룹에서 오는 앱 포트만. SSH 없음(SSM Session Manager).
- EBS 암호화, 종료 방지.
- 최소 AMI (Amazon Linux 2023), 자동 패치.

## 6. 데이터 보호

### 6.1 리터럴 정책 (NFR-S-06)

```
full            : sql_text 에 원문 저장. 조회는 can_see_literals 권한에 따름
full_restricted : sql_text 에 원문 저장. 조회는 operator 이상 + 조회 시 감사 로그 필수
masked          : sql_text 에 정규화 텍스트 저장 (리터럴 -> ?). 되돌릴 수 없음
off             : sql_text 미저장. digest_text 참조만
```

**기본값**: prd → `full_restricted`, stg/dev → `full`.

**왜 prd에 `masked`를 기본값으로 두지 않는가** — 처음 설계에서는 그렇게 했다가 되돌렸다.

- 저장 시점 마스킹은 **되돌릴 수 없다.** 리터럴을 지우면 다시 만들 수 없고, "실행 가능한
  실제 샘플 쿼리"(FR-DGS-06)가 영구히 불가능해진다. 그런데 튜닝이 필요한 쿼리는 대부분
  prd에 있다. 가장 중요한 환경에서 제품의 대표 기능이 꺼지는 셈이다.
- 노출 시점 통제는 **되돌릴 수 있고 동등하게 안전하다.** 응답 직렬화 계층에서 마스킹하므로
  (§4.3) 새 엔드포인트를 추가해도 빠뜨릴 수 없다. 권한을 조이거나 풀 수 있고, 누가 언제
  원문을 봤는지 감사 로그에 남는다.
- 즉 `masked`는 "저장을 막는" 정책이고 `full_restricted`는 "노출을 막는" 정책이다.
  같은 보호 수준을 얻으면서 되돌릴 수 있는 쪽을 택한다.

**단, 이건 조직 정책이 결정할 사안이다.** 저장 자체가 규정 위반인 환경(카드 정보, 의료 정보,
개인정보 처리 위탁 범위 밖)이면 `masked`/`off`가 맞다.
→ [OPEN-Q-15](OPEN-QUESTIONS.md)에서 사용자 확인이 필요하다.

`full_restricted`의 추가 통제:
- 조회 권한: `operator` 이상. `viewer`는 `can_see_literals`가 개별 부여된 경우만.
- 원문을 실제로 렌더한 요청마다 `data.literal_view` 감사 이벤트를 남긴다
  (`record_id`, `instance_id`, `actor`). 목록의 `sql_preview`는 정규화 텍스트를 쓰므로
  목록 조회만으로는 감사 로그가 폭증하지 않는다.
- 클립보드 복사·내보내기는 별도 감사 이벤트(`data.literal_copy`, `data.export`).

**공통 규칙**
- 정책 변경은 **앞으로 저장되는 데이터에만** 적용된다. `masked`로 강화할 때 "기존 데이터를
  삭제할까요?" 옵션을 제공한다(소급 마스킹은 불가능하므로 삭제가 유일한 수단).
- `full` / `full_restricted` 인스턴스는 목록·상세 화면에 배지를 표시해, 보는 사람이 운영
  데이터를 보고 있음을 인식하게 한다.
- `masked` / `off` 인스턴스의 다이제스트 상세에는 "이 인스턴스는 리터럴을 저장하지 않도록
  설정되어 있어 실행 가능한 샘플을 제공할 수 없습니다"를 명시하고 설정 화면으로 링크한다.


### 6.2 실행계획 리터럴 마스킹 (T-16)

**이것이 리터럴 정책의 가장 취약한 지점이었다.** `EXPLAIN FORMAT=JSON`의 다음 필드에는
쿼리의 실제 상수가 그대로 들어간다.

```json
{ "table": {
    "attached_condition": "(`orders`.`email` = 'kim@example.com')",
    "index_condition":    "(`orders`.`created_at` > '2026-07-01')",
    "used_key_parts":     ["status", "created_at"],
    "materialized_from_subquery": { "...": "중첩된 조건에도 리터럴" }
} }
```

정책이 `sql_text`에만 적용되면, `masked`/`off` 인스턴스에서도 리터럴이 다음 경로로 전파된다:

| 경로 | 보관 | 접근 |
|---|---|---|
| DynamoDB `plan_json` | 35일 | `GET /slow-queries/{id}/plan` (viewer) |
| `dbmon-plans-<acct>` (300KB 초과 오프로드) | 400일 | 앱 경유 |
| Iceberg `plan_zstd` | 1년 | Athena (T-17과 결합 시 직접) |
| 브라우저 캐시 | `immutable`, 1일 | 로컬 디스크 |
| **Bedrock** | — | `literals_included=false` 인데도 전송 |

**대응**

1. **플랜 정규화 단계에서 값 보유 필드를 마스킹한다.** `planparse`가 플랜을 파싱할 때
   조건식의 리터럴을 `normalize`와 같은 규칙으로 `?`로 치환한 `plan_normalized`를 만든다.
2. **저장 형태는 정책에 따라 갈린다.**

   | 정책 | 저장 |
   |---|---|
   | `full` | `plan_json`(원본) + `plan_normalized` |
   | `full_restricted` | `plan_json`(원본) + `plan_normalized`. 원본 조회는 operator 이상 + 감사 |
   | `masked` | **`plan_normalized`만.** 원본 폐기 |
   | `off` | `plan_normalized`만 |

3. **Bedrock에는 항상 `plan_normalized`를 보낸다.** `literals_included=true` 옵트인이어도
   플랜은 정규화본을 쓴다(리터럴이 필요하면 `literal_sample`로 명시적으로 보낸다).
   골든 페이로드 테스트에 플랜 필드를 포함한다.
4. **수집 제외 스키마·계정의 세션은 플랜 캡처 자체를 건너뛴다.**
   `EXPLAIN FOR CONNECTION`은 `PROCESS` 권한으로 **다른 계정 세션의 플랜**을 읽으므로,
   제외 규칙(FR-CAP-06)에 걸린 세션의 플랜을 수집하면 제외 규칙이 무의미해진다.
5. **DBA 수동 쿼리도 같은 문제다.** 제외 계정 목록에 DBA 계정을 넣을 수 있게 하고,
   기본 제외 목록에 `rdsadmin`, `system user`, `event_scheduler`, 모니터링 계정을 포함한다.

**검증** — `masked` 인스턴스의 저장된 레코드를 전수 검사해 `plan_normalized`에
문자열·숫자 리터럴 토큰이 남아 있지 않은지 확인하는 후조건 assert를 저장 직전에 둔다.
위반 시 플랜을 버리고 카운터를 올린다(T-35와 같은 원칙: 마스킹은 실패하면 저장하지 않는다).

### 6.3 암호화

| 대상 | 방식 |
|---|---|
| DynamoDB | KMS 고객 관리 키 (CMK) |
| S3 (raw, warehouse, 리포트) | SSE-KMS, 버킷 정책으로 비암호화 업로드 거부 |
| S3 Tables | 관리형 암호화 + CMK |
| Secrets Manager | CMK |
| CloudWatch Logs | CMK |
| 전송 중 | 전 구간 TLS. ALB는 TLS 1.2 이상, 최신 정책 |
| 대상 MySQL | TLS 필수 + CA 검증 |
| Athena 결과 | SSE-KMS |

CMK는 자동 회전 활성. 키 정책에서 앱 Role과 관리자만 허용.

### 6.4 S3 버킷 정책

```
- 퍼블릭 액세스 전면 차단 (계정 레벨 + 버킷 레벨)
- aws:SecureTransport = false 인 요청 거부
- s3:x-amz-server-side-encryption 미지정 PutObject 거부
- 버전 관리 활성 (리포트 버킷), raw 버킷은 비활성(Lifecycle 7일)
- 액세스 로깅 활성
```

## 7. 입력 검증 (NFR-S-08)

| 입력 | 검증 |
|---|---|
| `instance_id` | `^[a-z0-9-]+/[a-zA-Z0-9-]{1,63}$` + 인스턴스 레지스트리 존재 확인 |
| `app_digest` | `^[0-9a-f]{32}$` |
| `mysql_digest` | `^[0-9a-f]{64}$` |
| `env` | enum: `prd`/`stg`/`dev`/`unknown` |
| `from`/`to` | RFC3339. `from < to`. 범위 최대 400일 |
| 정렬 키 | 화이트리스트 enum |
| 페이지 크기 | 1~200, 기본 50 |
| 스키마명 | `^[A-Za-z0-9_$]{1,64}$` |
| **`monitor_user`** | `^[a-z][a-z0-9_]{0,31}$` + MySQL 예약어 거부 + 길이 32자 (T-18) |
| **`monitor_host`** | CIDR 또는 `%`. 기본값은 앱 서브넷 CIDR (T-04 완화) |
| 스레드 ID | `u64` 파싱 |
| 시크릿 ARN | ARN 정규식 + `DescribeSecret` 성공 + 태그 조건 |
| 웹훅 URL | https 필수, 도메인 허용목록(`hooks.slack.com`, `api.telegram.org`), 사설/링크로컬 IP 거부 |
| 알림 규칙 임계값 | 타입·범위 검증. 표현식은 파싱 후 AST 검증(임의 코드 실행 불가) |

**Athena SQL 구성** — 쿼리는 정적 템플릿 + 파라미터화한다.

```rust
// 값: Athena 파라미터화 쿼리 사용 (StartQueryExecution 의 ExecutionParameters)
//   SELECT ... WHERE env = ? AND started_at BETWEEN ? AND ?
// 식별자(정렬 컬럼, 파티션): 화이트리스트 매핑으로만
const SORT_COLUMNS: &[(&str, &str)] = &[
    ("duration", "duration_ms"),
    ("time",     "started_at"),
    ("rows",     "rows_examined"),
];
```

문자열 결합으로 SQL을 만드는 함수는 코드에 존재하지 않게 한다.
clippy lint + 리뷰 체크리스트로 강제한다.

### 7.1 부트스트랩 SQL 조립 (T-18)

**여기가 유일하게 문자열로 SQL을 만드는 곳이다.** 그리고 **마스터 권한으로 실행된다.**
`monitor_user`가 검증되지 않으면:

```
monitor_user = "dbmon'@'%' IDENTIFIED BY 'p'; GRANT ALL ON *.* TO 'evil'@'%'; -- "
→ CREATE USER 'dbmon'@'%' IDENTIFIED BY 'p'; GRANT ALL ON *.* TO 'evil'@'%'; -- '@'%' ...
```

앱 admin은 설계상 대상 DB를 장악할 수 없어야 하는데(T-06, §5.1의 전제) 이 한 필드가 그
경계를 넘긴다. 스키마명은 화이트리스트로 막았는데 사용자명만 빠져 있었다.

**3중 방어**
1. **화이트리스트 정규식** — `^[a-z][a-z0-9_]{0,31}$`. 인용부호·백틱·세미콜론·공백이
   구조적으로 들어올 수 없다.
2. **식별자 인용 유틸 강제** — 식별자는 백틱으로 감싸고 내부 백틱을 이스케이프하는
   `quote_ident()`를 통과해야 한다. 문자열 보간을 직접 쓰는 코드를 clippy로 금지.
3. **실행 전 다중 문장 검사** — 생성된 문장에 문장 구분자(`;`)가 문자열 리터럴 밖에
   존재하면 실행을 거부한다. `plan` 응답에 표시되는 문장에도 같은 검사를 적용한다.

**테스트** — 부트스트랩 SQL 생성 함수에 대해 인젝션 페이로드 테이블 테스트를 둔다
(§11의 "정렬 키" 테스트로는 잡히지 않는다).

## 8. 감사 로그 (FR-AUT-10)

기록 대상:

| 이벤트 | 남기는 것 |
|---|---|
| `auth.login` / `auth.logout` / `auth.denied` | sub, email, IP, UA, 결과 |
| `bootstrap.plan` / `bootstrap.apply` | [07 §4](07-credentials-bootstrap.md) 참조 |
| `config.update` | 경로, before, after |
| `rule.create/update/delete` | 규칙 내용 diff |
| `channel.create/update` | 채널 종류·이름 (자격증명 제외) |
| `idp.create/update/delete` | IdP 이름·타입 (client_secret 제외) |
| `user.invite/disable/group_change` | 대상 sub, 변경 내용 |
| `advisor.run` | app_digest, literals_included, 토큰 수 |
| `literal_policy.change` | 대상, before, after |
| `report.generate` | 대상 기간 |
| `data.export` | 사용자가 CSV/JSON을 내려받은 경우 대상·건수 |
| `data.literal_view` | `full_restricted` 인스턴스의 SQL 원문을 렌더한 요청 (record_id, actor) |
| `data.literal_copy` | 클립보드 복사 |
| `report.view` | 리포트 조회 (앱 프록시 경유. presigned 직접 노출을 하지 않는 이유) |
| `authadmin.assume` | `dbmon-auth-admin-role` AssumeRole (CloudTrail과 이중 기록) |
| `ai.model_change` | `ai.model_id` 설정 변경 |
| `network_access.state_change` | 대상 RDS 네트워크 접근 상태 전이 |
| `athena.query` | 실행한 쿼리 지문, 스캔 바이트 |

- 저장: `dbmon-config / AUDIT#<yyyy-mm>` → 월 1회 Iceberg 아카이브, 3년 보관.
- 감사 로그는 애플리케이션이 **수정·삭제할 수 없다**. IAM 정책에서 해당 키 범위의
  `DeleteItem`/`UpdateItem`을 조건으로 거부한다.
- `data.export`를 남기는 이유: 운영 SQL 리터럴이 조직 밖으로 나가는 유일한 합법 경로다.

## 9. Bedrock 데이터 경계 (T-05)

```
기본 (literals_included = false):
   전송: digest_text, 플랜 JSON, DDL(SHOW CREATE TABLE), 인덱스 카디널리티,
         테이블 행수·크기, 집계 지표, MySQL 버전·파라미터
   미전송: 리터럴이 살아있는 sql_text, QUERY_SAMPLE_TEXT, 실제 데이터 행

옵트인 (literals_included = true, 인스턴스별 admin 설정):
   추가 전송: 샘플 쿼리 1건의 원문
   감사 로그에 기록. UI에 경고 배지.
```

**DDL도 민감할 수 있다** — 컬럼명이 비즈니스 로직을 드러낸다(`customer_ssn`,
`internal_margin_rate`). 이것까지 막으면 어드바이저가 무의미해지므로 보내되,
설정에서 특정 스키마·테이블을 어드바이저 대상에서 제외할 수 있게 한다.

**Bedrock Guardrail — IAM으로 강제한다 (T-31).**
Guardrail을 **호출 파라미터로만** 적용하면 코드 버그·리팩터링·`ai.model_id` 설정 변경 한 번으로
PII 필터 없이 스키마·플랜이 모델로 나간다. IAM에서 강제할 수 있는데 안 하는 것은 낭비다.

```json
{ "Sid": "BedrockWithGuardrailOnly", "Effect": "Allow",
  "Action": ["bedrock:InvokeModel","bedrock:InvokeModelWithResponseStream","bedrock:Converse",
             "bedrock:ConverseStream"],
  "Resource": [
    "arn:aws:bedrock:*:${ACCOUNT}:inference-profile/${PROFILE_ID}",
    "arn:aws:bedrock:*::foundation-model/${MODEL_ID}"
  ],
  "Condition": { "StringEquals": {
      "bedrock:GuardrailIdentifier": "arn:aws:bedrock:${REGION}:${ACCOUNT}:guardrail/${GID}:${GVER}" } } },
{ "Sid": "ApplyGuardrail", "Effect": "Allow",
  "Action": "bedrock:ApplyGuardrail",
  "Resource": "arn:aws:bedrock:${REGION}:${ACCOUNT}:guardrail/${GID}" }
```

- **크로스 리전 추론 프로파일**을 쓰면 프로파일 ARN과 **각 대상 리전의 foundation-model ARN을
  모두** 허용해야 한다. 하나만 넣으면 런타임 `AccessDenied`가 나고, `*`로 열면 조건의 의미가
  약해진다. 대상 리전 목록을 Terraform 변수로 받아 열거한다.
- Guardrail은 각 대상 리전에 존재해야 한다.
- `ai.model_id` 설정 변경은 **admin 감사 이벤트**로 남기고, 정책 허용 목록과 교차 검증한다
  (허용되지 않은 모델 ID를 설정하면 저장 시 거부).

Guardrail 내용: PII 필터(이메일, 전화번호, 신용카드, 주민번호 패턴)를 입력·출력 양쪽에 적용.
차단되면 어드바이저 실행을 실패로 처리하고 무엇이 차단됐는지(패턴 종류만) 표시한다.

**모델 학습 사용** — Bedrock은 기본적으로 고객 입력을 모델 학습에 사용하지 않는다.
그래도 조직 정책 확인이 필요하면 AI 기능 전체를 설정으로 비활성화할 수 있게 한다
(`ai.enabled = false`. 이 경우 `dbmon-ai` IAM 정책을 붙이지 않는다).

## 10. 비밀 관리 요약

| 비밀 | 저장 위치 | 회전 | 앱 접근 |
|---|---|---|---|
| 대상 DB 마스터 자격증명 | Secrets Manager (RDS 관리형) 또는 저장 안 함 | RDS 자동 | 부트스트랩 시에만 |
| 모니터링 계정 | **없음** (IAM 토큰) | 해당 없음 | 매 연결 |
| 모니터링 계정 (폴백) | Secrets Manager `dbmon/target/<id>` | 30일 자동 | 연결 시 |
| Slack 웹훅/봇 토큰 | Secrets Manager `dbmon/channel/<id>` | 수동 | 발송 시 |
| Telegram 봇 토큰 | Secrets Manager `dbmon/channel/<id>` | 수동 | 발송 시 |
| Cognito IdP client_secret | Cognito 내부 (등록 후 앱이 보관 안 함) | 수동 | 없음 |
| KMS 키 | KMS | 자동 | 사용만 |

**하드코딩된 비밀은 없다.** CI에 secret scanning(gitleaks)을 게이트로 넣는다.

## 11. 보안 검증 항목

| 항목 | 방법 |
|---|---|
| 인증 없는 요청이 전부 401 | 전 엔드포인트 자동 테스트(라우터 목록 순회) |
| viewer 가 변경 API 호출 시 403 | 역할별 권한 매트릭스 테이블 주도 테스트 |
| 환경 스코프 밖 데이터 미노출 | 스코프 제한 사용자로 조회해 결과 검증 |
| 리터럴 마스킹 | `can_see_literals=false` 응답에 리터럴 부재 확인 |
| 로그에 비밀 미포함 | 부트스트랩 통합 테스트 실행 후 로그 전문을 패턴 검사 |
| `MasterSecret` 타입이 Debug/Serialize 미구현 | 컴파일 타임 (trait 미구현이므로 사용 시 컴파일 에러) |
| SQL 인젝션 | 정렬 키·식별자에 인젝션 페이로드 주입 테스트 |
| SSRF | 웹훅 URL에 `169.254.169.254`, `localhost`, 사설 IP 주입 테스트 |
| IMDSv2 | 인스턴스 메타데이터 옵션 검증 (Terraform + 런타임 확인) |
| 비밀 스캔 | CI에서 gitleaks |
| 의존성 취약점 | `cargo audit`, `cargo deny`, `npm audit` CI 게이트 |
| **플랜 리터럴 마스킹** | `masked` 인스턴스의 `plan_normalized`에 리터럴 토큰 0개 (T-16) |
| **Bedrock 페이로드에 플랜 리터럴 부재** | 골든 페이로드 스냅샷 + 정규식 검사 |
| **Athena 결과 마스킹·스코프** | `audit_search`를 viewer가 호출 시 403, 결과에 리터럴 부재 (T-17) |
| **부트스트랩 SQL 인젝션** | `monitor_user` 페이로드 테이블 테스트 + 다중 문장 검사 (T-18) |
| **폴백 비밀번호 미노출** | apply 후 plan 응답·감사 레코드·WS 메시지 전수 검사 (T-19) |
| **토큰 클레임 승격 차단** | 위조 `cognito:groups`로 admin 주장 → USER 레코드 교집합으로 차단 (T-20) |
| **감사 로그 변조 차단** | 앱 자격증명으로 `AUDIT#` 항목 삭제·수정 시도 → Deny (T-21) |
| **감사 해시 체인** | 중간 레코드 제거 후 체인 검증 실패 확인 |
| **WS 마스킹·토픽 인가** | 토픽 × 역할 × env 스코프 매트릭스 테스트 (T-22) |
| **알림 본문에 리터럴 부재** | 발송 페이로드 검사 (T-23) |
| **리포트 XSS** | `change_note`·LLM 출력에 `<script>` 주입 → 이스케이프 확인 (T-24) |
| **S3 타 계정 버킷 접근 차단** | 다른 계정의 `dbmon-x` 버킷에 PutObject 시도 → Deny (T-25) |
| **Glue 타 DB 접근 차단** | `dbmon` 외 데이터베이스 조회 시도 → Deny (T-26) |
| **auth-admin 분리** | 앱 Role로 `AdminCreateUser` 직접 호출 → Deny (T-27) |
| **부트스트랩 TOCTOU** | plan 후 계정 선점 → apply 거부 (T-28) |
| **env 태그 신뢰 경계** | prd→dev 태그 변경 시 리터럴 정책이 자동 완화되지 않음 (T-29) |
| **코어 덤프·WAF 로그에 비밀 부재** | `LimitCORE=0` 확인 + WAF 본문 로깅 제외 확인 (T-30) |
| **Guardrail 없는 Bedrock 호출 차단** | 조건 없이 Converse 호출 → AccessDenied (T-31) |
| **커서 위조 차단** | 다른 사용자의 커서·조작된 커서 → 400 (T-32) |
| **토큰 폐기 즉시 반영** | 그룹 강등 후 기존 토큰으로 호출 → 401 (T-33) |
| **프롬프트 인젝션** | 테이블 COMMENT에 지시문 삽입 → 중화 확인 (T-34) |
| **Athena 예산 소진 방어** | 사용자 1명이 한도 초과 시도 → 503, 리포트 워크그룹은 영향 없음 (T-35) |
| **마스킹 후조건** | `masked`/`off` 정책에서 리터럴 잔존 시 저장 거부 + 카운터 (T-36) |
