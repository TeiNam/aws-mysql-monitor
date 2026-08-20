import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Sparkline } from "./Sparkline";

describe("스파크라인", () => {
  it("관측되지 않은 구간을 잇지 않는다", () => {
    // `null` 을 건너뛰고 한 선으로 이으면 없는 데이터를 그린 것이 된다.
    const { container } = render(
      <Sparkline values={[1, 2, null, 3, 4]} label="테스트 추이" />,
    );
    expect(container.querySelectorAll("polyline")).toHaveLength(2);
  });

  it("표본이 하나뿐이면 선을 그리지 않는다", () => {
    const { container } = render(<Sparkline values={[null, 5]} label="테스트 추이" />);
    expect(container.querySelector("svg")).toBeNull();
  });

  it("모든 값이 같으면 바닥이 아니라 가운데에 그린다", () => {
    // 바닥에 그리면 "값이 최저" 로 읽힌다. 0 으로 나누지도 않아야 한다.
    const { container } = render(
      <Sparkline values={[7, 7, 7]} label="평평한 추이" height={24} />,
    );
    const points = container.querySelector("polyline")?.getAttribute("points") ?? "";
    expect(points).not.toContain("NaN");
    for (const point of points.split(" ")) {
      expect(point.split(",")[1]).toBe("12.0");
    }
  });

  it("표본이 있어도 이어진 구간이 없으면 값 없음 표시를 낸다", () => {
    // `[1, null, 2]` 는 표본이 둘이지만 선을 그릴 수 없다. 빈 `<svg>` 를 내면
    // 칸이 비어 "값이 없다" 와 구분되지 않는다.
    const { container } = render(<Sparkline values={[1, null, 2]} label="구멍뿐" />);
    expect(container.querySelector("svg")).toBeNull();
    expect(container.textContent).toBe("—");
  });
});
