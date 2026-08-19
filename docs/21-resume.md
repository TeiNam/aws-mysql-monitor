# 21. 이어서 하기 (2026-08-20 중단 지점)

작업 중 이동으로 중단했다. **이 문서만 읽으면 이어서 할 수 있게** 검증된 사실과
미확인 항목을 구분해 적는다.

브랜치: `feat/m0-foundation` · 마지막 커밋 `fa80adb` · **변경 19개 파일 커밋 안 됨**

---

## 1. 이번 구간에 무엇을 했는가

사용자 요청 두 개가 순서대로 들어왔다.

1. "모니터링 도커를 띄워서 **웹유아이로 접속** 가능한가" → 조회 API + 임베드 화면
2. "프론트는 실시간 메트릭 데이터 출력을 하니까 **React** 로 만드는 게 좋지 않아?"
   → 맞다. 그런데 그러려면 **백엔드에 실시간 경로가 먼저 있어야** 했고, 없었다.
   그래서 WebSocket + 실시간 지표 수집을 먼저 만들었다.

### 새로 만든 것

| 파일 | 내용 |
|---|---|
| `crates/dbmon/src/api/{mod,auth,view,cursor}.rs` | 조회 API, T-01 인증, 리터럴 통제, 서명 커서 |
| `crates/dbmon/src/api/topic.rs` | WS 구독 토픽 파싱·인가 (순수 함수) |
| `crates/dbmon/src/api/hub.rs` | 방송 허브. `slowq`/`status` 2채널 |
| `crates/dbmon/src/api/ws.rs` | WebSocket 엔드포인트 `/api/ws` |
| `crates/dbmon/src/metrics/derive.rs` | `global_status` → QPS 등 유도 (순수 함수) |
| `crates/dbmon/src/metrics/sampler.rs` | 5초 주기 샘플러 |
| `crates/dbmon/src/store/broadcast.rs` | 저장소 데코레이터 — **저장되면 반드시 방송된다** |
| `crates/dbmon/assets/index.html` | 임베드 최소 화면 (SPA 아님) |
| `web/package.json` | React 19 + Vite 6 + TS + Tailwind 4 스캐폴딩. **의존성 설치까지만 됨** |

전체 테스트 **657개 통과**, `cargo clippy --workspace --all-targets` 경고 0.

---

## 2. 실행으로 검증한 것 (재현 명령 포함)

### 2.1 호스트에서 화면 접속 — 확인됨

```bash
DBMON_TARGET_PASSWORD=dbmon-local-monitor \
  cargo run -p dbmon -- --config local/dbmon.toml --log-pretty serve
# → http://127.0.0.1:8080/
```

브라우저(Playwright)로 12행 렌더 확인. SQL 은 `masked` 정책대로 `?` 로 표시된다.

### 2.2 컨테이너에서 화면 접속 — 확인됨

```bash
docker compose --profile monitor up -d --build dbmon
docker compose logs dbmon | grep token=      # 접속 URL 이 나온다
# → http://127.0.0.1:18080/?token=<발급토큰>
```

브라우저로 25행 렌더 확인. 토큰이 URL 에서 즉시 제거되고 `sessionStorage` 로 옮겨진다.

### 2.3 T-01 인증 게이트 — 음성 대조군까지 확인됨

| 상황 | `/api/slow-queries` | `/healthz` | `/` |
|---|---|---|---|
| dev + 루프백 | 200 | 200 | 200 |
| dev + `0.0.0.0` (토큰 없음) | **401** | 200 | 200(토큰 필요) |
| dev + `0.0.0.0` (올바른 토큰) | 200 | 200 | 200 |
| dev + `0.0.0.0` (틀린 토큰) | **401** | 200 | — |
| prd | **401** | 200 | **404** |

### 2.4 WebSocket 프로토콜 — 확인됨

```
정상 흐름          ready → subscribed → pong
첫 메시지가 subscribe  error/unauthorized → 연결 종료
와일드카드          허용=["slowq:env=dev"] 거부=["slowq:*"]
auth 미전송         auth_timeout → 종료 (정확히 5.0초)
```

### 2.5 실시간 지표 방송 — 확인됨

```
status qps=8.98 thr_run=3 lock=0 gap=-
```

`global_status()` 는 이번에 처음으로 **호출부가 생겼다**(이전엔 포트·어댑터만 있고
부르는 곳이 없었다 — 이 프로젝트에서 열 번째 "구현은 있고 호출부가 없다").

---

