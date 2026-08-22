/**
 * 접속 토큰 관리.
 *
 * # 세 가지 모드
 *
 * | 모드 | 토큰 | 언제 |
 * |---|---|---|
 * | `local-dev` | 없음 | 호스트에서 루프백 바인드로 `cargo run` |
 * | `local-token` | 기동 로그의 URL | 컨테이너 (루프백 바인드가 불가능) |
 * | `shared-token` | 배포 설정의 `http.auth_token` | ECS 등 실제 배포 |
 * | `unconfigured` | **없다** | 토큰도 없고 인증도 안 껐다 — 전부 401 |
 * | `cognito` | OIDC 액세스 토큰 | 배포. **아직 미구현 — 백엔드가 fail closed** |
 *
 * # 토큰을 URL 에서 즉시 지운다
 *
 * `?token=…` 을 그대로 두면 주소창·브라우저 히스토리·`Referer` 헤더에 남는다.
 * 읽는 즉시 `sessionStorage` 로 옮기고 `history.replaceState` 로 지운다.
 * `localStorage` 를 쓰지 않는 이유: 탭을 닫으면 사라지는 편이 맞다.
 */

const STORAGE_KEY = "dbmon.token";
const TOKEN_PARAM = "token";

/**
 * URL 또는 세션에서 토큰을 꺼낸다. **URL 쪽이 우선**이다 — 새 토큰을 들고 오면
 * 낡은 세션 값을 덮어야 한다(컨테이너 재시작 시 토큰이 바뀐다).
 */
export function takeToken(): string | null {
  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get(TOKEN_PARAM);

  if (fromUrl) {
    sessionStorage.setItem(STORAGE_KEY, fromUrl);
    url.searchParams.delete(TOKEN_PARAM);
    // 나머지 쿼리스트링(필터 등)은 보존한다 — URL 이 상태를 담기 때문이다(FR-UI-19).
    window.history.replaceState(null, "", url.pathname + url.search + url.hash);
    return fromUrl;
  }
  return sessionStorage.getItem(STORAGE_KEY);
}

/** 저장된 토큰. 없으면 `null`. */
export function currentToken(): string | null {
  return sessionStorage.getItem(STORAGE_KEY);
}

/**
 * 토큰을 직접 넣는다 (화면의 붙여넣기 칸).
 *
 * # 왜 URL 대신 이 경로가 필요한가
 *
 * `?token=…` 은 **첫 요청이 나간 뒤에** 지워진다. `history.replaceState` 는 브라우저
 * 주소창만 고치고, 그 사이 ALB·리버스 프록시의 액세스 로그에는 요청 대상이 그대로
 * 남는다 — 공유 토큰은 admin 이고 만료도 없으므로 그 로그가 곧 자격증명이다.
 *
 * 붙여넣기는 요청을 만들지 않는다. 그래서 배포에서는 이쪽을 안내하고, `?token=`
 * 경로는 로그가 로컬에만 남는 dev 컨테이너용으로 남겨 둔다.
 */
export function saveToken(token: string): void {
  sessionStorage.setItem(STORAGE_KEY, token.trim());
}

/** 토큰을 버린다. 401 을 받았을 때 부른다 — 낡은 토큰으로 계속 재시도하지 않는다. */
export function clearToken(): void {
  sessionStorage.removeItem(STORAGE_KEY);
}

/** `Authorization` 헤더. 토큰이 없으면 빈 객체 — 로컬 우회 모드다. */
export function authHeaders(): Record<string, string> {
  const token = currentToken();
  return token ? { authorization: `Bearer ${token}` } : {};
}
