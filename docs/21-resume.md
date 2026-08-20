# 21. 이어서 하기 (2026-08-21 중단 지점)

**이 문서만 읽으면 이어서 할 수 있게** 검증된 사실과 미확인 항목을 구분해 적는다.

브랜치: `feat/m0-foundation` · 코드 전부 커밋됨 (원격 없음 — §3.1)

---

## 0. ⚠ 이 프로젝트가 무엇을 옮기는 것인지 (먼저 읽는다)

부모 폴더에 **이미 동작하는 구현 두 개**가 있다. 이 프로젝트는 그것을 Rust + React
로 다시 만드는 것이다.

| 경로 | 무엇 |
|---|---|
| `../my_slow_query_scraper` | FastAPI + MongoDB 백엔드 (실시간 캡처·CloudWatch 수집·EXPLAIN·통계) |
| `../my_slow_query_dashboard` | React 대시보드 5화면 + `screenshots/` |

**화면을 만들거나 API 를 정할 때 이 두 레포를 먼저 본다.** `docs/09-frontend.md`
는 이상적인 설계(24 라우트)를 적어 뒀지만, **사용자가 기대하는 것은 참조 구현의
모습**이다. 한 세션이 이걸 모르고 만들어서 전혀 다른 화면(다크 4화면)을 냈고,
전부 다시 만들었다. 두 번 하지 않는다.

참조 API 표면과 이쪽 대응은 [20 §참조 이식](20-review-log.md) 표에 있다.

---

## 1. 이번 구간에 무엇을 했는가

세 구간이 이어졌다.

1. **조회 API + WebSocket + 실시간 지표** (M6 일부) — 커밋 `7454766`
2. **React SPA + 정적 서빙 + 9차 2way 리뷰** — 다크 4화면. **참조 구현을 보지 않아
   전부 다시 만들었다**(§0)
3. **참조 대시보드 이식** — 조회 경로 8개 추가 + 5화면 재구성 + 수집 제어
4. **2way 리뷰 2~10라운드** — 읽기 경로를 하나로 합치고(`collect_views`), 일시정지가
   **태스크를 정말 멈추게** 고쳤고, **읽은 구간을 화면에 정직하게 말하게** 했고,
   **유령 레코드의 뿌리**(병합 후 키 표류)와 **항진명제였던 낙관적 잠금**을 고쳤고,
   실행 동일성 판정을 도메인으로 올려 저장소·페이크·조회가 같은 규칙을 쓰게 했다
   ([20 §2way 리뷰 2~10라운드](20-review-log.md))

**리뷰가 저장 경로의 결함 세 겹을 벗겨냈다.** 화면을 옮기려고 시작한 라운드가
저장소의 유령 레코드·키 표류·무력한 조건부 쓰기까지 내려갔다 — 프론트가 없었으면
"진행 중이 영원히 진행 중" 이라는 화면을 아무도 보지 않았을 것이다.

`docs/09-frontend.md` 가 규정한 SPA 가 실제로 돌아간다. 그리고 그 프론트가
**백엔드 결함 하나를 드러냈다**(WS 유휴 종료 — [20 §R9-1](20-review-log.md)).

### 새로 만든 것

**백엔드 (참조 API 표면에 대응)**

| 파일 | 내용 |
|---|---|
| `api/aggregate.rs` | 다이제스트·월간 통계·사용자 통계를 **조회 시점에** 접는다(순수 함수) |
| `api/mod.rs` | `/api/{aws/info,digests,statistics,statistics/users,plans,queries/{id}/plan,queries/{id}/markdown}` + 제어 5개 |
| `control.rs` | 수집 정지·재개·즉시 탐색·즉시 백필. API 와 리더 루프가 원자값으로 공유 |
| `main.rs` | 리더 루프에 제어 배선, `web/dist` 정적 서빙(캐시 정책), 없는 `/api/…` 는 404 |
| `Dockerfile` | 노드 빌더 스테이지 — **런타임 이미지에 노드는 없다** |

**프론트 (참조 대시보드 5화면)**

| 파일 | 내용 |
|---|---|
| `pages/{MySQLMonitor,PlanVisualization,CloudWatch,Statistics,RDSInstance}Page.tsx` | 5화면. 경로까지 참조와 같다 |
| `lib/plan-graph.ts` | EXPLAIN JSON → 그래프 (순수 함수). 참조의 canvas 판을 대체 |
| `components/PlanGraph.tsx` | SVG 렌더 — 확대·검색·스크린리더가 된다 |
| `components/CollectorControls.tsx` | 수집 정지/재개·인스턴스 수집·지금 수집 |
| `lib/live{,-reduce}.ts` | WS 배관과 판정부(순수). 실시간 지표·방송 |
| `components/{Shell,Card,Pagination,SqlModal,ui}.tsx` | 껍데기·카드·페이지네이션·SQL 팝업·클래스 상수 |

