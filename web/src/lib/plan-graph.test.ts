import { describe, expect, it } from "vitest";
import { layoutPlan, parsePlan } from "./plan-graph";
import { costShare, planRows } from "./plan-rows";

/** MySQL 8.4.11 의 `EXPLAIN ANALYZE FORMAT=JSON` 실제 출력(값 그대로, 일부만). */
const V2_ANALYZE = {
  "limit": 3,
  "query": "select `c`.`id`,count(0) from `shop`.`orders` `o` join `shop`.`customers` `c` ...",
  "operation": "Limit: 3 row(s)",
  "actual_rows": 3.0,
  "actual_loops": 1,
  "actual_last_row_ms": 0.2,
  "estimated_total_cost": 4919.65,
  "inputs": [
    {
      "operation": "Nested loop inner join",
      "access_type": "join",
      "actual_rows": 10.0,
      "actual_loops": 1,
      "estimated_rows": 3.02,
      "actual_last_row_ms": 0.046457,
      "estimated_total_cost": 4919.64565465911,
      "inputs": [
        {
          "alias": "c",
          "covering": true,
          "operation": "Covering index scan on c using PRIMARY",
          "index_name": "PRIMARY",
          "table_name": "customers",
          "access_type": "index",
          "actual_rows": 4.0,
          "schema_name": "shop",
          "actual_loops": 1,
          "estimated_rows": 1.0,
          "index_access_type": "index_scan",
          "actual_last_row_ms": 0.020458999999999998,
          "estimated_total_cost": 0.0020586734693877552
        },
        {
          "alias": "o",
          "covering": true,
          "operation": "Covering index lookup on o using idx_orders_customer (customer_id=c.id)",
          "index_name": "idx_orders_customer",
          "table_name": "orders",
          "access_type": "index",
          "actual_rows": 2.5,
          "schema_name": "shop",
          "actual_loops": 4,
          "estimated_rows": 3.024895429611206,
          "lookup_condition": "customer_id=c.id",
          "actual_last_row_ms": 0.00597975,
          "estimated_total_cost": 0.25100222428498165
        }
      ]
    }
  ]
};

/**
 * 참조 대시보드는 노드 좌표를 **라벨 문자열로** 정했다(`case 'Nested_Loop#2'`).
 * 그래서 테이블이 셋 이상이거나 `grouping_operation` 이 끼면 노드가 같은 자리에
 * 겹치거나 아예 사라졌다. 여기서는 구조를 따라가므로 그 모양들을 테스트한다.
 */