## 3. ⚠ 다음에 가장 먼저 할 일

### 3.1 `slowq` 방송이 오는지 확인되지 않았다 (최우선)

지표(`status`)는 왔지만 **슬로우 쿼리(`slowq`) 방송은 한 번도 관측하지 못했다.**
저장소 데코레이터 단위 테스트는 통과하는데(`a_stored_record_is_broadcast`),
실제 프로세스에서 확인이 안 됐다.

재현 절차:

```bash
# 1. 격리 테이블로 리더를 잡는다 (아래 3.3 참고 — dbmon-data-local 은 다른
#    컨테이너가 리스를 쥐고 있다)
DBMON_TARGET_PASSWORD=dbmon-local-monitor \
DBMON__STORAGE__DATA_TABLE=dbmon-data-wstest \
AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local \
  ./target/release/dbmon --config local/dbmon.toml --log-pretty serve

# 2. WS 를 붙이고 (스크립트는 /tmp/ws-live.mjs 에 있었다 — 아래 부록 A 에 전문)
node ws-live.mjs

# 3. 느린 쿼리를 하나 만든다
docker exec dbmon-dev-mysql84-1 mysql -uloadgen -pdbmon-local-loadgen -D shop \
  -e "SELECT /* ws-demo */ SLEEP(6);"
```

확인해야 할 것 — **순서대로 좁힐 것**:

1. `dbmon-data-wstest` 에 새 `SQ#` 레코드가 들어오는가?
   (중단 시점에는 레코드가 1개뿐이고 그건 23:00 에 시작된 **오래된 in_flight**
   레코드였다 — 즉 SLEEP(6) 자체가 캡처되지 않았을 가능성이 높다)
2. 캡처가 됐는데 방송이 안 됐는가? → `Hub::publish_slow_query` 의 키와
   구독 키가 어긋나는지 본다 (`slowq:env=dev`)
3. `BroadcastingStore` 가 실제로 배선됐는가? → `build_stores` 가 감싸고 있고
   `AppSlowQueryStore` 별칭을 쓰므로 구조적으로는 보장되지만, **실행으로 확인한
   적이 없다**

> ⚠ **함정**: `SELECT ... FROM orders o JOIN order_items i ... WHERE SLEEP(4)=0`
> 형태로 테스트하지 말 것. `SLEEP` 이 **조인 행마다** 평가돼 12만 행 × 4초 =
> 며칠이 걸린다. 이 세션에서 두 번 걸렸다. `SELECT SLEEP(6);` 처럼 스칼라로 쓴다.

### 3.2 React SPA — 스캐폴딩만 됐다

`web/` 에 `package.json` 과 `node_modules` 만 있다. 소스 파일이 하나도 없다.

만들 것 (docs/09 규정 기준, 백엔드가 실제로 주는 것만):

| 라우트 | 쓸 수 있는 API |
|---|---|
| `/` 플릿 개요 | `GET /api/instances` + WS `status:inst=…` |
| `/slow-queries` | `GET /api/slow-queries` |
| `/slow-queries/:id` | `GET /api/queries/{id}` |
| `/live` | WS `slowq:env=…` |

**백엔드에 없는 것은 화면도 만들지 않는다** — 다이제스트, 알림, 리포트,
부트스트랩, CloudWatch 메트릭, Athena 콜드 티어. 빈 화면을 만들면 "데이터가 없다"
로 오해된다(같은 이유로 API 도 라우트를 만들지 않았다).

Dockerfile 에 **노드 빌더 스테이지 추가**가 필요하다. 런타임 이미지에 노드를
넣지 않는다는 기존 결정을 지키려면 `dist/` 만 `COPY` 해서 Rust 가 정적 서빙한다.

### 3.3 로컬 환경에 남아 있는 것

| 대상 | 상태 | 조치 |
|---|---|---|
| `dbmon-web` 컨테이너 (`dbmon:round8`) | **실행 중**, `dbmon-data-local` 의 수집 리더 리스를 쥐고 있다 | 이 세션에서 만든 게 아니라 손대지 않았다. 정리 여부는 사용자 판단 |
| `dbmon-dev-dbmon-1` (컴포즈 `monitor` 프로파일) | 실행 중, `role=api`, HEALTHCHECK `unhealthy` | **원인 미확인** — 컨테이너 안 `dbmon healthcheck` 가 왜 실패하는지 봐야 한다 |
| `dbmon-data-wstest` 테이블 | 이 세션에서 만든 격리 테이블 | 필요 없으면 삭제 |
| `dbmon-dev-mysql84-1` 의 장기 실행 쿼리 | 23:00 시작된 조인+SLEEP 이 아직 돌 수 있다 | `SHOW PROCESSLIST` 로 확인 후 `KILL` |