테스트: **Rust 695개 + web 71개**, `cargo clippy --workspace --all-targets` 경고 0.

### 의존성

`lucide-react`(참조와 같은 아이콘), `sql-formatter`(SQL 정형화), `@tanstack/react-query`,
`react-router`. 차트 라이브러리는 쓰지 않는다 — 스파크라인은 SVG 12줄, 실행계획은
직접 그린다. `vitest` 는 vite 6 과 타입이 충돌해 3.x 로 올렸다.

---

## 2. 실행으로 검증한 것 (재현 명령 포함)

### 2.1 개발 흐름 — 확인됨

```bash
# 백엔드 (호스트, 루프백 → 토큰 없이 접속)
DBMON_TARGET_PASSWORD=dbmon-local-monitor \
  cargo run -p dbmon -- --config local/dbmon.toml --log-pretty serve

# 프론트 개발 서버 (5173 → /api 와 /api/ws 를 8080 으로 프록시)
npm --prefix web run dev
```

프록시를 쓰는 이유는 **CORS 설정을 만들지 않기 위해서**다. 백엔드에 CORS 를 열면
그 설정이 프로덕션까지 따라간다.

### 2.2 Rust 가 SPA 를 서빙한다 — 확인됨

```bash
npm --prefix web run build          # → web/dist
DBMON_TARGET_PASSWORD=dbmon-local-monitor \
  cargo run -p dbmon -- --config local/dbmon.toml --log-pretty serve
# 로그: serve_ui=true spa=web/dist
```

| 경로 | 응답 |
|---|---|
| `/` · `/slow-queries` · `/live` (딥링크) | 200 `text/html` (셸, `cache-control: no-cache`) |
| `/assets/index-*.js` | 200, `public, max-age=31536000, immutable` |
| `/assets/없는파일.js` | 404, **캐시 헤더 없음** |
| `/api` · `/api/없는경로` | 404 `{"error":"not_found"}` |

산출물 위치는 `DBMON_UI_DIR` (기본 `web/dist`). **없으면 기존 임베드 화면으로
떨어진다** — `npm run build` 를 안 돌린 체크아웃도 화면을 잃지 않는다.

### 2.3 컨테이너 — 확인됨

```bash
docker compose --profile monitor build dbmon   # 노드 스테이지 + Rust 스테이지
docker compose --profile monitor up -d dbmon
docker compose logs dbmon | grep token=
# → http://127.0.0.1:8080/?token=…   ⚠ 포트는 컨테이너 안 것이다
# → 호스트에서는 http://127.0.0.1:18080/?token=…
```

로그: `serve_ui=true spa=/app/web/dist`. 브라우저로 접속해 토큰이 URL 에서 즉시
제거되고 `sessionStorage` 로 옮겨지는 것, HTTP·WS 양쪽이 그 토큰으로 인증되는 것을
확인했다. 컨테이너 상태는 **`healthy`** 다.

이 컨테이너는 `DBMON__ROLE=api` 라 지표를 수집하지 않는다. 그래서 플릿 타일이
`0/1 인스턴스 기준` 이라고 적는다 — **정상이다**(합계가 몇 개 기준인지 말하는
장치가 동작하는 것이다). 실시간 지표를 컨테이너에서 보려면 `DBMON__ROLE=all`.

### 2.4 5화면 전부 브라우저에서 확인됨

| 화면 | 확인한 것 |
|---|---|
| `/mysql` | 슬로우 쿼리 83건 표(20건씩 5페이지), QPS 13.6·실행 3·접속 10, 스파크라인, 수집 정지/재개 |
| `/plan` | 플랜 11건 목록 → 선택 → 쿼리 정보 + 실행계획 그래프(Select → No tables used) |
| `/cloudwatch` | 다이제스트 9행 / 84건 실행, 월 선택(KST 경계), 지금 수집 |
| `/statistics` | 인스턴스별 1행 + 사용자별 3행(loadgen/dbmon/root), 유형 분포 칩 |
| `/rds` | 태그·엔드포인트·수집 여부·마지막 관측 |

