/**
 * 실행계획 그래프 → **표(Simple) 행**.
 *
 * # 왜 순수 함수인가
 *
 * 트리를 접었다 펴는 표는 "무엇을 그릴까" 와 "어떻게 그릴까" 가 섞이기 쉽다. 접힌
 * 노드의 **자손 전체**를 건너뛰는 규칙이 그리기 코드 안에 들어가면, 손자만 남거나
 * 형제가 사라지는 식으로 조용히 틀린다. 그래서 행 목록을 먼저 만들고 그리기는 표에 맡긴다.
 *
 * # 순서는 전위 순회다
 *
 * DataGrip 의 `Simple` 탭과 같다 — 부모 다음에 그 자식들이 온다. 그래프는 깊이를 x 축에
 * 쓰지만 표는 **들여쓰기**로 같은 정보를 보여준다.
 */

import type { PlanGraph, PlanNode } from "./plan-graph";

export interface PlanRow {
  node: PlanNode;
  /** 들여쓰기 단계. 루트가 0 이다(`node.depth` 와 다를 수 있다 — 아래 참고). */
  indent: number;
  /** 자식이 있나. 없으면 접기 화살표를 그리지 않는다. */
  hasChildren: boolean;
  /** 지금 접혀 있나. */
  collapsed: boolean;
}

/**
 * 표에 그릴 행을 만든다. `collapsed` 에 든 노드의 **자손은 건너뛴다.**
 *
 * `node.depth` 를 그대로 쓰지 않고 순회 깊이를 세는 이유: 깊이는 파서가 원본 JSON 의
 * 중첩을 따라 붙인 값이라 형제 사이에 건너뛴 값이 있을 수 있다. 표는 **보이는 계층**을
 * 보여줘야 하므로 들여쓰기는 순회에서 센다.
 */
export function planRows(graph: PlanGraph, collapsed: ReadonlySet<string>): PlanRow[] {
  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  // 부모가 있는 노드를 모아 루트를 찾는다. 루트가 여럿일 수 있다(union 등).
  const hasParent = new Set(graph.nodes.flatMap((n) => n.children));
  const roots = graph.nodes.filter((n) => !hasParent.has(n.id));

  const out: PlanRow[] = [];
  const seen = new Set<string>();

  const walk = (node: PlanNode, indent: number) => {
    // **같은 노드를 두 번 그리지 않는다.** 파서가 같은 자식을 두 부모에 이으면
    // (형식이 바뀌면 가능하다) 표가 무한히 늘어난다.
    if (seen.has(node.id)) return;
    seen.add(node.id);

    const isCollapsed = collapsed.has(node.id);
    out.push({
      node,
      indent,
      hasChildren: node.children.length > 0,
      collapsed: isCollapsed,
    });
    if (isCollapsed) return;
    for (const childId of node.children) {
      const child = byId.get(childId);
      if (child !== undefined) walk(child, indent + 1);
    }
  };

  for (const root of roots) walk(root, 0);

  // **접힌 것과 도달 불가를 구분한다.** 접힌 자손을 "못 찾은 노드" 로 보고 다시 그리면
  // 접기가 무력화된다(테스트가 이걸 잡았다). 도달 가능성은 접기와 무관하게 따로 센다.
  const reachable = new Set<string>();
  const mark = (id: string) => {
    if (reachable.has(id)) return;
    reachable.add(id);
    for (const child of byId.get(id)?.children ?? []) mark(child);
  };
  for (const root of roots) mark(root.id);

  // 어느 루트에서도 닿지 않는 노드는 그래도 보여준다 — 표에서 조용히 사라지는 것이 최악이다.
  for (const node of graph.nodes) if (!reachable.has(node.id)) walk(node, 0);
  return out;
}

/**
 * 비용 비중(%). 가장 큰 비용을 100% 로 본다.
 *
 * 레퍼런스(DataGrip)는 루트 비용을 100% 로 잡는데, MySQL v1 은 `query_block.query_cost`
 * 가 루트에 있고 테이블의 `prefix_cost` 는 그 이하다 — 그래서 최댓값을 기준으로 삼으면
 * 두 형식(v1·v2) 모두에서 "무엇이 제일 비싼가" 를 같은 방식으로 읽을 수 있다.
 *
 * 비용이 하나도 없으면(`EXPLAIN` 이 비용을 안 준 경우) `null` — 0% 로 채우지 않는다.
 */
export function costShare(rows: readonly PlanRow[], node: PlanNode): number | null {
  if (node.cost === undefined) return null;
  const max = rows.reduce((m, r) => Math.max(m, r.node.cost ?? 0), 0);
  if (max <= 0) return null;
  return (node.cost / max) * 100;
}
