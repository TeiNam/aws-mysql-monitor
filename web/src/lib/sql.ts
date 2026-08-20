import { format } from "sql-formatter";

/**
 * SQL 정형화. 참조 대시보드와 같이 `sql-formatter` 를 쓴다.
 *
 * **실패하면 원문을 그대로 돌려준다.** 마스킹된 SQL(`SELECT ? FROM …`)이나 잘린
 * SQL 은 파서를 통과하지 못할 수 있고, 그때 예외를 올리면 팝업이 안 열린다 —
 * 읽기 어려운 SQL 이 안 보이는 SQL 보다 낫다.
 */
export function formatSql(sql: string): string {
  try {
    return format(sql, { language: "mysql", tabWidth: 2, keywordCase: "upper" });
  } catch {
    return sql;
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
