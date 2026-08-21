/**
 * HTTP 조회 클라이언트.
 *
 * # 401 을 받으면 토큰을 버린다
 *
 * 낡은 토큰으로 재시도를 반복하면 (a) 서버 로그가 401 로 가득 차고 (b) 화면은
 * "로딩 중" 에서 멈춘 것처럼 보인다. 한 번 거부된 토큰은 즉시 버리고 사용자에게
 * 새 접속 URL 을 받으라고 말한다.
 *
 * # 오류 코드를 그대로 들고 온다
 *
 * 백엔드는 `{"error":"env_not_allowed"}` 처럼 코드만 준다(내부 사정을 노출하지
 * 않는 설계). 화면은 이 코드로 문장을 고른다 — HTTP 상태만 보면 "권한 없음" 과
 * "잘못된 범위" 가 같은 메시지가 된다.
 */

import { authHeaders, clearToken } from "./auth";
import type {
  AggregateEnvelope,
  AppSettings,
  AuthConfig,
  AwsInfo,
  CollectorStatus,
  DigestRow,
  FleetMetricsResponse,
  InstanceMetricsResponse,
  InstanceStats,
  InstanceView,
  ListResponse,
  PlanView,
  SettingsProblem,
  SettingsView,
  TuningView,
  SlowQueryView,
  UserStats,
} from "./types";

/**
 * 백엔드가 답한 오류. `code` 는 `{"error": …}` 의 값이다.
 *
 * `detail` 은 서버가 함께 준 사람이 읽을 사유다(`{"reason": …}`). **코드만 보여주면
 * 사용자가 무엇을 고칠지 알 수 없는 경우**가 있다 — 모델 호출 실패가 그렇다:
 * 모델 ID 오타·리전에 없는 모델·콘텐츠 필터가 전부 같은 코드로 온다.
 */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    readonly detail?: string,
  ) {
    super(`${status} ${code}`);
    this.name = "ApiError";
  }
}

/** 인증 실패인가. 화면이 "토큰이 필요하다" 안내로 갈아탈 신호다. */
export function isUnauthorized(error: unknown): boolean {
  return error instanceof ApiError && error.status === 401;
}

/** 오류 본문의 `reason`. 없으면 `undefined`. */
async function errorOf(res: Response): Promise<{ code: string; reason?: string }> {
  try {
    const body: unknown = await res.json();
    if (body !== null && typeof body === "object") {
      const obj = body as { error?: unknown; reason?: unknown };
      return {
        code: typeof obj.error === "string" ? obj.error : `http_${res.status}`,
        ...(typeof obj.reason === "string" ? { reason: obj.reason } : {}),
      };
    }
  } catch {
    // 본문이 JSON 이 아니다 — 상태 코드로라도 알린다.
  }
  return { code: `http_${res.status}` };
}

async function errorCodeOf(res: Response): Promise<string> {
  // 오류 본문이 JSON 이 아닐 수 있다(프록시가 끼어든 경우). **거기서 또 던지지
  // 않는다** — 원래 실패를 상태 코드로라도 알려야 한다.
  try {
    const body: unknown = await res.json();
    if (body !== null && typeof body === "object" && "error" in body) {
      const code = (body as { error: unknown }).error;
      if (typeof code === "string") return code;
    }
  } catch {
    // 무시하고 아래 기본값으로.
  }
  return `http_${res.status}`;
}

async function request(path: string, signal: AbortSignal | null): Promise<Response> {
  const res = await fetch(path, {
    headers: { accept: "application/json", ...authHeaders() },
    // 백엔드가 캐시 헤더를 주지 않는다. 브라우저 휴리스틱 캐시가 낡은 목록을
    // 돌려주면 "왜 새 쿼리가 안 보이나" 를 코드에서 찾게 된다.
    cache: "no-store",
    signal,
  });
  if (res.status === 401) {
    clearToken();
    throw new ApiError(401, await errorCodeOf(res));
  }
  if (!res.ok) throw new ApiError(res.status, await errorCodeOf(res));
  return res;
}

