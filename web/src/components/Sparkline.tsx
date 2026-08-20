import { EMPTY } from "../lib/format";

interface SparklineProps {
  /** `null` 은 "그 시점에 비율을 못 냈다" 다. **0 으로 그리지 않는다.** */
  values: readonly (number | null)[];
  /** 스크린 리더용. 차트는 그림이라 이 문장이 유일한 설명이다. */
  label: string;
  width?: number;
  height?: number;
}

/**
 * 최소 스파크라인.
 *
 * 차트 라이브러리를 쓰지 않는 이유: 축·범례·툴팁이 없는 24px 선 하나에
 * uPlot(약 40KB)을 링크할 이유가 없다. 축이 필요해지면 그때 바꾼다.
 *
 * **없는 구간은 잇지 않는다.** `null` 을 건너뛰고 이으면 관측되지 않은 구간에
 * 선을 그리게 되고, 그건 없는 데이터를 그린 것이다.
 */
export function Sparkline({ values, label, width = 96, height = 24 }: SparklineProps) {
  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  for (const v of values) {
    if (v === null) continue;
    if (v < min) min = v;
    if (v > max) max = v;
  }
  const flat = max === min;
  // 평평한 계열은 0 으로 나눌 수 없다. 가운데 높이에 그린다 — 바닥에 그리면
  // "값이 최저" 로 읽힌다.
  const span = flat ? 1 : max - min;
  const stepX = values.length > 1 ? width / (values.length - 1) : width;

  const segments: string[] = [];
  let current: string[] = [];
  values.forEach((v, i) => {
    if (v === null) {
      if (current.length > 1) segments.push(current.join(" "));
      current = [];
      return;
    }
    const x = i * stepX;
    const y = flat ? height / 2 : height - ((v - min) / span) * height;
    current.push(`${x.toFixed(1)},${y.toFixed(1)}`);
  });
  if (current.length > 1) segments.push(current.join(" "));

  // **표본이 있어도 이어진 구간이 없으면 선을 그릴 수 없다** (`[1, null, 2]`).
  // 빈 `<svg>` 를 내면 칸이 비어 "값이 없다" 와 구분되지 않으므로 표시로 낸다.
  if (segments.length === 0) {
    return <span className="text-zinc-400">{EMPTY}</span>;
  }

  return (
    <svg
      role="img"
      aria-label={label}
      viewBox={`0 0 ${width} ${height}`}
      width={width}
      height={height}
      className="overflow-visible"
      preserveAspectRatio="none"
    >
      {segments.map((points) => (
        <polyline
          key={points}
          points={points}
          fill="none"
          stroke="currentColor"
          strokeWidth={1.5}
          strokeLinejoin="round"
          className="text-sky-400"
          vectorEffect="non-scaling-stroke"
        />
      ))}
    </svg>
  );
}
