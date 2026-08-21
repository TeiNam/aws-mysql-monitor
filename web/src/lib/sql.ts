import { format } from "sql-formatter";

/**
 * SQL 정형화. 참조 대시보드와 같이 `sql-formatter` 를 쓴다.
 *
 * **실패하면 원문을 그대로 돌려준다.** 마스킹된 SQL(`SELECT ? FROM …`)이나 잘린
 * SQL 은 파서를 통과하지 못할 수 있고, 그때 예외를 올리면 팝업이 안 열린다 —
 * 읽기 어려운 SQL 이 안 보이는 SQL 보다 낫다.
 */
export function formatSql(sql: string): string {
  // **마스킹 표식(`...`)이 파서를 막는다.**
  //
  // 리터럴 정책이 `masked` 면 저장된 SQL 은 `sleep ( ... ) = ?` 처럼 생겼고,
  // `sql-formatter` 는 `...` 에서 `Parse error at token: .` 로 죽는다(실측). 그러면
  // catch 가 원문을 돌려주므로 **마스킹된 SQL 은 영원히 한 줄로 보인다** — 기본 정책이
  // `masked` 이니 사실상 전부다.
  //
  // 그래서 식별자로 파싱되는 토큰으로 바꿔 두고 포맷한 뒤 되돌린다. 실측: 치환하면
  // 같은 쿼리가 1줄 → 11줄로 펴진다.
  const prepared = sql.replace(ELLIPSIS_RE, MASK_TOKEN);
  try {
    const out = format(prepared, { language: "mysql", tabWidth: 2, keywordCase: "upper" });
    return out.split(MASK_TOKEN).join("...");
  } catch {
    return sql;
  }
}

/** 마스킹 표식 자리에 넣는 **식별자로 파싱되는** 토큰. 실제 SQL 에 나올 수 없는 이름이다. */
const MASK_TOKEN = "__dbmon_masked__";
const ELLIPSIS_RE = /\.\.\./g;

/**
 * 마크다운의 코드 블록을 **읽을 수 있게 편다.**
 *
 * # 왜 서버가 아니라 여기서 하는가
 *
 * `GET /api/queries/{id}/markdown` 은 **저장된 그대로** 내보낸다 — SQL 은 한 줄이고
 * 플랜 JSON 은 압축돼 있다. 저장 형태를 바꾸면 안 되고(멱등 키·병합이 원문을 쓴다),
 * Rust 에 SQL 포매터를 넣는 것은 이 화면이 이미 가진 `sql-formatter` 를 두 번 만드는
 * 일이다. 내려받는 사람이 원하는 것은 **읽기 좋은 파일**이므로 경계에서 편다.
 *
 * ` ```text ` 블록(계획 트리)은 건드리지 않는다 — 그건 이미 서버가 정렬한 결과다.
 */
export function formatMarkdown(md: string): string {
  return md.replace(/```(sql|json)\n([\s\S]*?)```/g, (whole, lang: string, body: string) => {
    const pretty = lang === "sql" ? formatSql(body.trim()) : formatJson(body.trim());
    // 편지 못했으면 원문을 그대로 둔다 — 깨진 파일보다 읽기 어려운 파일이 낫다.
    return pretty === null ? whole : `\`\`\`${lang}\n${pretty}\n\`\`\``;
  });
}

/** 압축 JSON 을 들여쓴다. **파싱 실패하면 `null`** — 원문을 살리라는 신호다. */
function formatJson(text: string): string | null {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return null;
  }
}

/** 텍스트를 파일로 내려준다. 마크다운 다운로드에 쓴다. */
export function downloadText(filename: string, text: string, mime = "text/markdown"): void {
  const url = URL.createObjectURL(new Blob([text], { type: `${mime};charset=utf-8` }));
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  // **URL 을 회수한다.** 안 하면 탭이 살아 있는 동안 블롭이 메모리에 남는다.
  URL.revokeObjectURL(url);
}