async function apiGet<T>(path: string, signal: AbortSignal | null): Promise<T> {
  const res = await request(path, signal);
  let body: unknown;
  try {
    body = await res.json();
  } catch {
    // 200 을 받았는데 본문이 JSON 이 아니다(프록시가 HTML 을 끼워 넣는 경우).
    // 이걸 그냥 던지면 화면이 "서버에 닿지 못했다" 고 말한다 — 닿았는데.
    throw new ApiError(res.status, "malformed_response");
  }
  if (body === null || typeof body !== "object") {
    // 스키마 전체를 검증하지는 않는다(백엔드 뷰 타입이 계약이다). 다만 컨테이너
    // 종류가 다르면 화면이 `undefined.map` 으로 깨지므로 여기서 막는다.
    throw new ApiError(res.status, "malformed_response");
  }
  return body as T;
}

/** 조회 파라미터. 값이 `undefined`·빈 문자열인 항목은 **보내지 않는다.** */
export type QueryParams = Record<string, string | number | undefined>;

export function queryString(params: QueryParams): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === "") continue;
    search.set(key, String(value));
  }
  const s = search.toString();
  return s === "" ? "" : `?${s}`;
}

export function fetchAuthConfig(signal: AbortSignal | null): Promise<AuthConfig> {
  return apiGet<AuthConfig>("/api/auth/config", signal);
}

export function fetchAwsInfo(signal: AbortSignal | null): Promise<AwsInfo> {
  return apiGet<AwsInfo>("/api/aws/info", signal);
}

export async function fetchInstances(signal: AbortSignal | null): Promise<InstanceView[]> {
  const body = await apiGet<InstanceView[]>("/api/instances", signal);
  if (!Array.isArray(body)) throw new ApiError(200, "malformed_response");
  return body;
}

export async function fetchSlowQueries(
  params: QueryParams,
  signal: AbortSignal | null,
): Promise<ListResponse> {
  const body = await apiGet<ListResponse>(`/api/slow-queries${queryString(params)}`, signal);
  if (!Array.isArray(body.items)) throw new ApiError(200, "malformed_response");
  return body;
}

export async function fetchPlans(
  params: QueryParams,
  signal: AbortSignal | null,
): Promise<ListResponse> {
  const body = await apiGet<ListResponse>(`/api/plans${queryString(params)}`, signal);
  if (!Array.isArray(body.items)) throw new ApiError(200, "malformed_response");
  return body;
}

/**
 * `record_id` 는 `계정/리전/이름:스레드:초` 라서 **슬래시를 담는다.** 인코딩하지
 * 않으면 라우트가 한 세그먼트에 걸리지 않아 404 가 된다.
 */
export function fetchSlowQuery(
  recordId: string,
  signal: AbortSignal | null,
): Promise<SlowQueryView> {
  return apiGet<SlowQueryView>(`/api/queries/${encodeURIComponent(recordId)}`, signal);
}

export function fetchPlan(recordId: string, signal: AbortSignal | null): Promise<PlanView> {
  return apiGet<PlanView>(`/api/queries/${encodeURIComponent(recordId)}/plan`, signal);
}

/**
 * 배열 필드가 없으면 빈 배열로 접는다.
 *
 * **롤링 배포 중에는 구 백엔드가 응답한다.** `paused_scopes` 가 없으면
 * `s.paused_scopes.length` 가 던지고, 그 예외는 카드 하나가 아니라 **화면 전체를
 * 흰 화면으로** 만든다(실측: `controllable_envs` 누락으로 RDS 화면이 죽었다).
 * 배포 순서에 의존하지 않도록 경계에서 한 번 정규화한다.
 */
const withArrayDefaults = (s: CollectorStatus): CollectorStatus => ({
  ...s,
  paused_scopes: s.paused_scopes ?? [],
  controllable_envs: s.controllable_envs ?? [],
});

/**
 * 플릿 메트릭. **엔진별 최소 세트만** 온다 — 연결·QPS 는 WS 스냅샷에서 받는다
 * (자체 수집이 더 신선하고 무료다).
 */
export function fetchFleetMetrics(signal: AbortSignal | null): Promise<FleetMetricsResponse> {
  return apiGet<FleetMetricsResponse>("/api/metrics/fleet", signal);
}

