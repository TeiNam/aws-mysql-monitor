/**
 * 실행계획 **표(Simple)** 뷰 — 참조 도구(DataGrip)의 `Simple` 탭과 같은 열 구성.
 *
 * | 열 | 출처 |
 * |---|---|
 * | Node Type | 노드 종류 + 접근 방식 (`orders (ALL)`, `Nested loop (2)`) |
 * | Entity | 대상 테이블·CTE |
 * | Cost | v1 `cost_info`, v2 `estimated_total_cost` |
 * | Rows | 예상 행 수(v2 는 실측이 있으면 그쪽) |
 * | Time | **`EXPLAIN ANALYZE`(v2) 에만 있다** — 없으면 `—` |
 * | Condition | `attached_condition`·`index_condition`·`using filesort` 같은 신호 |
 *
 * # 그래프와 무엇이 다른가
 *
 * 그래프는 **모양**(어디서 조인이 터지나)을 보여주고 표는 **숫자**(무엇이 제일 비싼가)를
 * 보여준다. 같은 파싱 결과를 두 방식으로 그리므로 둘이 어긋날 수 없다.
 *
 * # Time 을 추정치로 채우지 않는다
 *
 * MySQL 의 `EXPLAIN FORMAT=JSON` 은 시간을 주지 않는다. 비용으로 시간을 추정해 넣으면
 * 운영자가 "이 노드가 2.5초 걸렸다" 로 읽는다 — 그건 측정값이 아니다. 없으면 비운다.
 */

import { ChevronDown, ChevronRight } from "lucide-react";
import { useMemo, useState } from "react";
import { NODE_COLOR, parsePlan } from "../lib/plan-graph";
import { costShare, planRows } from "../lib/plan-rows";
import { CELL_X, COL_GROW, COL_TIGHT, TABLE, TBODY, TH, TH_NUM, TR } from "./ui";

const INT = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 0 });
const COST = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 2 });

/** 들여쓰기 한 단계의 폭. 레퍼런스와 비슷한 밀도. */
const INDENT_PX = 16;

export function PlanTable({ json }: { json: string }) {
  const graph = useMemo(() => parsePlan(json), [json]);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());

  const rows = useMemo(
    () => (graph === null ? [] : planRows(graph, collapsed)),
    [graph, collapsed],
  );

  if (graph === null) {
    return (
      <p className="rounded-md bg-amber-50 p-3 text-sm text-amber-800">
        계획 JSON 을 읽지 못했다. 아래 원문을 확인한다 — 형식이 예상과 다르다.
      </p>
    );
  }

  function toggle(id: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  // 시간이 하나도 없으면 그 열은 의미가 없다 — `EXPLAIN ANALYZE` 가 아닌 계획이다.
  const hasTime = rows.some((r) => r.node.timeMs !== undefined);

  return (
    <div className="overflow-x-auto rounded-lg border border-gray-200">
      <table className={TABLE}>
        <thead>
          <tr>
            <th className={`${TH} ${COL_TIGHT}`}>Node Type</th>
            <th className={`${TH} ${COL_TIGHT}`}>Entity</th>
            <th className={`${TH_NUM} ${COL_TIGHT}`}>Cost</th>
            <th className={`${TH_NUM} ${COL_TIGHT}`}>Rows</th>
            {hasTime ? <th className={`${TH_NUM} ${COL_TIGHT}`}>Time</th> : null}
            <th className={`${TH} ${COL_GROW}`}>Condition</th>
          </tr>
        </thead>
        <tbody className={TBODY}>
          {rows.map(({ node, indent, hasChildren, collapsed: isCollapsed }) => {
            const share = costShare(rows, node);
            return (
              <tr key={node.id} className={TR}>
                <td className={`${CELL_X} ${COL_TIGHT} py-1.5 text-sm text-gray-800`}>
                  <span
                    className="flex items-center gap-1"
                    style={{ paddingLeft: `${indent * INDENT_PX}px` }}
                  >
                    {hasChildren ? (
                      <button
                        type="button"
                        onClick={() => toggle(node.id)}
                        className="text-gray-400 hover:text-gray-700"
                        aria-label={isCollapsed ? "펼치기" : "접기"}
                        aria-expanded={!isCollapsed}
                      >
                        {isCollapsed ? (
                          <ChevronRight className="h-3.5 w-3.5" />
                        ) : (
                          <ChevronDown className="h-3.5 w-3.5" />
                        )}
                      </button>
                    ) : (
                      // 자식이 없어도 자리를 비워 열이 어긋나지 않게 한다.
                      <span className="w-3.5" />
                    )}
                    {/* 그래프와 **같은 색**을 쓴다 — 두 탭을 번갈아 봐도 같은 노드로 읽힌다. */}
                    <span
                      className="h-2 w-2 shrink-0 rounded-full"
                      style={{ backgroundColor: NODE_COLOR[node.kind] }}
                      aria-hidden="true"
                    />
                    {node.label}
                  </span>
                </td>
                <td className={`${CELL_X} ${COL_TIGHT} py-1.5 font-mono text-xs text-gray-700`}>
                  {node.entity ?? ""}
                </td>
                <td
                  className={`${CELL_X} ${COL_TIGHT} py-1.5 text-right text-sm tabular-nums text-gray-800`}
                >
                  {node.cost === undefined ? (
                    "—"
                  ) : (
                    <>
                      {COST.format(node.cost)}
                      {share === null ? null : (
                        <span className="ml-1 text-xs text-gray-500">
                          ({share.toFixed(0)}%)
                        </span>
                      )}
                    </>
                  )}
                </td>
                <td
                  className={`${CELL_X} ${COL_TIGHT} py-1.5 text-right text-sm tabular-nums text-gray-800`}
                >
                  {node.rows === undefined ? "—" : INT.format(node.rows)}
                </td>
                {hasTime ? (
                  <td
                    className={`${CELL_X} ${COL_TIGHT} py-1.5 text-right text-sm tabular-nums text-gray-800`}
                  >
                    {node.timeMs === undefined ? "—" : `${COST.format(node.timeMs)}ms`}
                  </td>
                ) : null}
                <td className={`${CELL_X} ${COL_GROW} max-w-0 py-1.5 text-sm text-gray-700`}>
                  <span className="block truncate font-mono text-xs" title={node.condition ?? ""}>
                    {node.condition ?? ""}
                  </span>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
