/**
 * Cognito Hosted UI 로그인 (Authorization Code + PKCE).
 *
 * # 왜 PKCE 인가
 *
 * SPA 는 **클라이언트 시크릿을 숨길 수 없다.** 그래서 앱 클라이언트를 퍼블릭으로 두고,
 * 인가 코드를 훔쳐도 쓸 수 없게 PKCE 로 묶는다: 브라우저가 `code_verifier` 를 만들고
 * 해시(`code_challenge`)만 먼저 보낸다. 코드를 토큰으로 바꿀 때 원본을 제시해야 하므로,
 * 리다이렉트 URL 에서 코드를 가로챈 쪽은 교환할 수 없다.
 *
 * Implicit 플로우는 **쓰지 않는다** — 액세스 토큰이 URL 프래그먼트로 오면 브라우저
 * 히스토리에 남는다. Terraform 이 `allowed_oauth_flows = ["code"]` 로 못 박는다.
 *
 * # 저장 위치
 *
 * `sessionStorage` 다. `localStorage` 를 쓰지 않는 이유: 탭을 닫으면 세션이 끝나는 편이
 * 맞고, XSS 가 있을 때 노출 창이 짧다. 기존 공유 토큰 경로(`auth.ts`)와 같은 판단이다.
 */

import { TOKEN_STORAGE_KEY } from "./auth";

const VERIFIER_KEY = "dbmon.pkce.verifier";
const STATE_KEY = "dbmon.pkce.state";
const REFRESH_KEY = "dbmon.cognito.refresh";
const EXPIRES_KEY = "dbmon.cognito.expires";

/** 액세스 토큰 만료 몇 밀리초 전에 갱신할까. */
export const REFRESH_MARGIN_MS = 5 * 60 * 1000;

/** `/api/auth/config` 의 `cognito` 부분. */
export interface CognitoConfig {
  user_pool_id: string;
  client_id: string;
  region: string | null;
  domain: string;
}

/** 로그인 화면으로 보낼 준비물. */
export interface AuthorizeRequest {
  url: string;
  verifier: string;
  state: string;
}

/**
 * Hosted UI 도메인을 절대 URL 로 만든다.
 *
 * 설정에 `https://` 가 붙어 올 때도 있고 접두어만 올 때도 있다 — 둘 다 받는다.
 * 판정을 화면 여러 곳에 두면 한쪽만 고쳐지고, 그러면 로그인 링크가 깨진다.
 */
export function hostedUiBase(config: CognitoConfig): string | null {
  const raw = config.domain.trim().replace(/\/+$/, "");
  if (!raw) return null;
  if (raw.startsWith("https://")) return raw;
  if (raw.startsWith("http://")) return null; // 인가 코드를 평문으로 보내지 않는다
  const region = config.region;
  if (!region) return null;
  // 접두어만 왔다.
  return `https://${raw}.auth.${region}.amazoncognito.com`;
}

/** 콜백 주소 — 현재 오리진 기준. Terraform 의 `callback_urls` 와 같아야 한다. */
export function callbackUrl(origin: string = window.location.origin): string {
  return `${origin}/auth/callback`;
}

/** base64url 인코딩 (패딩 없음). */
function base64url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** 무작위 문자열 — `code_verifier` 와 `state` 에 쓴다. */
export function randomString(byteLength = 32): string {
  const buf = new Uint8Array(byteLength);
  crypto.getRandomValues(buf);
  return base64url(buf);
}

/** `code_challenge` = base64url(SHA256(verifier)). */
export async function challengeOf(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(verifier),
  );
  return base64url(new Uint8Array(digest));
}

/**
 * 로그인 URL 을 만든다. **`state` 와 `verifier` 를 함께 돌려준다** — 호출부가
 * 저장해야 콜백에서 대조할 수 있다.
 */
export async function buildAuthorizeRequest(
  config: CognitoConfig,
  origin?: string,
): Promise<AuthorizeRequest | null> {
  const base = hostedUiBase(config);
  if (!base || !config.client_id) return null;

  const verifier = randomString();
  const state = randomString(16);
  const params = new URLSearchParams({
    response_type: "code",
    client_id: config.client_id,
    redirect_uri: callbackUrl(origin),
    scope: "openid email profile",
    state,
    code_challenge: await challengeOf(verifier),
    code_challenge_method: "S256",
  });
  return { url: `${base}/oauth2/authorize?${params}`, verifier, state };
}

/** 로그인 화면으로 보낸다. */
export async function beginLogin(config: CognitoConfig): Promise<boolean> {
  const req = await buildAuthorizeRequest(config);
  if (!req) return false;
  sessionStorage.setItem(VERIFIER_KEY, req.verifier);
  sessionStorage.setItem(STATE_KEY, req.state);
  window.location.assign(req.url);
  return true;
}

/** 토큰 교환 응답. */
export interface TokenResponse {
  access_token: string;
  id_token?: string;
  refresh_token?: string;
  expires_in: number;
}

