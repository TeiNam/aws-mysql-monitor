import { describe, expect, it } from "vitest";
import { formatMarkdown, formatSql } from "./sql";

/**
 * 내려받는 마크다운은 **읽으려고** 받는 파일이다. 서버는 저장된 원문(한 줄 SQL,
 * 압축 JSON)을 주므로 경계에서 편다. 여기서 고정하는 것은 두 가지다:
 * 편다는 것과, **못 펴면 원문을 잃지 않는다**는 것.
 */
describe("마크다운 코드 블록 포맷", () => {
  it("SQL 블록을 여러 줄로 편다", () => {
    const md = "## SQL\n\n```sql\nSELECT id, name FROM orders WHERE status = ? ORDER BY id\n```\n";
    const out = formatMarkdown(md);
    expect(out).toContain("```sql");
    expect(out.split("\n").length).toBeGreaterThan(md.split("\n").length);
    expect(out).toMatch(/SELECT/);
    // 펜스는 유지된다 — 깨지면 마크다운 전체가 코드 블록으로 보인다.
    expect(out.match(/```/g)?.length).toBe(2);
  });

  it("압축 JSON 을 들여쓴다", () => {
    const md = '```json\n{"query_block":{"select_id":1,"cost":"3.5"}}\n```';
    const out = formatMarkdown(md);
    expect(out).toContain('"select_id": 1');
    expect(out).toContain("\n  ");
  });

  /** 마스킹·절단된 SQL 은 파서를 통과하지 못할 수 있다. 그때 파일이 비면 안 된다. */
  it("파싱할 수 없는 JSON 은 원문을 유지한다", () => {
    const md = "```json\n{절단됨…\n```";
    expect(formatMarkdown(md)).toBe(md);
  });

  /** 계획 트리는 서버가 이미 정렬했다 — 건드리면 정렬이 깨진다. */
  it("text 블록은 건드리지 않는다", () => {
    const md = "```text\n-> Table scan on orders  (cost=1.2 rows=60000)\n```";
    expect(formatMarkdown(md)).toBe(md);
  });

  it("블록이 여러 개면 각각 편다", () => {
    const md =
      "```sql\nSELECT a FROM t WHERE b = ?\n```\n\n```json\n{\"a\":[1,2]}\n```\n";
    const out = formatMarkdown(md);
    expect(out.match(/```/g)?.length).toBe(4);
    expect(out).toContain('"a": [');
  });
});

describe("마스킹된 SQL 포맷", () => {
  /**
   * **실측**: 리터럴 정책 `masked` 가 만드는 `sleep ( ... )` 이 `sql-formatter` 를
   * `Parse error at token: .` 로 죽였다. 기본 정책이 masked 이므로, 이걸 다루지 않으면
   * 화면과 다운로드 파일의 SQL 이 **전부** 한 줄로 남는다.
   */
  it("마스킹 표식이 있어도 여러 줄로 편다", () => {
    const masked =
      "SELECT o . status , count ( * ) c FROM orders o JOIN order_items i ON i . order_id = o . id WHERE o . memo IS NULL AND sleep ( ... ) = ? GROUP BY ?";
    const out = formatSql(masked);
    expect(out.split("\n").length).toBeGreaterThan(5);
    // 표식은 되돌아와야 한다 — 치환 토큰이 파일에 남으면 그게 SQL 인 줄 안다.
    expect(out).toContain("...");
    expect(out).not.toContain("__dbmon_masked__");
  });

  it("정말 못 펴는 SQL 은 원문을 유지한다", () => {
    const broken = "SELECT ((( FROM";
    expect(formatSql(broken)).toBe(broken);
  });
});