`just` 가 이 머신에 **설치돼 있지 않다.** 그래서 justfile 타깃(`docker-up`,
`docker-down` 포함)은 **실행으로 검증되지 않았다.** 컴포즈 명령은 직접 실행해 확인했다.

---

## 4. 이번에 고친 결함 (재발 방지용 기록)

`docs/20-review-log.md` 에 옮겨 넣을 것.

### 4.1 커서 구분자 충돌 (테스트가 잡음)

`Cursor` 페이로드를 `sub|filters|position|exp` 로 만들고 주석에 "`|` 는 나타날 수
없다" 고 적었다. **DynamoDB 키 직렬화가 `PK|SK` 라서 항상 나타났다.** 정당한 커서
전부가 `Malformed` 로 거부 → 페이지네이션이 첫 페이지에서 멈춘다.
→ `position` 을 맨 뒤로 옮기고 `splitn(4, '|')`. 회귀 테스트 추가.

### 4.2 `mysql_common` 패닉 — IAM 토큰 + 평문 접속 (실행이 잡음)

`caching_sha2_password` 전체 인증은 비밀을 서버 공개키로 **RSA 암호화**한다.
IAM DB Auth 토큰은 ~1000바이트라 RSA-2048 블록에 안 들어가고
`mysql_common/crypto/rsa.rs` 가 `assert!` 로 **패닉**한다("message too long").
tokio 워커 스레드가 죽는다.

발생 조건: `deployment_env=dev` 인데 `DBMON_TARGET_PASSWORD` 가 없으면 IAM
폴백을 타고, 로컬 MySQL 에 붙는 순간 패닉.
→ `plaintext_secret_is_sendable(len)` 가드로 **패닉을 오류로 바꿨다**. 오류
메시지가 `DBMON_TARGET_PASSWORD` 를 알려 준다.

### 4.3 `DBMON_TARGET_PASSWORD` 가 어디에도 배선돼 있지 않았다

dev 폴백 코드는 있는데 justfile·local/dbmon.toml·docker-compose 어디에도
없었다. 그래서 문서대로 따라 하면 4.2 의 패닉을 만난다.
→ 세 곳에 모두 넣고 이유를 주석으로 남겼다.

### 4.4 `0` 을 센티널로 쓴 것 (테스트가 잡음)

`MetricsSampler.last_sampled_at_ms: EpochMs = 0` 이 "아직 샘플링 안 함" 을
의미하게 했는데 `0` 은 유효한 `EpochMs` 다. `now_ms = 0` 인 테스트가 잡았다.
→ `Option<EpochMs>`.

### 4.5 임베드 화면이 prd 에서도 서빙됐다

정적 HTML 이라 데이터는 안 새지만 T-01 의 인증 예외가 하나 늘고, prd 에서는
인증을 통과할 수 없으니 **깨진 화면**이다.
→ `policy.serves_local_ui()` 일 때만 라우트를 붙인다. prd 는 404.

---

## 5. 이번에 내린 설계 결정

### 5.1 컨테이너 접속: 우회를 넓히지 않고 토큰을 발급한다

인증 우회는 `deployment_env == dev` **AND** 루프백 바인드다. 그런데 컨테이너는
포트 퍼블리시를 위해 `0.0.0.0` 에 바인드해야 하므로 **우회가 꺼지고 화면을 쓸 수
없다.** 컨테이너 안에서는 호스트가 포트를 루프백에만 공개했는지 알 수 없다 —
네임스페이스 밖의 사실이다.

우회 조건을 넓히는 대신 **실제 자격증명을 하나 발급**한다:

```
토큰 발급 = (deployment_env == dev) AND (bind 이 루프백 아님) AND (ECS 아님)
```

우회가 아니라 인증이므로 바인드 주소가 무엇이든 구멍이 없다. ECS 를 배제하는
이유: dev ECS 배포가 공개 ALB 뒤에 있으면 토큰이 CloudWatch Logs 에 남는다.
ECS 는 태스크마다 `ECS_CONTAINER_METADATA_URI_V4` 를 주입하므로 **그 부재**로
판정한다 — 위험한 방향(프로덕션에서 켜짐)을 막으려면 부재가 조건이어야 한다.

### 5.2 `endpoint_url` 루프백 규칙을 컨테이너까지 넓혔다

