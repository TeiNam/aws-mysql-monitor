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
  AuthConfig,
  AwsInfo,
  CollectorStatus,
  DigestRow,
  InstanceStats,
  InstanceView,
  ListResponse,
  PlanView,
  SlowQueryView,
  UserStats,
} from "./types";

/** 백엔드가 답한 오류. `code` 는 `{"error": …}` 의 값이다. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
  ) {
    super(`${status} ${code}`);
    this.name = "ApiError";
  }
}

/** 인증 실패인가. 화면이 "토큰이 필요하다" 안내로 갈아탈 신호다. */
export function isUnauthorized(error: unknown): boolean {
  return error instanceof ApiError && error.status === 401;
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

export function fetchCollectorStatus(signal: AbortSignal | null): Promise<CollectorStatus> {
  return apiGet<CollectorStatus>("/api/collector/status", signal);
}

/**
 * 수집 제어. **쓰기 경로이므로 `operator` 이상만 통과한다** — 403 이면 화면이
 * "권한이 없다" 를 말해야 한다(버튼을 숨기지 않는다: 왜 못 누르는지 알려야 한다).
 */
async function post<T>(path: string): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { accept: "application/json", ...authHeaders() },
    cache: "no-store",
  });
  if (res.status === 401) {
    clearToken();
    throw new ApiError(401, await errorCodeOf(res));
  }
  if (!res.ok) throw new ApiError(res.status, await errorCodeOf(res));
  return (await res.json()) as T;
}

export const pauseCollector = () => post<CollectorStatus>("/api/collector/pause");
export const resumeCollector = () => post<CollectorStatus>("/api/collector/resume");
export const runDiscovery = () => post<CollectorStatus>("/api/discovery/run");
export const runBackfill = () => post<CollectorStatus>("/api/backfill/run");

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
};
