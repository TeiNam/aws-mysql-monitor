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

  it("모든 값이 같아도 0 으로 나누지 않는다", () => {
    const { container } = render(<Sparkline values={[7, 7, 7]} label="평평한 추이" />);
    const points = container.querySelector("polyline")?.getAttribute("points") ?? "";
    expect(points).not.toContain("NaN");
  });
});