describe("실행계획 파싱", () => {
  it("테이블을 쓰지 않는 계획의 message 를 버리지 않는다", () => {
    // 로컬 `SELECT SLEEP(7)` 이 실제로 이 형태로 저장된다.
    const graph = parsePlan('{"query_block":{"select_id":1,"message":"No tables used"}}');
    expect(graph).not.toBeNull();
    expect(graph!.nodes.map((n) => n.label)).toEqual(["Select", "No tables used"]);
  });

  it("nested_loop 의 테이블을 전부 노드로 만든다", () => {
    const json = JSON.stringify({
      query_block: {
        select_id: 1,
        cost_info: { query_cost: "353891.58" },
        ordering_operation: {
          using_filesort: true,
          nested_loop: [
            { table: { table_name: "sg", access_type: "ref", rows_examined_per_scan: 2, filtered: "100.00" } },
            { table: { table_name: "g", access_type: "ALL", rows_examined_per_scan: 3113305, filtered: "0.01" } },
            { table: { table_name: "t3", access_type: "eq_ref", rows_examined_per_scan: 1 } },
          ],
        },
      },
    });
    const graph = parsePlan(json)!;
    const tables = graph.nodes.filter((n) => n.kind === "table");
    // **셋 다 나와야 한다.** 참조 구현은 두 개까지만 그렸다.
    expect(tables.map((t) => t.label)).toEqual(["sg (ref)", "g (ALL)", "t3 (eq_ref)"]);
    expect(graph.nodes.find((n) => n.kind === "operation")?.details).toContain("using filesort");
    expect(graph.nodes[0]?.details).toContain("Cost: 353,891.58");
  });

  it("grouping·union·서브쿼리도 노드로 잇는다", () => {
    const json = JSON.stringify({
      query_block: {
        select_id: 1,
        grouping_operation: {
          using_temporary_table: true,
          table: {
            table_name: "derived",
            access_type: "ALL",
            materialized_from_subquery: { query_block: { select_id: 2, message: "Impossible WHERE" } },
          },
        },
        union_result: {
          query_specifications: [{ query_block: { select_id: 3, message: "No tables used" } }],
        },
      },
    });
    const graph = parsePlan(json)!;
    const kinds = graph.nodes.map((n) => n.kind);
    expect(kinds).toContain("operation");
    expect(kinds).toContain("table");
    expect(kinds).toContain("subquery");
    // 서브쿼리 안의 블록도 살아 있어야 한다.
    expect(graph.nodes.filter((n) => n.kind === "select")).toHaveLength(3);
  });

  /**
   * v2(`explain_json_format_version=2`, MySQL 8.3+) — **8.4.11 실측 출력**이다.
   *
   * 첫 판은 루트를 `{ query_plan: … }` 로, 시간을 `actual_total_time_ms` 로 가정했는데
   * 둘 다 틀렸다. 진짜 루트는 계획 노드 자신이고 시간은 `actual_last_row_ms` 다.
   * 가정으로 쓴 테스트는 통과하면서 **실제 계획을 노드 하나로 그렸다.**
   */
  it("v2 (EXPLAIN ANALYZE FORMAT=JSON) 실측 출력을 읽는다", () => {
    const json = JSON.stringify(V2_ANALYZE);
    const graph = parsePlan(json)!;
    expect(graph.nodes.map((n) => n.label)).toEqual([
      "Limit: 3 row(s)",
      "Nested loop inner join",
      "Covering index scan on c using PRIMARY",
      "Covering index lookup on o using idx_orders_customer (customer_id=c.id)",
    ]);
    // 테이블 접근은 테이블 색, 연산은 연산 색.
    expect(graph.nodes.filter((n) => n.kind === "table")).toHaveLength(2);

    // **루프를 곱한다.** `o` 는 4번 돌며 매번 2.5행 → 10행이다. 곱하지 않으면
    // "2.5행" 으로 보이고, 그 노드가 싼 줄로 읽힌다.
    const o = graph.nodes.find((n) => n.entity?.startsWith("orders"))!;
    expect(o.rows).toBe(10);
    expect(o.timeMs).toBeCloseTo(0.00597975 * 4);
    // 별칭을 함께 보여준다 — 같은 테이블이 두 번 나오면 별칭만이 구분이다.
    expect(o.entity).toBe("orders (o)");
    expect(o.key).toBe("idx_orders_customer");
    expect(o.condition).toBe("customer_id=c.id");
  });

  it("v2 에 실측이 없으면(ANALYZE 아님) 시간은 비어 있다", () => {
    const noAnalyze = {
      operation: "Table scan on orders",
      table_name: "orders",
      access_type: "table",
      estimated_rows: 60023,
      estimated_total_cost: 6074.55,
    };
    const graph = parsePlan(JSON.stringify(noAnalyze))!;
    expect(graph.nodes).toHaveLength(1);
    expect(graph.nodes[0]!.timeMs).toBeUndefined();
    expect(graph.nodes[0]!.rows).toBe(60023);
    expect(graph.nodes[0]!.cost).toBeCloseTo(6074.55);
  });

  it("깨진 JSON 은 null 이다 — 빈 그래프와 구분된다", () => {
    expect(parsePlan("not json")).toBeNull();
    expect(parsePlan("[]")).toBeNull();
    // 빈 객체는 "읽었지만 내용이 없다" 이므로 Select 노드 하나가 나온다.
    expect(parsePlan("{}")?.nodes).toHaveLength(1);
  });

  it("모르는 키는 무시하고 아는 것만 그린다", () => {
    const graph = parsePlan(
      '{"query_block":{"select_id":1,"future_operation":{"x":1},"table":{"table_name":"t"}}}',
    )!;
    expect(graph.nodes).toHaveLength(2);
  });
});

describe("배치", () => {
  it("깊이가 x, 형제가 y — 노드가 겹치지 않는다", () => {
    const json = JSON.stringify({
      query_block: {
        nested_loop: [
          { table: { table_name: "a" } },
          { table: { table_name: "b" } },
          { table: { table_name: "c" } },
        ],
      },
    });
    const layout = layoutPlan(parsePlan(json)!);
    const tables = layout.nodes.filter((n) => n.kind === "table");
    // 같은 깊이면 x 가 같고 y 는 달라야 한다.
    expect(new Set(tables.map((t) => t.x)).size).toBe(1);
    expect(new Set(tables.map((t) => t.y)).size).toBe(3);
    // 캔버스는 노드를 담을 만큼 커야 한다.
    expect(layout.height).toBeGreaterThan(tables[2]!.y);
  });

  it("부모는 자식들의 가운데에 온다", () => {
    const json = JSON.stringify({
      query_block: { nested_loop: [{ table: { table_name: "a" } }, { table: { table_name: "b" } }] },
    });
    const layout = layoutPlan(parsePlan(json)!);
    const loop = layout.nodes.find((n) => n.kind === "loop")!;
    const tables = layout.nodes.filter((n) => n.kind === "table");
    expect(loop.y).toBeCloseTo((tables[0]!.y + tables[1]!.y) / 2);
  });
});