수집 제어도 화면에서 확인했다: **정지** → 로그에 `일시정지됐다` + 감사 로그
(subject 포함) → 표시가 `일시정지` 로 바뀜 → **재개** → `collecting=1` 복귀.

3라운드에서 **정지가 진짜로 멈추는지**를 부하로 다시 측정했다 — 이게 없으면
"멈췄다고 표시되는데 계속 수집하는" 상태를 볼 수 없다:

```bash
bash local/loadgen.sh longsql            # 6초짜리 쿼리를 만든다
curl -XPOST -H 'x-dbmon-control: 1' :8080/api/collector/pause
sleep 6                                  # ⚠ 아래 설명 — 즉시 멈추지 않는다
bash local/loadgen.sh longsql            # 정지 구간에 다시 부하
# → 정지 시각 이후에 시작된 기록: 0건   (유령 태스크 없음)
curl -XPOST -H 'x-dbmon-control: 1' :8080/api/collector/resume
bash local/loadgen.sh longsql
# → 재개 후 기록 1건, "수집 태스크 시작" 로그는 인스턴스당 1회 (중복 없음)
```

⚠ **정지는 즉시가 아니다.** API 는 플래그만 세우고, 리더 루프가 다음 tick(≤1초)에
그걸 보고 드레인(≤3초)한다. 그래서 **정지 직후 약 4초 안에 시작된 쿼리는 여전히
기록된다** — `sleep 6` 없이 측정하면 "정지했는데 기록이 남는다" 로 보인다(실제로 그렇게
한 번 놀랐다). 이건 결함이 아니라 드레인의 정의다: 이미 관측 중인 쿼리는 확정한다.

### 2.5 실패 경로도 확인됨 (이게 리뷰의 절반이었다)

```bash
# ① 백엔드를 10초 죽였다 → 배지 "연결 끊김" → 자동 재접속 → 스파크라인 구간 2개
#    (관측이 없던 구간에 선을 그리지 않는다)
# ② 실시간 화면: "스트림이 1번 끊기거나 밀렸다 — 그 사이의 쿼리는 이 표에 없다"
# ③ 토큰 무효화(서버 재기동): 배지 "인증 실패" + 표 0행 + DOM 에 SELECT 없음
# ④ WS 유휴 종료: 인증만 하고 침묵 → 서버가 60.0초에 닫는다
```

④ 는 **수정 전에 75초를 침묵해도 열려 있었다** — [20 §R9-1](20-review-log.md).

---

## 3. ⚠ 다음에 할 일

### 3.1 원격이 없다

`git remote -v` 가 비어 있고 `main` 브랜치도 없다 — **로컬 전용 레포**다. 커밋은
전부 `feat/m0-foundation` 에 있고, 원격을 붙이면 그때 푸시·PR 을 한다.

### 3.2 M6 의 남은 것 (백엔드)

- `next_cursor` 발급 — `list_by_instance` 는 페이지를 **내부에서** 따라가고
  `LastEvaluatedKey` 를 밖으로 주지 않아 **페이지네이션이 없다.** 화면은 상한에
  걸리면 그 사실을 말한다(0건일 때도 — 3라운드 자체 발견). 다중 인스턴스에서
  제대로 된 커서는 인스턴스별 위치를 한 토큰에 담아야 한다(k-way 병합)
- 커서 서명 키가 프로세스마다 다르다 — 다중 워커에서 커서 공유 불가
- **실행 고유 식별자가 없다.** 레코드의 신원은 `(인스턴스, 스레드, 시작 초)` 추정이고,
  같은 실행의 두 관측을 붙이려면 ±창 + 겹침 + 추정 오차 폭(1초)을 쓴다. `long_query_time`
  이 1초 미만이면 **연속한 두 실행**이 그 폭 안에 들어와 합쳐질 수 있다(되돌릴 수 없다).
  근본 해결은 `performance_schema` 의 `EVENT_ID`(스레드 안에서 실행마다 증가)를 레코드에
  담아 신원으로 쓰는 것이다 — 수집 경로와 스키마를 함께 건드린다. **다음 세션의 결정 사항.**
- **동시 첫 쓰기 경합**은 남아 있다. 실시간 캡처와 슬로우로그가 정확히 같은 순간에
  첫 쓰기를 하면 둘 다 "없다" 를 보고 항목이 둘 생긴다(키 표류는 6라운드에서 고쳤고,
  이건 다른 경로다). 조회는 `dedupe_executions` 로 한 줄로 접고 정리도 정상 동작하지만,
  근본 수정은 `SK` 를 `record_id` 의 초 버킷으로 **결정론화**하는 것이다 — 스키마
  변경이므로 기존 데이터(TTL 35일)와 두 체계가 공존한다. **다음 세션의 결정 사항.**
