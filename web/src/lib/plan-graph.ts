/**
 * MySQL `EXPLAIN FORMAT=JSON` → 그래프.
 *
 * # 왜 순수 함수인가
 *
 * 참조 대시보드는 캔버스에 그리면서 파싱까지 같은 `useEffect` 안에서 했다. 그래서
 * 플랜 모양이 조금만 달라도(테이블 셋 이상, `grouping_operation`, `union`) 화면이
 * 조용히 비었다 — 노드가 안 그려지는지 파싱이 실패했는지 구분할 수 없다.
 *
 * 여기서는 **그래프를 먼저 만들고** 그리기는 SVG 에 맡긴다. 그래서 모양별로
 * 테스트할 수 있고, 모르는 구조를 만나면 그 사실을 노드로 남길 수 있다.
 *
 * # 신뢰하지 않는다
 *
 * 입력은 저장된 문자열이다. 오래된 레코드나 다른 MySQL 버전의 형식이 올 수 있으므로
 * 모든 필드를 좁혀서 읽고, 모르는 키는 무시한다.
 */

export type NodeKind = "select" | "operation" | "loop" | "table" | "subquery" | "message";

export interface PlanNode {
  id: string;
  kind: NodeKind;
  label: string;
  /** 라벨 아래 줄들 (`Cost: …`, `Rows: …`). */
  details: string[];
  depth: number;
  children: string[];
}

export interface PlanGraph {
  nodes: PlanNode[];
  edges: Array<{ from: string; to: string }>;
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function num(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  // MySQL 은 `cost_info` 값을 문자열로 준다 ("353891.58").
  if (typeof v === "string") {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  return null;
}

const INT = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 0 });
const COST = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 2 });

/**
 * 플랜 JSON 문자열을 그래프로. 파싱 자체가 실패하면 `null`.
 *
 * `null` 과 "노드가 없는 그래프" 는 다르다 — 호출부가 "형식을 못 읽었다" 와
 * "계획이 비어 있다" 를 구분해 말해야 한다.
 */
export function parsePlan(json: string): PlanGraph | null {
  let root: unknown;
  try {
    root = JSON.parse(json);
  } catch {
    return null;
  }
  if (!isRecord(root)) return null;

  const nodes: PlanNode[] = [];
  const edges: Array<{ from: string; to: string }> = [];

  const add = (kind: NodeKind, label: string, details: string[], depth: number): string => {
    const id = `n${nodes.length}`;
    nodes.push({ id, kind, label, details, depth, children: [] });
    return id;
  };
  const link = (from: string, to: string) => {
    edges.push({ from, to });
    nodes.find((n) => n.id === from)?.children.push(to);
  };

  /** `table` 객체 하나를 노드로. */
  const walkTable = (table: Record<string, unknown>, depth: number): string => {
    const name = str(table.table_name) ?? "(unnamed)";
    const access = str(table.access_type);
    const details: string[] = [];
    const rows = num(table.rows_examined_per_scan);
    if (rows !== null) details.push(`Rows: ${INT.format(rows)}`);
    const filtered = num(table.filtered);
    if (filtered !== null) details.push(`Filtered: ${filtered}%`);
    const key = str(table.key);
    details.push(key === null ? "Key: (없음)" : `Key: ${key}`);
    if (isRecord(table.cost_info)) {
      const cost = num(table.cost_info.prefix_cost) ?? num(table.cost_info.read_cost);
      if (cost !== null) details.push(`Cost: ${COST.format(cost)}`);
    }
    const id = add("table", access === null ? name : `${name} (${access})`, details, depth);

    // 파생 테이블·서브쿼리는 이 테이블 아래로 이어진다.
    for (const key2 of ["materialized_from_subquery", "attached_subqueries"]) {
      const child = table[key2];
      if (isRecord(child)) {
        const sub = walkQueryBlockContainer(child, depth + 1, "Subquery");
        if (sub !== null) link(id, sub);
      } else if (Array.isArray(child)) {
        for (const one of child) {
          if (!isRecord(one)) continue;
          const sub = walkQueryBlockContainer(one, depth + 1, "Subquery");
          if (sub !== null) link(id, sub);
        }
      }
    }
    return id;
  };

  /** `{ query_block: … }` 또는 그 자체가 블록인 객체. */
  const walkQueryBlockContainer = (
    obj: Record<string, unknown>,
    depth: number,
    label: string,
  ): string | null => {
    const block = isRecord(obj.query_block) ? obj.query_block : obj;
    return walkBlock(block, depth, label);
  };

  /** 연산 노드(`ordering_operation` 등)의 공통 처리. */
  const walkOperation = (
    op: Record<string, unknown>,
    depth: number,
    label: string,
  ): string => {
    const details: string[] = [];
    if (op.using_filesort === true) details.push("using filesort");
    if (op.using_temporary_table === true) details.push("using temporary");
    const id = add("operation", label, details, depth);
    linkChildren(op, id, depth + 1);
    return id;
  };

  /**
   * 블록 안의 자식들을 잇는다. **순서를 고정한다** — MySQL JSON 의 키 순서에
   * 의존하면 같은 플랜이 실행마다 다르게 그려진다.
   */
  const linkChildren = (block: Record<string, unknown>, parent: string, depth: number) => {
    for (const [key, label] of [
      ["ordering_operation", "Ordering"],
      ["duplicates_removal", "Duplicates removal"],
      ["grouping_operation", "Grouping"],
    ] as const) {
      const op = block[key];
      if (isRecord(op)) link(parent, walkOperation(op, depth, label));
    }

    if (Array.isArray(block.nested_loop)) {
      const loopId = add("loop", `Nested loop (${block.nested_loop.length})`, [], depth);
      link(parent, loopId);
      for (const step of block.nested_loop) {
        if (isRecord(step) && isRecord(step.table)) {
          link(loopId, walkTable(step.table, depth + 1));
        }
      }
    }

    if (isRecord(block.table)) {
      link(parent, walkTable(block.table, depth));
    }

    if (isRecord(block.union_result)) {
      const unionId = add("subquery", "Union result", [], depth);
      link(parent, unionId);
      const specs = block.union_result.query_specifications;
      if (Array.isArray(specs)) {
        for (const spec of specs) {
          if (!isRecord(spec)) continue;
          const sub = walkQueryBlockContainer(spec, depth + 1, "Select");
          if (sub !== null) link(unionId, sub);
        }
      }
    }
  };

  const walkBlock = (
    block: Record<string, unknown>,
    depth: number,
    label: string,
  ): string | null => {
    const details: string[] = [];
    const selectId = num(block.select_id);
    if (selectId !== null) details.push(`select_id: ${selectId}`);
    if (isRecord(block.cost_info)) {
      const cost = num(block.cost_info.query_cost);
      if (cost !== null) details.push(`Cost: ${COST.format(cost)}`);
    }
    const id = add("select", label, details, depth);

    // `message` 는 "테이블을 쓰지 않는다" 처럼 옵티마이저가 남긴 설명이다.
    // **버리지 않는다** — 이게 유일한 내용인 플랜이 실제로 있다.
    const message = str(block.message);
    if (message !== null) {
      link(id, add("message", message, [], depth + 1));
    }
    linkChildren(block, id, depth + 1);
    return id;
  };

  /**
   * v2 (`explain_json_format_version=2`, MySQL 8.3+) 트리.
   *
   * v1 과 **키 이름이 완전히 다르다** — `operation`·`inputs`·`estimated_*`. v1 만
   * 읽으면 v2 계획이 노드 하나짜리 그래프로 그려져 **그럴싸하게 비어 있다.**
   * 백엔드가 v2 를 지원하므로(`planparse::PlanFormat::JsonV2`) 여기서도 읽는다.
   */
  const walkV2 = (node: Record<string, unknown>, depth: number): string => {
    const operation = str(node.operation);
    const table = str(node.table_name);
    const access = str(node.access_type);
    const label =
      operation ??
      (table === null ? "Operation" : access === null ? table : `${table} (${access})`);

    const details: string[] = [];
    const rows = num(node.estimated_rows);
    if (rows !== null) details.push(`Rows: ${INT.format(rows)}`);
    const cost = num(node.estimated_total_cost);
    if (cost !== null) details.push(`Cost: ${COST.format(cost)}`);
    if (table !== null && operation !== null) details.push(`Table: ${table}`);
    const key = str(node.covering_index) ?? str(node.index_name) ?? str(node.key);
    if (key !== null) details.push(`Key: ${key}`);

    // 테이블 접근인지 연산인지로 색을 나눈다.
    const kind: NodeKind = table !== null && operation === null ? "table" : "operation";
    const id = add(kind, label, details, depth);

    if (Array.isArray(node.inputs)) {
      for (const child of node.inputs) {
        if (isRecord(child)) link(id, walkV2(child, depth + 1));
      }
    }
    return id;
  };

  if (isRecord(root.query_plan)) {
    walkV2(root.query_plan, 0);
    return { nodes, edges };
  }

  const start = isRecord(root.query_block) ? root.query_block : root;
  walkBlock(start, 0, "Select");

  return { nodes, edges };
}