describe("표(Simple) 행", () => {
  it("전위 순회로 펼치고 들여쓰기는 순회 깊이다", () => {
    const json = JSON.stringify({
      query_block: {
        select_id: 1,
        cost_info: { query_cost: "353891.58" },
        nested_loop: [
          {
            table: {
              table_name: "sg",
              access_type: "ref",
              rows_examined_per_scan: 2,
              attached_condition: "(sg.id = 7)",
              cost_info: { prefix_cost: "1.20" },
            },
          },
          { table: { table_name: "g", access_type: "ALL", rows_examined_per_scan: 3113305 } },
        ],
      },
    });
    const rows = planRows(parsePlan(json)!, new Set());
    expect(rows.map((r) => `${"  ".repeat(r.indent)}${r.node.label}`)).toEqual([
      "Select",
      "  Nested loop (2)",
      "    sg (ref)",
      "    g (ALL)",
    ]);
    // 표가 쓰는 구조화된 값이 채워져 있어야 한다.
    const sg = rows[2]!.node;
    expect(sg.entity).toBe("sg");
    expect(sg.rows).toBe(2);
    expect(sg.cost).toBeCloseTo(1.2);
    expect(sg.condition).toBe("(sg.id = 7)");
    // **시간은 없다** — `EXPLAIN FORMAT=JSON` 은 시간을 주지 않는다. 추정으로 채우지 않는다.
    expect(sg.timeMs).toBeUndefined();
  });

  it("조인 단계 수를 행 수로 내지 않는다", () => {
    // `Nested loop (3)` 의 3 은 **자식 수**다. 표의 `Rows` 열에 넣으면 3행짜리 조인으로
    // 읽힌다 — 화면에서 그렇게 나오는 것을 보고 잡았다.
    const json = JSON.stringify({
      query_block: {
        nested_loop: [
          { table: { table_name: "a", rows_examined_per_scan: 60023 } },
          { table: { table_name: "b", rows_examined_per_scan: 2 } },
          { table: { table_name: "c" } },
        ],
      },
    });
    const rows = planRows(parsePlan(json)!, new Set());
    const loop = rows.find((r) => r.node.kind === "loop")!;
    expect(loop.node.label).toBe("Nested loop (3)");
    expect(loop.node.rows).toBeUndefined();
    // 테이블의 행 수는 그대로 온다.
    expect(rows.find((r) => r.node.entity === "a")!.node.rows).toBe(60023);
  });

  it("접힌 노드의 **자손 전체**를 건너뛴다", () => {
    const json = JSON.stringify({
      query_block: {
        nested_loop: [{ table: { table_name: "a" } }, { table: { table_name: "b" } }],
      },
    });
    const graph = parsePlan(json)!;
    const loop = graph.nodes.find((n) => n.kind === "loop")!;
    const rows = planRows(graph, new Set([loop.id]));
    expect(rows.map((r) => r.node.label)).toEqual(["Select", "Nested loop (2)"]);
    expect(rows[1]!.hasChildren).toBe(true);
    expect(rows[1]!.collapsed).toBe(true);
  });

  it("비용 비중은 가장 비싼 노드를 100% 로 본다", () => {
    const json = JSON.stringify({
      query_block: {
        cost_info: { query_cost: "100.00" },
        table: { table_name: "t", cost_info: { prefix_cost: "25.00" } },
      },
    });
    const rows = planRows(parsePlan(json)!, new Set());
    expect(costShare(rows, rows[0]!.node)).toBe(100);
    expect(costShare(rows, rows[1]!.node)).toBe(25);
  });

  it("비용이 없으면 비중은 `null` — 0% 로 채우지 않는다", () => {
    const rows = planRows(parsePlan('{"query_block":{"message":"No tables used"}}')!, new Set());
    expect(costShare(rows, rows[0]!.node)).toBeNull();
    // 메시지는 **라벨**로 남는다. 조건 열에 또 넣으면 표에 같은 문자열이 두 번 나온다.
    expect(rows[1]!.node.label).toBe("No tables used");
    expect(rows[1]!.node.condition).toBeUndefined();
  });

  it("v2 의 실측 시간·행 수는 표에 그대로 온다", () => {
    const rows = planRows(parsePlan(JSON.stringify(V2_ANALYZE))!, new Set());
    // 루트(Limit) → 조인 → 두 테이블 접근.
    expect(rows.map((r) => r.indent)).toEqual([0, 1, 2, 2]);
    expect(rows[1]!.node.timeMs).toBeCloseTo(0.046457);
    expect(rows[1]!.node.rows).toBe(10);
    expect(rows[3]!.node.entity).toBe("orders (o)");
  });
});