/** 인스턴스 하나의 전체 메트릭. `rangeMs` 만큼 과거부터 지금까지. */
export function fetchInstanceMetrics(
  instanceId: string,
  rangeMs: number,
  signal: AbortSignal | null,
): Promise<InstanceMetricsResponse> {
  const to = Date.now();
  const qs = queryString({ from_ms: to - rangeMs, to_ms: to });
  return apiGet<InstanceMetricsResponse>(
    `/api/metrics/instance/${encodeURIComponent(instanceId)}${qs}`,
    signal,
  );
}

export function fetchCollectorStatus(signal: AbortSignal | null): Promise<CollectorStatus> {
  return apiGet<CollectorStatus>("/api/collector/status", signal).then(withArrayDefaults);
}

/**
 * 수집 제어. **쓰기 경로이므로 `operator` 이상만 통과한다** — 403 이면 화면이
 * "권한이 없다" 를 말해야 한다(버튼을 숨기지 않는다: 왜 못 누르는지 알려야 한다).
 */
async function post<T>(path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: {
      accept: "application/json",
      // **CSRF 방어.** 커스텀 헤더는 크로스 오리진에서 preflight 를 통과해야 보낼 수
      // 있고 서버는 CORS 를 열지 않는다 — 로컬 개발(토큰 없이 통과)에서 아무 웹페이지가
      // `POST /api/collector/pause` 로 수집을 멈추는 것을 막는다.
      "x-dbmon-control": "1",
      ...(body === undefined ? {} : { "content-type": "application/json" }),
      ...authHeaders(),
    },
    // `exactOptionalPropertyTypes` 라 `undefined` 를 넣을 수 없다 — 키 자체를 뺀다.
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    cache: "no-store",
  });
  if (res.status === 401) {
    clearToken();
    throw new ApiError(401, await errorCodeOf(res));
  }
  if (!res.ok) {
    // **사유를 함께 들고 온다.** 코드만으로는 무엇을 고칠지 모르는 실패가 있다.
    const { code, reason } = await errorOf(res);
    throw new ApiError(res.status, code, reason);
  }
  return (await res.json()) as T;
}

/**
 * 정지·재개는 **스코프를 반드시 보낸다.** 서버에 기본값이 없다 — 빈 본문을 "전체" 로
 * 해석하면 오래된 화면이 실수로 전 환경 관측을 멈출 수 있다.
 */
const postStatus = (path: string, body?: unknown) =>
  post<CollectorStatus>(path, body).then(withArrayDefaults);

/**
 * 수집을 시작한다 (`Pending` → `Collecting`).
 *
 * **등록은 자동, 시작은 사람이다** — 시작은 대상 DB 에 매초 쿼리를 날리기 시작하는 일이다.
 * 태그·버전·필터가 근거인 상태(`disabled`·`unsupported`·`excluded`)는 서버가 409 로 거부한다.
 */
export const startInstance = (instanceId: string) =>
  post<InstanceView>(`/api/instances/${encodeURIComponent(instanceId)}/start`);

export const pauseCollector = (scope: string) => postStatus("/api/collector/pause", { scope });
export const resumeCollector = (scope: string) => postStatus("/api/collector/resume", { scope });
export const runDiscovery = () => postStatus("/api/discovery/run");
export const runBackfill = () => postStatus("/api/backfill/run");

export function fetchSettings(signal: AbortSignal | null): Promise<SettingsView> {
  return apiGet<SettingsView>("/api/settings", signal);
}

/**
 * 저장 실패 중 **검증 실패만** 필드별 사유를 갖는다. 화면이 그 자리에 표시해야
 * 하므로 오류 코드가 아니라 이 타입으로 꺼낸다.
 */
export class SettingsInvalid extends Error {
  constructor(readonly problems: SettingsProblem[]) {
    super("invalid_settings");
    this.name = "SettingsInvalid";
  }
}

/**
 * 설정을 저장한다. `expected_version` 은 **읽은 값 그대로** 보낸다 —
 * 그 사이 다른 관리자가 저장했으면 409 이고, 화면은 다시 읽어야 한다.
 */
