import { describe, expect, it } from "vitest";
import { regionLabel, regionName } from "./regions";

describe("리전 이름", () => {
  it("코드 옆에 이름을 붙인다", () => {
    expect(regionLabel("ap-northeast-2")).toBe("ap-northeast-2 (서울)");
    expect(regionLabel("us-east-1")).toBe("us-east-1 (버지니아 북부)");
  });

  /**
   * **모르는 코드를 지우지 않는다.** AWS 는 리전을 계속 추가하고, 목록에 없다고
   * "알 수 없음" 으로 바꾸면 실재하는 리전이 화면에서 사라진 것처럼 보인다.
   */
  it("모르는 코드는 그대로 남는다", () => {
    expect(regionName("xx-nowhere-9")).toBeNull();
    expect(regionLabel("xx-nowhere-9")).toBe("xx-nowhere-9");
  });

  /** 한 글자 차이인 리전들이 **다르게 읽혀야** 한다 — 이 매핑의 존재 이유다. */
  it("비슷한 코드가 서로 다르게 읽힌다", () => {
    const labels = ["ap-northeast-1", "ap-northeast-2", "ap-northeast-3"].map(regionLabel);
    expect(new Set(labels).size).toBe(3);
    expect(labels).toEqual([
      "ap-northeast-1 (도쿄)",
      "ap-northeast-2 (서울)",
      "ap-northeast-3 (오사카)",
    ]);
  });
});