/** 콜백 URL 에서 읽은 값. */
export interface CallbackParams {
  code: string | null;
  state: string | null;
  error: string | null;
  errorDescription: string | null;
}

/** 콜백 쿼리스트링을 읽는다. */
export function readCallback(search: string): CallbackParams {
  const p = new URLSearchParams(search);
  return {
    code: p.get("code"),
    state: p.get("state"),
    error: p.get("error"),
    errorDescription: p.get("error_description"),
  };
}

/**
 * `state` 를 대조한다 — **CSRF 방어다.**
 *
 * 공격자가 자기 인가 코드로 콜백 URL 을 만들어 피해자에게 열게 하면, 피해자의
 * 세션이 공격자 계정으로 바뀐다(세션 고정). 저장된 `state` 와 같아야 진행한다.
 *
 * 저장된 값이 없으면 **거부한다.** 우리가 시작하지 않은 로그인이다.
 */
export function stateMatches(received: string | null): boolean {
  const expected = sessionStorage.getItem(STATE_KEY);
  if (!expected || !received) return false;
  return expected === received;
}

/** 교환에 쓸 요청 본문. 퍼블릭 클라이언트라 시크릿이 없다. */
export function tokenExchangeBody(
  config: CognitoConfig,
  code: string,
  verifier: string,
  origin?: string,
): URLSearchParams {
  return new URLSearchParams({
    grant_type: "authorization_code",
    client_id: config.client_id,
    code,
    redirect_uri: callbackUrl(origin),
    code_verifier: verifier,
  });
}

/** 갱신에 쓸 요청 본문. */
export function refreshBody(
  config: CognitoConfig,
  refreshToken: string,
): URLSearchParams {
  return new URLSearchParams({
    grant_type: "refresh_token",
    client_id: config.client_id,
    refresh_token: refreshToken,
  });
}

/**
 * 토큰 교환 실패의 종류.
 *
 * # 왜 구분하는가 (교차 리뷰 4차가 잡은 결함)
 *
 * 처음에는 모든 오류를 "리프레시 토큰 재사용 감지" 와 같이 취급해 세션을 버렸다.
 * 그러면 **DNS 오류·5xx·네트워크 단절 한 번으로 아직 유효한 액세스 토큰과 리프레시
 * 토큰까지 지워지고 사용자가 로그아웃된다.**
 *
 * 되돌릴 수 없는 것만 세션을 버린다:
 *
 * | 응답 | 뜻 | 세션 |
 * |---|---|---|
 * | 400·401 | `invalid_grant` — 토큰이 무효하거나 폐기됐다 | **버린다** |
 * | 그 외 4xx | 우리 요청이 잘못됐다 (설정 오류) | 유지 (고칠 수 있다) |
 * | 5xx·네트워크 | 일시적이다 | 유지 — 다음 주기에 재시도 |
 */
export class TokenExchangeError extends Error {
  constructor(
    message: string,
    /** 참이면 리프레시 토큰이 확실히 무효다 — 세션을 버려야 한다. */
    readonly definitive: boolean,
  ) {
    super(message);
    this.name = "TokenExchangeError";
  }
}

/**
 * 이 응답이 **리프레시 토큰이 확실히 무효** 임을 뜻하는가.
 *
 * # 상태 코드만으로는 판정할 수 없다 (교차 리뷰 6차)
 *
 * Cognito 는 여러 OAuth 오류를 **400** 으로 돌려준다:
 *
 * | `error` | 뜻 | 세션 |
 * |---|---|---|
 * | `invalid_grant` | 리프레시 토큰이 무효·폐기됐다 | **버린다** |
 * | `invalid_client` | 클라이언트 ID 가 틀렸다 (설정 오류) | 유지 — 설정을 고치면 된다 |
 * | `invalid_request` | 요청 형식이 틀렸다 (우리 버그) | 유지 |
 * | `unauthorized_client` | 그랜트가 허용되지 않았다 (설정 오류) | 유지 |
 *
 * 400 을 전부 무효로 보면 **클라이언트 ID 전환 실수 하나로 모든 사용자가 로그아웃된다.**
 * 그래서 본문의 `error` 를 읽는다. 읽을 수 없으면(본문이 없거나 JSON 이 아니면)
 * 보수적으로 **유지**한다 — 잘못 버리는 것이 잘못 남기는 것보다 나쁘다(남겨도 다음
 * 요청이 401 을 받아 화면이 사유를 말한다).
 */
export function isDefinitiveAuthFailure(status: number, oauthError?: string): boolean {
  if (status === 401) return true;
  if (status !== 400) return false;
  return oauthError === "invalid_grant";
}