- 콜드 티어(Athena) 조회 경로
- `POST /api/queries`(샘플 실행), 실행계획 본문 조회 API

**화면은 이것들을 만들지 않았다.** 없는 데이터를 위한 빈 화면은 "데이터가 없다"
로 오해되기 때문이다([09 §3.2] 판단 유지).

### 3.3 M5 Cognito

배포 환경(prd·stg)은 지금 **접속 자체가 불가능**하다(fail closed). 화면은
`/api/auth/config` 의 `mode` 를 읽어 "이 환경은 아직 접속할 수 없다" 를 말한다.
Cognito 가 배선되면 SPA 서빙 게이트(`serves_local_ui()`)도 함께 열어야 한다.

### 3.3a 로컬 수집은 `DBMON_TARGET_PASSWORD` 가 있어야 돈다

없이 띄우면 IAM 토큰을 로컬 MySQL 에 보내려 하고, 평문 접속의 RSA 한도에 걸려
**연결 옵션 구성 실패**로 끝난다. 로그에 그 사실이 정확히 찍히지만(친절한 메시지),
**슬로우로그 백필은 그대로 돌기 때문에** 화면에는 데이터가 있어 눈치채기 어렵다 —
실시간 캡처(`processlist`)와 진행 중 상태만 조용히 사라진다. 이 세션에서 실제로
그렇게 30분을 흘렸다.

```bash
DBMON_TARGET_PASSWORD=dbmon-local-monitor \
  ./target/release/dbmon --config local/dbmon.toml --log-pretty serve
```

### 3.4 로컬 환경에 남아 있는 것

| 대상 | 상태 | 조치 |
|---|---|---|
| `dbmon-web` 컨테이너 (`dbmon:round8`) | **정지시켰다** (3라운드 검증 중). 낡은 바이너리로 수집 리더 리스를 19시간 쥐고 있어 **호스트 바이너리가 리더가 되지 못했다** — 원인을 찾는 데 5분을 썼다 | 정지 상태로 둔다. 다시 띄우면 리스를 다시 뺏는다 |
| `dbmon-dev-dbmon-1` (컴포즈 `monitor`) | 이번 세션에서 새 이미지로 재생성. **`healthy`** | 해결됨 — 아래 |
| `dbmon-data-wstest` · `dbmon-data-uitest` | 세션들이 만든 격리 테이블 | 필요 없으면 삭제 |
| 호스트 `target/debug/dbmon` | 8081 에 떠 있을 수 있다(`DBMON__HTTP__BIND=0.0.0.0`, uitest 테이블) | `pkill -f "target/debug/dbmon"` |

### 3.5 컨테이너 `unhealthy` 의 원인 — 해결됨

`command:` 의 `--config /etc/dbmon.toml` 은 엔트리포인트에만 붙는다. Dockerfile 의
HEALTHCHECK(`dbmon healthcheck`)는 그 인자를 못 받으므로 설정을 **환경변수만으로**
읽었고, `aws.account_id` 가 없어 검증에서 실패했다.

```text
$ docker exec dbmon-dev-dbmon-1 dbmon healthcheck
Error: 설정을 읽을 수 없다 … [aws.account_id]: 필수다     ← 헬스가 아니라 설정 문제
```

→ 컴포즈에 `DBMON_CONFIG: /etc/dbmon.toml` 을 넣었다(clap 이 이미 이 환경변수를
읽는다). 지금은 `Up (healthy)` 다.

`just` 가 이 머신에 **설치돼 있지 않다.** justfile 타깃은 실행으로 검증되지 않았다.

---

## 4. 이번 구간의 설계 결정 (프론트)

### 4.1 판정과 배관을 나눴다

백엔드가 `ws.rs`(배관)와 `topic.rs`(판정)를 나눈 것과 같은 이유다. `live-reduce.ts`
는 순수 함수라 테스트가 전부 여기 걸리고, 소켓 쪽에 남는 것은 순서와 수명뿐이다.
그쪽도 가짜 WebSocket 으로 12개 테스트를 붙였다 — **구독을 안 보내는 버그는 화면이
조용히 비는 것**으로 나타나고, 그게 이 프로젝트에서 열 번 재발한 유형이다.

### 4.2 연결은 하나, 토픽은 참조 계수

