import { beforeEach, describe, expect, it } from "vitest";
import {
  buildAuthorizeRequest,
  callbackUrl,
  challengeOf,
  clearSession,
  expiryOf,
  hostedUiBase,
  logoutUrl,
  needsRefresh,
  randomString,
  readCallback,
  refreshBody,
  REFRESH_MARGIN_MS,
  isDefinitiveAuthFailure,
  refreshAccessToken,
  stateMatches,
  tokenExchangeBody,
  type CognitoConfig,
} from "./cognito";

const config: CognitoConfig = {
  user_pool_id: "ap-northeast-2_AbCdEf",
  client_id: "1h57kf5cpq17m0eml12EXAMPLE",
  region: "ap-northeast-2",
  domain: "dbmon-dev-319165777726",
};

beforeEach(() => {
  sessionStorage.clear();
});

describe("hostedUiBase", () => {
  it("접두어만 오면 리전으로 조립한다", () => {
    expect(hostedUiBase(config)).toBe(
      "https://dbmon-dev-319165777726.auth.ap-northeast-2.amazoncognito.com",
    );
  });

  it("절대 URL 이 오면 그대로 쓴다", () => {
    expect(
      hostedUiBase({ ...config, domain: "https://login.example.com/" }),
    ).toBe("https://login.example.com");
  });

  /** **평문 http 를 거부한다** — 인가 코드가 그 주소로 오간다. */
  it("http 도메인을 거부한다", () => {
    expect(hostedUiBase({ ...config, domain: "http://login.example.com" })).toBeNull();
  });

  it("도메인이 비었거나 리전이 없으면 만들지 않는다", () => {
    expect(hostedUiBase({ ...config, domain: "" })).toBeNull();
    expect(hostedUiBase({ ...config, region: null })).toBeNull();
  });
});

describe("PKCE", () => {
  /** `code_challenge` 는 **verifier 의 SHA-256** 이다 (RFC 7636 S256). */
  it("challenge 는 verifier 의 SHA-256 base64url 이다", async () => {
    // RFC 7636 부록 B 의 검증 벡터.
    const verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    expect(await challengeOf(verifier)).toBe(
      "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
    );
  });

  it("verifier 는 매번 다르고 URL 안전 문자만 쓴다", () => {
    const a = randomString();
    const b = randomString();
    expect(a).not.toBe(b);
    expect(a).toMatch(/^[A-Za-z0-9_-]+$/);
    // RFC 7636 은 43~128자를 요구한다.
    expect(a.length).toBeGreaterThanOrEqual(43);
  });

  it("로그인 URL 이 code 플로우와 S256 을 요구한다", async () => {
    const req = await buildAuthorizeRequest(config, "https://dbmon.example.com");
    expect(req).not.toBeNull();
    const url = new URL(req!.url);
    expect(url.searchParams.get("response_type")).toBe("code");
    expect(url.searchParams.get("code_challenge_method")).toBe("S256");
    expect(url.searchParams.get("client_id")).toBe(config.client_id);
    expect(url.searchParams.get("redirect_uri")).toBe(
      "https://dbmon.example.com/auth/callback",
    );
    expect(url.searchParams.get("scope")).toBe("openid email profile");
    // **challenge 는 verifier 와 일치해야 한다.**
    expect(url.searchParams.get("code_challenge")).toBe(
      await challengeOf(req!.verifier),
    );
    // **verifier 자체는 URL 에 없어야 한다.** 있으면 PKCE 가 무의미하다.
    expect(req!.url).not.toContain(req!.verifier);
  });

  it("설정이 불완전하면 로그인 URL 을 만들지 않는다", async () => {
    expect(await buildAuthorizeRequest({ ...config, client_id: "" })).toBeNull();
    expect(await buildAuthorizeRequest({ ...config, domain: "" })).toBeNull();
  });
});

describe("state 대조 (CSRF)", () => {
  /**
   * **저장된 state 가 없으면 거부한다.** 우리가 시작하지 않은 로그인이고,
   * 통과시키면 공격자의 인가 코드로 세션이 바뀐다(세션 고정).
   */
  it("저장된 값이 없으면 거부한다", () => {
    expect(stateMatches("anything")).toBe(false);
  });

  it("값이 다르면 거부하고 같으면 통과한다", () => {
    sessionStorage.setItem("dbmon.pkce.state", "expected-state");
    expect(stateMatches("other")).toBe(false);
    expect(stateMatches(null)).toBe(false);
    expect(stateMatches("")).toBe(false);
    expect(stateMatches("expected-state")).toBe(true);
  });
});

describe("콜백 파싱", () => {
  it("코드와 state 를 읽는다", () => {
    const p = readCallback("?code=abc123&state=xyz");
    expect(p.code).toBe("abc123");
    expect(p.state).toBe("xyz");
    expect(p.error).toBeNull();
  });

  it("오류 응답을 읽는다", () => {
    const p = readCallback("?error=access_denied&error_description=User+cancelled");
    expect(p.error).toBe("access_denied");
    expect(p.errorDescription).toBe("User cancelled");
    expect(p.code).toBeNull();
  });
});

describe("토큰 교환 본문", () => {
  /** 퍼블릭 클라이언트라 **시크릿이 없다.** */
  it("교환 본문에 client_secret 이 없다", () => {
    const body = tokenExchangeBody(config, "code123", "verifier123", "https://x.example");
    expect(body.get("grant_type")).toBe("authorization_code");
    expect(body.get("code_verifier")).toBe("verifier123");
    expect(body.get("redirect_uri")).toBe("https://x.example/auth/callback");
    expect(body.get("client_secret")).toBeNull();
  });

  it("갱신 본문은 refresh_token 그랜트다", () => {
    const body = refreshBody(config, "refresh123");
    expect(body.get("grant_type")).toBe("refresh_token");
    expect(body.get("refresh_token")).toBe("refresh123");
    expect(body.get("client_secret")).toBeNull();
  });
});