컨테이너 안의 `127.0.0.1` 은 컨테이너 자신이라 호스트 DynamoDB Local 에 닿지
못한다. 컴포즈 서비스 이름(`http://dynamodb:8000`)이 필요한데 그건 루프백이 아니다.

→ `endpoint_override_allowed(env, url, in_local_container)` **순수 함수**로 빼고
`deployment_env == dev` 는 그대로 필수로 뒀다. prd·stg·unknown 은 URL 이
무엇이든, 컨테이너 안이든 밖이든 거부된다(전수 테스트 있음).

### 5.3 방송은 저장소를 감싸서 한다

수집기는 세 곳에서 저장한다(선행 저장·확정·고아 정리). 호출부마다 `publish` 를
넣으면 빠뜨릴 기회가 셋 생긴다 — 이 프로젝트에서 아홉 번 재발한 실패 유형이다.
→ `BroadcastingStore` 데코레이터 + `AppSlowQueryStore` 별칭. **감싸는 것을 잊을
수 없다.**

### 5.4 방송 페이로드는 `AuthContext` 를 받지 않는다 (T-22)

방송은 여러 사용자에게 같은 바이트를 보내므로 사용자별 권한으로 가릴 수 없다.
`SlowQueryBroadcast::from_record(q)` 는 **문맥을 인자로 받지 않는다** — 그래서
실수로 관대하게 만들 수 없다. `sql_preview` 는 정책이 `masked` 일 때만 담고,
원문 정책이면 비운다(클라이언트가 HTTP 상세로 조회 → 그 경로에 권한·감사가 있다).

### 5.5 백프레셔: 채널을 둘로 나눴다

규정이 "`status` 는 버리고 `slowq`·`alert` 는 버리지 않는다" 인데 한 채널로는
구분이 불가능하다. `slowq`(1000)는 밀리면 `stream_lagged` 로 **알려서 재조회**를
유발하고, `status`(64)는 조용히 버린다 — 게이지는 최신값만 의미가 있고 5초 뒤
갱신된다.

---

## 6. 미해결 (이전부터)

- **OPEN-Q-15 컴플라이언스 절반**: 운영 데이터 리터럴 저장 허용 여부는 사용자
  판단 대기. 엔지니어링 절반은 결정·문서화됨(기본 `masked`, `collector.literal_policy`
  한 줄로 전환, prd+masked 는 기동 경고).
- M5 Cognito JWT 검증(JWKS·kid 캐시·`AuthContext::intersect`) — 지금은 **fail closed**
- 콜드 티어(Athena) 페이지네이션, `next_cursor` 발급
- `POST /api/queries`(샘플 실행), M3 부트스트랩, M7 아카이브
- `docs/20-review-log.md` 의 이연 항목: 쓰기 증폭(M4), 다이제스트 스냅샷 배선
  (M12/M11/M13-15), M24-26, `is_collectible()` 수집 시점 재검사(R6-24),
  STS 계정 검증(R6-25)
- **9차 2way 리뷰 미실시** — 이번 구간(API·WS·지표·방송)이 리뷰를 안 받았다.
  Stop 훅의 종료 조건이 이것이다.

---

## 부록 A. WS 확인 스크립트

`web/` 밖에 두면 잃어버리므로 여기 남긴다. `node ws-live.mjs` 로 돌린다.

```js
const ws = new WebSocket("ws://127.0.0.1:8080/api/ws");
const got = [];
ws.onopen = () => {
  ws.send(JSON.stringify({ t: "auth" }));   // 로컬 우회는 토큰이 없다
  ws.send(JSON.stringify({ t: "subscribe", topics: [
    "slowq:env=dev",
    "status:inst=000000000000/ap-northeast-2/mysql84-local",
  ]}));
};
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.t === "slowq")  got.push(`slowq ${m.data.state} ${m.data.duration_ms}ms ${m.data.sql_preview ?? "(없음)"}`);
  if (m.t === "status") got.push(`status qps=${m.metrics.qps ?? "—"} thr_run=${m.metrics.threads_running}`);
  if (m.t === "subscribed") got.push(`subscribed=${JSON.stringify(m.topics)} denied=${JSON.stringify(m.denied)}`);
};
setTimeout(() => { console.log(got.join("\n") || "(아무것도 오지 않았다)"); process.exit(0); }, 20000);
```

## 부록 B. 커밋 전에 돌릴 것

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-docs.py          # 이 문서의 링크도 검사된다
```