화면마다 소켓을 열면 서버의 재인증·구독 상한이 화면 수만큼 늘고 같은 방송을 N배로
받는다. 모듈 싱글턴 하나를 공유하고 화면은 "이 토픽이 필요하다" 만 말한다. 두
화면이 같은 토픽을 요구할 수 있으므로 **계수를 센다** — 계수가 없으면 한 화면이
언마운트될 때 다른 화면의 스트림이 끊긴다.

### 4.3 `null` 을 `0` 으로 만들지 않는다

백엔드는 "아직 비율을 낼 수 없다" 를 `null` 로 준다. `?? 0` 으로 채우면 화면이
"쿼리가 없다" 로 읽힌다. 포맷터가 전부 `—` 를 내고, 스파크라인은 없는 구간을 잇지
않으며, 합계 타일도 표본이 없으면 `—` 다. **이 규칙을 두 번 어겼고 둘 다 리뷰가
잡았다**([20 §R9-4], [20 §R9-9]).

### 4.4 조용히 삼키는 것을 만들지 않는다

- 거부된 구독(`denied`) → 배너. "데이터 없음" 과 구분돼야 한다
- 프로토콜 오류(`malformed`·`auth_timeout`) → 배너. 프론트·백엔드 불일치 신호다
- 방송 밀림·연결 끊김(`missedCount`) → 목록 재조회 + 실시간 화면에 구멍 표시
- 렌더 예외 → `ErrorBoundary`. 하얀 화면은 모니터링 도구에서 "서버가 죽었나" 로
  오해된다
- 구독 상한(50)에 걸려 자른 인스턴스 → 표 아래에 몇 개를 못 받는지 적는다

### 4.5 SPA 는 로컬 개발에서만 서빙한다

`serves_local_ui()` 게이트를 그대로 유지했다(prd 404). Cognito 검증이 없으므로
prd 에서 서빙하면 **인증을 통과할 수 없는 깨진 화면**이다. 없는 `/api/…` 경로가
SPA 폴백에 삼켜지지 않도록 404 라우트를 명시했다 — 200 에 HTML 을 주면 "아직
구현되지 않았다" 가 "응답이 이상하다" 로 바뀐다.

---

## 5. 미해결 (이전부터)

- **OPEN-Q-15 컴플라이언스 절반**: 운영 데이터 리터럴 저장 허용 여부는 사용자 판단
  대기. 엔지니어링 절반은 결정·문서화됨(기본 `masked`)
- M5 Cognito JWT 검증(JWKS·kid 캐시·`AuthContext::intersect`)
- 콜드 티어(Athena) 페이지네이션, `next_cursor` 발급
- M3 부트스트랩, M7 아카이브
- `docs/20-review-log.md` 의 이연 항목: 쓰기 증폭(M4), 다이제스트 스냅샷 배선
  (M12/M11/M13-15), M24-26, `is_collectible()` 수집 시점 재검사(R6-24),
  STS 계정 검증(R6-25)
- 첫 `finalized` 가 실제보다 짧게 보고되는 건(span 추정) — 화면은 최종값을 쓰므로
  표시 문제는 없다. 정확 지표는 백필된 값을 봐야 한다

---

## 부록 A. WS 확인 스크립트

`web/` 밖에 두면 잃어버리므로 여기 남긴다. 느린 쿼리는 **`docker exec -d`** 로
쏜다(`-d` 가 없으면 셸이 완료를 기다려 관측 창과 겹치지 않는다).

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
  if (m.t === "slowq")  got.push(`slowq ${m.data.state} ${m.data.duration_ms}ms`);
  if (m.t === "status") got.push(`status qps=${m.metrics.qps ?? "—"}`);
  if (m.t === "subscribed") got.push(`subscribed=${JSON.stringify(m.topics)} denied=${JSON.stringify(m.denied)}`);
};
setTimeout(() => { console.log(got.join("\n") || "(아무것도 오지 않았다)"); process.exit(0); }, 20000);
```

유휴 종료를 확인하려면 **auth 만 보내고 아무것도 하지 않는다.** 60초에 닫혀야 한다.

⚠ 느린 쿼리를 만들 때 `WHERE SLEEP(4)=0` 형태를 쓰지 말 것 — 조인 행마다 평가돼
며칠이 걸린다. `SELECT SLEEP(7);` 처럼 스칼라로 쓴다.

## 부록 B. 커밋 전에 돌릴 것

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm --prefix web run build      # tsc --noEmit 포함
npm --prefix web test
./scripts/check-docs.py         # 이 문서의 링크도 검사된다
```