describe("만료와 갱신", () => {
  it("만료 시각은 지금 + expires_in 초다", () => {
    expect(expiryOf(3600, 1_000_000)).toBe(1_000_000 + 3_600_000);
    // 음수는 지금으로 접는다.
    expect(expiryOf(-5, 1_000_000)).toBe(1_000_000);
  });

  it("여유 시간 안에 들면 갱신이 필요하다", () => {
    const now = 1_000_000;
    sessionStorage.setItem("dbmon.cognito.expires", String(now + REFRESH_MARGIN_MS + 1));
    expect(needsRefresh(now)).toBe(false);
    sessionStorage.setItem("dbmon.cognito.expires", String(now + REFRESH_MARGIN_MS));
    expect(needsRefresh(now)).toBe(true);
    // 이미 만료됐어도 참이다.
    sessionStorage.setItem("dbmon.cognito.expires", String(now - 1));
    expect(needsRefresh(now)).toBe(true);
  });

  it("만료 정보가 없거나 깨졌으면 갱신하지 않는다", () => {
    expect(needsRefresh(1_000_000)).toBe(false);
    sessionStorage.setItem("dbmon.cognito.expires", "nonsense");
    expect(needsRefresh(1_000_000)).toBe(false);
  });
});

describe("세션 정리", () => {
  /** **액세스 토큰까지 지운다** — 리프레시만 지우면 낡은 토큰으로 401 이 반복된다. */
  it("모든 흔적을 지운다", () => {
    sessionStorage.setItem("dbmon.token", "access");
    sessionStorage.setItem("dbmon.cognito.refresh", "refresh");
    sessionStorage.setItem("dbmon.cognito.expires", "123");
    sessionStorage.setItem("dbmon.pkce.verifier", "v");
    sessionStorage.setItem("dbmon.pkce.state", "s");
    clearSession();
    expect(sessionStorage.length).toBe(0);
  });
});

describe("로그아웃", () => {
  /** Hosted UI 세션도 끊어야 한다 — 안 끊으면 "로그아웃" 뒤 재로그인이 즉시 통과한다. */
  it("Hosted UI 로그아웃 URL 을 만든다", () => {
    const url = logoutUrl(config, "https://dbmon.example.com");
    expect(url).toContain("/logout?");
    expect(url).toContain(`client_id=${config.client_id}`);
    expect(url).toContain("logout_uri=https%3A%2F%2Fdbmon.example.com%2F");
  });

  it("도메인이 없으면 만들지 않는다", () => {
    expect(logoutUrl({ ...config, domain: "" })).toBeNull();
  });
});

describe("콜백 주소", () => {
  it("오리진 기준이다 — Terraform 의 callback_urls 와 같아야 한다", () => {
    expect(callbackUrl("https://dbmon.example.com")).toBe(
      "https://dbmon.example.com/auth/callback",
    );
    expect(callbackUrl("http://localhost:5173")).toBe(
      "http://localhost:5173/auth/callback",
    );
  });
});

describe("갱신 실패 처리", () => {
  /**
   * **일시적 실패로 세션을 버리지 않는다** (교차 리뷰 4차가 잡은 결함).
   *
   * 만료 5분 전 DNS 오류나 5xx 가 한 번 나면, 아직 유효한 액세스 토큰과 8시간짜리
   * 리프레시 토큰까지 지워져 사용자가 로그아웃됐다.
   */
  it("확실한 인증 실패만 세션을 버린다", () => {
    expect(isDefinitiveAuthFailure(400)).toBe(true);
    expect(isDefinitiveAuthFailure(401)).toBe(true);
    for (const transient of [403, 404, 429, 500, 502, 503, 504]) {
      expect(isDefinitiveAuthFailure(transient)).toBe(false);
    }
  });

  it("네트워크 실패는 세션을 유지한다", async () => {
    sessionStorage.setItem("dbmon.token", "still-valid");
    sessionStorage.setItem("dbmon.cognito.refresh", "refresh-token");
    const original = globalThis.fetch;
    globalThis.fetch = (() => Promise.reject(new TypeError("network down"))) as typeof fetch;
    try {
      const result = await refreshAccessToken(config);
      expect(result).toBeNull();
      // **세션이 남아 있어야 한다.**
      expect(sessionStorage.getItem("dbmon.token")).toBe("still-valid");
      expect(sessionStorage.getItem("dbmon.cognito.refresh")).toBe("refresh-token");
    } finally {
      globalThis.fetch = original;
    }
  });

  it("400 은 세션을 버린다 — 리프레시 토큰이 무효다", async () => {
    sessionStorage.setItem("dbmon.token", "stale");
    sessionStorage.setItem("dbmon.cognito.refresh", "revoked-token");
    const original = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(new Response("{}", { status: 400 }))) as typeof fetch;
    try {
      expect(await refreshAccessToken(config)).toBeNull();
      expect(sessionStorage.length).toBe(0);
    } finally {
      globalThis.fetch = original;
    }
  });

  it("5xx 는 세션을 유지한다", async () => {
    sessionStorage.setItem("dbmon.token", "still-valid");
    sessionStorage.setItem("dbmon.cognito.refresh", "refresh-token");
    const original = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(new Response("{}", { status: 503 }))) as typeof fetch;
    try {
      expect(await refreshAccessToken(config)).toBeNull();
      expect(sessionStorage.getItem("dbmon.cognito.refresh")).toBe("refresh-token");
    } finally {
      globalThis.fetch = original;
    }
  });
});
