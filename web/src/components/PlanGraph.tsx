import { useMemo } from "react";
import { NODE_COLOR, layoutPlan, parsePlan, type PlacedNode } from "../lib/plan-graph";

interface PlanGraphProps {
  /** 저장된 플랜 JSON(마스킹됨). */
  json: string;
}

/**
 * 실행계획 그래프.
 *
 * 참조 대시보드는 `<canvas>` 에 그렸다. SVG 로 바꾼 이유:
 * - 확대해도 안 깨진다(플랜은 깊어지면 옆으로 길다)
 * - 텍스트가 DOM 에 남아 **검색·복사·스크린리더**가 된다
 * - 테스트에서 노드 수를 셀 수 있다
 */
export function PlanGraph({ json }: PlanGraphProps) {
  const layout = useMemo(() => {
    const graph = parsePlan(json);
    return graph === null ? null : layoutPlan(graph);
  }, [json]);

  if (layout === null) {
    return (
      <p className="rounded-md bg-amber-50 p-3 text-sm text-amber-800">
        계획 JSON 을 읽지 못했다. 아래 원문을 확인한다 — 형식이 예상과 다르다.
      </p>
    );
  }

  const byId = new Map(layout.nodes.map((n) => [n.id, n]));

  return (
    <div className="overflow-x-auto rounded-lg border border-gray-200 bg-white">
      <svg
        role="img"
        aria-label={`실행계획 노드 ${layout.nodes.length}개`}
        viewBox={`0 0 ${layout.width} ${layout.height}`}
        width={layout.width}
        height={layout.height}
        className="max-w-none"
      >
        {/* 엣지를 먼저 그려 노드 아래로 보낸다. */}
        {layout.edges.map(({ from, to }) => {
          const a = byId.get(from);
          const b = byId.get(to);
          if (a === undefined || b === undefined) return null;
          const x1 = a.x + a.width;
          const y1 = a.y + a.height / 2;
          const x2 = b.x;
          const y2 = b.y + b.height / 2;
          const mid = x1 + (x2 - x1) / 2;
          return (
            <path
              key={`${from}-${to}`}
              d={`M ${x1} ${y1} C ${mid} ${y1}, ${mid} ${y2}, ${x2} ${y2}`}
              fill="none"
              stroke="#93c5fd"
              strokeWidth={2}
            />
          );
        })}
        {layout.nodes.map((node) => (
          <PlanNodeBox key={node.id} node={node} />
        ))}
      </svg>
    </div>
  );
}

function PlanNodeBox({ node }: { node: PlacedNode }) {
  const color = NODE_COLOR[node.kind];
  return (
    <g>
      <rect
        x={node.x}
        y={node.y}
        width={node.width}
        height={node.height}
        rx={4}
        fill="#ffffff"
        stroke={color}
        strokeWidth={2}
      />
      {/* 왼쪽 색 바. 참조 구현과 같은 표시다. */}
      <rect x={node.x} y={node.y} width={5} height={node.height} fill={color} />
      <text
        x={node.x + 12}
        y={node.y + 20}
        fontSize={12}
        fontWeight={700}
        fill="#111827"
        className="font-sans"
      >
        {node.label.length > 26 ? `${node.label.slice(0, 25)}…` : node.label}
      </text>
      {node.details.slice(0, 3).map((line, i) => (
        <text
          key={line}
          x={node.x + 12}
          y={node.y + 38 + i * 14}
          fontSize={11}
          fill="#4b5563"
          className="font-sans"
        >
          {line.length > 28 ? `${line.slice(0, 27)}…` : line}
        </text>
      ))}
      {/* 잘린 라벨의 전문은 툴팁으로 남긴다 — 숨기지 않고 좁힌다. */}
      <title>{[node.label, ...node.details].join("\n")}</title>
    </g>
  );
}
