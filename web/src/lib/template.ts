/**
 * 알림 문구 미리보기.
 *
 * **자리표시자 목록은 서버(`core::settings::TEMPLATE_PLACEHOLDERS`)와 같아야 한다.**
 * 어긋나면 화면이 "쓸 수 있다" 고 안내한 이름을 서버가 거부한다 — 사용자에게는
 * 도구가 자기 말을 뒤집는 것으로 보인다.
 */
export const PLACEHOLDERS = [
  "emoji",
  "severity",
  "env",
  "instance",
  "title",
  "detail",
  "link",
  "at",
] as const;

/** 미리보기에 쓸 예시 값. 실제 알림과 같은 자리에 같은 종류가 들어간다. */
const SAMPLE: Record<string, string> = {
  emoji: "🔴",
  severity: "critical",
  env: "prd",
  instance: "orders-prd-01",
  title: "슬로우 쿼리 급증",
  detail: "최근 5분간 초당 12건 (임계 3건)",
  link: "https://dbmon.example.com/mysql?instance=orders-prd-01",
  at: "14:21:03 KST",
};

/**
 * 예시 값으로 렌더한다. **모르는 자리표시자는 그대로 남긴다** — 미리보기에서
 * `{instnace}` 가 눈에 보여야 오타를 잡는다.
 */
export function renderTemplate(template: string): string {
  let out = template;
  for (const [key, value] of Object.entries(SAMPLE)) {
    out = out.split(`{${key}}`).join(value);
  }
  return out;
}