export async function saveSettings(
  expectedVersion: number,
  settings: AppSettings,
): Promise<SettingsView> {
  const res = await fetch("/api/settings", {
    method: "PUT",
    headers: {
      accept: "application/json",
      "content-type": "application/json",
      "x-dbmon-control": "1",
      ...authHeaders(),
    },
    body: JSON.stringify({ expected_version: expectedVersion, settings }),
    cache: "no-store",
  });
  if (res.status === 401) {
    clearToken();
    throw new ApiError(401, await errorCodeOf(res));
  }
  if (res.status === 400) {
    // 검증 실패는 `{error, problems}` 다. 본문을 못 읽으면 일반 오류로 떨어진다.
    const problems = await problemsOf(res);
    if (problems.length > 0) throw new SettingsInvalid(problems);
    throw new ApiError(400, "invalid_settings");
  }
  if (!res.ok) throw new ApiError(res.status, await errorCodeOf(res));
  return (await res.json()) as SettingsView;
}

async function problemsOf(res: Response): Promise<SettingsProblem[]> {
  try {
    const body: unknown = await res.json();
    if (body !== null && typeof body === "object" && "problems" in body) {
      const list = (body as { problems: unknown }).problems;
      if (Array.isArray(list)) return list as SettingsProblem[];
    }
  } catch {
    // 본문이 JSON 이 아니다 — 호출부가 일반 오류로 처리한다.
  }
  return [];
}

export function fetchTuning(recordId: string, signal: AbortSignal | null): Promise<TuningView> {
  return apiGet<TuningView>(`/api/queries/${encodeURIComponent(recordId)}/tuning`, signal);
}

/**
 * 튜닝 권고를 **새로 만든다.** 모델 호출이라 수십 초 걸릴 수 있다.
 *
 * `operator` 이상만 통과한다 — 대상 DB 에 쿼리를 던지고 토큰을 쓰는 조작이다.
 */
export const generateTuning = (recordId: string) =>
  post<TuningView>(`/api/queries/${encodeURIComponent(recordId)}/tuning`);

/** 마크다운은 JSON 이 아니다 — 텍스트로 받아 브라우저 다운로드로 넘긴다. */
export async function fetchMarkdown(recordId: string): Promise<string> {
  const res = await request(`/api/queries/${encodeURIComponent(recordId)}/markdown`, null);
  return res.text();
}

export function fetchDigests(
  params: QueryParams,
  signal: AbortSignal | null,
): Promise<AggregateEnvelope<DigestRow>> {
  return apiGet<AggregateEnvelope<DigestRow>>(`/api/digests${queryString(params)}`, signal);
}

export function fetchInstanceStatistics(
  params: QueryParams,
  signal: AbortSignal | null,
): Promise<AggregateEnvelope<InstanceStats>> {
  return apiGet<AggregateEnvelope<InstanceStats>>(`/api/statistics${queryString(params)}`, signal);
}

export function fetchUserStatistics(
  params: QueryParams,
  signal: AbortSignal | null,
): Promise<AggregateEnvelope<UserStats>> {
  return apiGet<AggregateEnvelope<UserStats>>(
    `/api/statistics/users${queryString(params)}`,
    signal,
  );
}

/** react-query 캐시 키. 문자열을 손으로 적으면 무효화가 조용히 빗나간다. */
export const queryKeys = {
  authConfig: ["auth-config"] as const,
  awsInfo: ["aws-info"] as const,
  collectorStatus: ["collector-status"] as const,
  instances: ["instances"] as const,
  /** 필터 조합 전체. 놓친 방송을 복구할 때 이 접두로 무효화한다. */
  slowQueriesAll: ["slow-queries"] as const,
  slowQueries: (p: QueryParams) => ["slow-queries", p] as const,
  slowQuery: (id: string) => ["slow-query", id] as const,
  plans: (p: QueryParams) => ["plans", p] as const,
  plan: (id: string) => ["plan", id] as const,
  digests: (p: QueryParams) => ["digests", p] as const,
  statistics: (p: QueryParams) => ["statistics", p] as const,
  userStatistics: (p: QueryParams) => ["user-statistics", p] as const,
  fleetMetrics: ["fleet-metrics"] as const,
  settings: ["settings"] as const,
  tuning: (id: string) => ["tuning", id] as const,
  /** 범위가 키에 들어간다 — 안 넣으면 범위를 바꿔도 앞 결과가 그려진다. */
  instanceMetrics: (id: string, rangeMs: number) => ["instance-metrics", id, rangeMs] as const,
};