/** Cognito 토큰 엔드포인트를 호출한다. */
async function postToken(
  config: CognitoConfig,
  body: URLSearchParams,
): Promise<TokenResponse> {
  const base = hostedUiBase(config);
  if (!base) throw new TokenExchangeError("Cognito 도메인이 설정되지 않았다", false);

  let res: Response;
  try {
    res = await fetch(`${base}/oauth2/token`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body,
    });
  } catch (e) {
    // 네트워크 실패. **세션을 버리지 않는다.**
    throw new TokenExchangeError(
      `토큰 엔드포인트에 닿을 수 없다: ${e instanceof Error ? e.message : String(e)}`,
      false,
    );
  }

  if (!res.ok) {
    // OAuth 오류 코드를 읽는다 — 400 안에서 갈라야 한다(위 함수 참조).
    // **본문 전체를 노출하지 않는다.** 오류에 코드·리다이렉트가 실릴 수 있다.
    let oauthError: string | undefined;
    try {
      const body = (await res.json()) as { error?: unknown };
      if (typeof body.error === "string") oauthError = body.error;
    } catch {
      // 본문이 없거나 JSON 이 아니다. 보수적으로 유지한다.
    }
    throw new TokenExchangeError(
      `토큰 교환 실패 (HTTP ${res.status}${oauthError ? `, ${oauthError}` : ""})`,
      isDefinitiveAuthFailure(res.status, oauthError),
    );
  }
  return (await res.json()) as TokenResponse;
}

/**
 * 콜백을 완료한다 — 코드를 토큰으로 바꾼다.
 *
 * 성공하면 액세스 토큰을 돌려주고 리프레시 토큰·만료를 저장한다. **`verifier` 는
 * 한 번만 쓴다** — 교환 후 지운다.
 */
export async function completeLogin(
  config: CognitoConfig,
  params: CallbackParams,
): Promise<string> {
  if (params.error) {
    throw new Error(params.errorDescription || params.error);
  }
  if (!params.code) throw new Error("인가 코드가 없다");
  if (!stateMatches(params.state)) {
    throw new Error("state 가 일치하지 않는다 — 로그인을 다시 시작한다");
  }
  const verifier = sessionStorage.getItem(VERIFIER_KEY);
  if (!verifier) throw new Error("PKCE 검증자가 없다 — 로그인을 다시 시작한다");

  try {
    const tokens = await postToken(
      config,
      tokenExchangeBody(config, params.code, verifier),
    );
    storeTokens(tokens);
    return tokens.access_token;
  } finally {
    // 성공이든 실패든 재사용하지 않는다.
    sessionStorage.removeItem(VERIFIER_KEY);
    sessionStorage.removeItem(STATE_KEY);
  }
}

/** 리프레시 토큰으로 액세스 토큰을 새로 받는다. 실패하면 `null`. */
export async function refreshAccessToken(
  config: CognitoConfig,
): Promise<string | null> {
  const refresh = sessionStorage.getItem(REFRESH_KEY);
  if (!refresh) return null;
  try {
    const tokens = await postToken(config, refreshBody(config, refresh));
    storeTokens(tokens);
    return tokens.access_token;
  } catch (e) {
    // **확실한 인증 실패만 로그아웃이다.** 재사용 감지에 걸렸으면 계보 전체가
    // 무효이므로 계속 재시도하면 안 된다.
    //
    // 일시적 실패(네트워크·5xx)는 세션을 유지한다 — 아직 유효한 액세스 토큰이
    // 남아 있고, 다음 주기에 다시 시도하면 된다(교차 리뷰 4차).
    if (e instanceof TokenExchangeError && e.definitive) {
      clearSession();
    }
    return null;
  }
}

/** 액세스 토큰이 곧 만료되는가. */
export function needsRefresh(nowMs: number = Date.now()): boolean {
  const raw = sessionStorage.getItem(EXPIRES_KEY);
  if (!raw) return false;
  const expires = Number(raw);
  if (!Number.isFinite(expires)) return false;
  return expires - nowMs <= REFRESH_MARGIN_MS;
}

/** 만료 시각을 계산한다. `expires_in` 은 초 단위다. */
export function expiryOf(expiresInSec: number, nowMs: number): number {
  return nowMs + Math.max(0, expiresInSec) * 1000;
}

function storeTokens(tokens: TokenResponse): void {
  // 액세스 토큰은 기존 경로(`auth.ts`)에 저장한다 — `authHeaders()` 가 한 곳만 본다.
  sessionStorage.setItem(TOKEN_STORAGE_KEY, tokens.access_token);
  if (tokens.refresh_token) {
    sessionStorage.setItem(REFRESH_KEY, tokens.refresh_token);
  }
  sessionStorage.setItem(
    EXPIRES_KEY,
    String(expiryOf(tokens.expires_in, Date.now())),
  );
}

/** 세션을 버린다. */
export function clearSession(): void {
  for (const k of [VERIFIER_KEY, STATE_KEY, REFRESH_KEY, EXPIRES_KEY, TOKEN_STORAGE_KEY]) {
    sessionStorage.removeItem(k);
  }
}

/** 로그아웃 URL — Hosted UI 세션도 함께 끊는다. */
export function logoutUrl(config: CognitoConfig, origin?: string): string | null {
  const base = hostedUiBase(config);
  if (!base) return null;
  const params = new URLSearchParams({
    client_id: config.client_id,
    logout_uri: (origin ?? window.location.origin) + "/",
  });
  return `${base}/logout?${params}`;
}