/** 노드 색. 참조 대시보드의 색을 그대로 쓴다. */
export const NODE_COLOR: Record<NodeKind, string> = {
  select: "#e63946",
  operation: "#457b9d",
  loop: "#2a9d8f",
  table: "#f4a261",
  subquery: "#8b5cf6",
  message: "#94a3b8",
};

export interface PlacedNode extends PlanNode {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface PlanLayout {
  nodes: PlacedNode[];
  edges: Array<{ from: string; to: string }>;
  width: number;
  height: number;
}

const NODE_W = 210;
const NODE_H = 78;
const GAP_X = 70;
const GAP_Y = 16;
const PAD = 16;

/**
 * 깊이 = x, 잎 순서 = y. 부모는 자식들의 가운데로 올린다.
 *
 * 참조 구현은 노드 **라벨 문자열로** 좌표를 정했다(`case 'Nested_Loop#2'`). 그래서
 * 세 번째 루프나 세 번째 테이블은 좌표가 없어 같은 자리에 겹쳐 그려졌다.
 */
export function layoutPlan(graph: PlanGraph): PlanLayout {
  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  const placed = new Map<string, PlacedNode>();
  let cursorY = PAD;

  const place = (id: string): PlacedNode => {
    const existing = placed.get(id);
    if (existing !== undefined) return existing;
    const node = byId.get(id);
    if (node === undefined) throw new Error(`알 수 없는 노드: ${id}`);

    const childBoxes = node.children.map(place);
    const y =
      childBoxes.length === 0
        ? (() => {
            const at = cursorY;
            cursorY += NODE_H + GAP_Y;
            return at;
          })()
        : (childBoxes[0]!.y + childBoxes[childBoxes.length - 1]!.y) / 2;

    const box: PlacedNode = {
      ...node,
      x: PAD + node.depth * (NODE_W + GAP_X),
      y,
      width: NODE_W,
      height: NODE_H,
    };
    placed.set(id, box);
    return box;
  };

  for (const node of graph.nodes) place(node.id);

  const nodes = [...placed.values()];
  const width = Math.max(...nodes.map((n) => n.x + n.width), NODE_W) + PAD;
  const height = Math.max(...nodes.map((n) => n.y + n.height), NODE_H) + PAD;
  return { nodes, edges: graph.edges, width, height };
}
