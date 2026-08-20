/**
 * 화면 통합 테스트.
 *
 * # 왜 DOM 까지 내려와서 보는가
 *
 * 단위 테스트로는 두 부류의 결함을 못 잡는다.
 *
 * 1. **인증이 거부됐는데 표에 데이터가 남는다.** 스냅샷과 캐시를 비웠는데도 이미
 *    마운트된 화면이 마지막 결과를 계속 그렸다(`queryClient.clear()` 는 관찰자를
 *    되돌리지 않는다). 브라우저에서 토큰을 무효화해 확인한 실제 결함이다.
 * 2. **표가 백엔드 응답의 필드를 실제로 읽는지.** 타입만 맞으면 컴파일은 통과한다.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeSocket } from "./lib/fake-socket";

const INSTANCE = "000000000000/ap-northeast-2/mysql84-local";
const SQL = "SELECT count ( * ) FROM orders WHERE id = ?";

const realWebSocket = globalThis.WebSocket;
const realFetch = globalThis.fetch;

/** 401 로 뒤집을 수 있는 가짜 백엔드. */
let unauthorized = false;

function json(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

const RECORD = {
  record_id: `${INSTANCE}:12:1700000000`,
  instance_id: INSTANCE,
  env: "dev",
  state: "finalized",
  thread_id: 12,
  started_at_ms: Date.now(),
  duration_ms: 7002,
  duration_source: "slowlog",
  capture_source: "merged",
  app_digest: "d1",
  statement_type: "select",
  schema_name: "shop",
  db_user: "loadgen",
  sql_text: SQL,
  sql_redacted_reason: null,
  sql_text_truncated: false,
  rows_examined: 20001,
  rows_sent: 1,
  lock_time_ms: 0,
  has_plan: true,
  abandoned_reason: null,
};

function fakeBackend(input: RequestInfo | URL): Promise<Response> {
  const url = typeof input === "string" ? input : input.toString();
  if (unauthorized) {
    return Promise.resolve(
      new Response(JSON.stringify({ error: "unauthorized" }), {
        status: 401,
        headers: { "content-type": "application/json" },
      }),
    );
  }
  if (url.startsWith("/api/aws/info")) {
    return Promise.resolve(
      json({ account_id: "000000000000", region: "ap-northeast-2", deployment_env: "dev" }),
    );
  }
  if (url.startsWith("/api/instances")) {
    return Promise.resolve(
      json([
        {
          id: INSTANCE,
          name: "mysql84-local",
          env: "dev",
          env_from_tags: "dev",
          env_override: null,
          state: "collecting",
          engine: "mysql",
          engine_version: "8.4.6",
          endpoint: "127.0.0.1",
          port: 3306,
          instance_class: "db.t4g.micro",
          cluster_id: null,
          is_cluster_writer: false,
          iam_auth_enabled: false,
          vpc_id: "vpc-local",
          availability_zone: "ap-northeast-2a",
          collectible: true,
          tags: { env: "dev", team: "dba" },
          first_seen_ms: Date.now() - 86_400_000,
          last_seen_ms: Date.now(),
          deleted_at_ms: null,
          cert_valid_till_ms: null,
        },
      ]),
    );
  }
  if (url.startsWith("/api/slow-queries") || url.startsWith("/api/plans")) {
    return Promise.resolve(json({ items: [RECORD], next_cursor: null, has_more: false, total: 1 }));
  }
  if (url.includes("/plan")) {
    return Promise.resolve(
      json({
        record_id: RECORD.record_id,
        instance_id: INSTANCE,
        started_at_ms: RECORD.started_at_ms,
        duration_ms: RECORD.duration_ms,
        statement_type: "select",
        normalized_json: '{"query_block":{"select_id":1,"message":"No tables used"}}',
        tree_text: null,
        format_version: "json_v1",
        s3_key: null,
        fingerprint: "e3b0c44298fc1c14",
        referenced_tables: [],
        source: "rerun",
        error: null,
      }),
    );
  }
  if (url.startsWith("/api/digests")) {
    return Promise.resolve(
      json({
        items: [
          {
            instance_id: INSTANCE,
            app_digest: "d1",
            digest_query: SQL,
            users: ["loadgen"],
            statement_type: "select",
            schema_name: "shop",
            exec_count: 21,
            total_time_ms: 186_774,
            avg_time_ms: 8_894,
            max_time_ms: 11_002,
            avg_rows_examined: 1_234,
            first_seen_ms: Date.now() - 1000,
            last_seen_ms: Date.now(),
          },
        ],
        scanned: 83,
        truncated: false,
        from_ms: Date.now() - 86_400_000,
        to_ms: Date.now(),
        month: "2026-08",
      }),
    );
  }
  if (url.startsWith("/api/statistics/users")) {
    return Promise.resolve(
      json({
        items: [
          {
            instance_id: INSTANCE,
            user: "loadgen",
            total_queries: 51,
            unique_digest_count: 6,
            total_exec_time_ms: 1_580_312,
            avg_execution_time_ms: 30_987,
            max_execution_time_ms: 1_287_986,
            read_query_count: 47,
            write_query_count: 4,
            ddl_query_count: 0,
            commit_query_count: 0,
            other_query_count: 0,
          },
        ],
        scanned: 83,
        truncated: false,
        from_ms: 0,
        to_ms: 1,
        month: "2026-08",
      }),
    );
  }
  if (url.startsWith("/api/statistics")) {
    return Promise.resolve(
      json({
        items: [
          {
            instance_id: INSTANCE,
            unique_digest_count: 8,
            total_slow_query_count: 83,
            total_execution_count: 83,
            total_execution_time_ms: 1_687_477,
            avg_execution_time_ms: 20_331,
            max_execution_time_ms: 1_287_986,
            total_rows_examined: 85_574,
            read_query_count: 58,
            write_query_count: 4,
            ddl_query_count: 0,
            commit_query_count: 0,
            other_query_count: 21,
            first_seen_ms: 0,
            last_seen_ms: 1,
          },
        ],
        scanned: 83,
        truncated: true,
        from_ms: 0,
        to_ms: 1,
        month: "2026-08",
      }),
    );
  }
  if (url.startsWith("/api/collector/status")) {
    return Promise.resolve(
      json({
        paused: false,
        paused_since_ms: null,
        is_leader: true,
        collecting: 1,
        last_tick_ms: Date.now(),
        last_discovery_ms: Date.now() - 60_000,
        last_backfill_ms: Date.now() - 30_000,
        discovery_requested: false,
        backfill_requested: false,
        worker_id: "all-local",
        scope: "process",
        runs_collector: true,
        can_control: true,
        role: "admin",
      }),
    );
  }
  if (url.startsWith("/api/auth/config")) {
    return Promise.resolve(
      json({ mode: "local-token", cognito_configured: false, deployment_env: "dev" }),
    );
  }
  return Promise.resolve(new Response("not found", { status: 404 }));
}

beforeEach(() => {
  unauthorized = false;
  FakeSocket.install();
  // **모듈 상태를 리셋한다.** `liveClient` 는 싱글턴이라 한 테스트에서
  // `unauthorized` 가 되면 다음 테스트에서 아예 접속하지 않는다(그게 프로덕션
  // 에서는 올바른 동작이다).
  vi.resetModules();
  globalThis.fetch = fakeBackend as typeof fetch;
  sessionStorage.setItem("dbmon.token", "test-token");
});

afterEach(() => {
  // ⚠ **자동 정리에 기대지 않는다.** RTL 의 auto-cleanup 은 `afterEach` 가
  // 전역일 때만 붙는데 이 프로젝트는 `globals: false` 다 — 정리하지 않으면 앞
  // 테스트의 DOM 이 남아 조회 결과가 조용히 어긋난다.
  cleanup();
  globalThis.WebSocket = realWebSocket;
  globalThis.fetch = realFetch;
  sessionStorage.clear();
});

async function renderApp(path: string) {
  // 리셋한 모듈 그래프에서 새로 가져온다 — 정적 import 는 옛 싱글턴을 붙든다.
  const { App } = await import("./App");
  // 재시도를 끄는 이유: 401 재시도 정책은 `main.tsx` 의 관심사이고, 여기서는
  // 화면 전이만 본다.
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[path]}>
        <App />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("MySQL Monitor", () => {
  it("슬로우 쿼리 표와 AWS 계정을 보여준다", async () => {
    await renderApp("/mysql");
    expect(await screen.findByText(SQL)).toBeDefined();
    // 계정 착각을 막는 머리말. `<strong>Account:</strong>` 와 값이 형제라서
    // 머리말 전체(`role=banner`)를 본다.
    expect(screen.getByRole("banner").textContent).toContain("000000000000");
    expect(screen.getByRole("banner").textContent).toContain("ap-northeast-2");
    // 조사 행이 표에 실제로 들어간다.
    expect(screen.getByText(/20,001/)).toBeDefined();
    // 수집 제어가 상태를 읽어 버튼을 고른다(수집 중이면 "정지").
    expect(await screen.findByText("수집 정지")).toBeDefined();
    // 상태를 표에 보여준다 — 진행 중·추적 끊김·확정이 같아 보이면 하한을
    // 확정값으로 읽는다.
    expect(screen.getByText("확정")).toBeDefined();
    // 워커 식별자는 상태 줄과 사실 표 두 곳에 나온다 — 어느 워커를 멈추는지
    // 헷갈리면 안 되는 값이라 일부러 두 번 적는다.
    expect(screen.getAllByText(/all-local/).length).toBeGreaterThanOrEqual(1);
  });

  it("실시간 지표가 오면 상태 표에 채운다", async () => {
    await renderApp("/mysql");
    expect(await screen.findByText(SQL)).toBeDefined();

    const socket = FakeSocket.latest();
    socket.makeReady();
    socket.deliver({
      t: "status",
      instance_id: INSTANCE,
      metrics: {
        at_ms: Date.now(),
        qps: 13.4,
        slow_per_sec: 0,
        threads_running: 3,
        threads_connected: 5,
        lock_waits: 0,
        rate_gap_reason: null,
      },
    });

    expect(await screen.findByText("13.4")).toBeDefined();
  });
});

describe("인증 거부", () => {
  it("스트림이 거부되면 표에 남아 있던 SQL 이 사라지고 토큰 안내가 나온다", async () => {
    await renderApp("/mysql");
    expect(await screen.findByText(SQL)).toBeDefined();

    // 서버가 재인증에서 거부한다(T-33: 권한이 줄었거나 토큰이 만료됐다).
    unauthorized = true;
    FakeSocket.latest().accept();
    FakeSocket.latest().deliver({ t: "error", code: "unauthorized" });

    await waitFor(() => {
      expect(screen.queryByText(SQL)).toBeNull();
    });
    expect(screen.getByText("접속 토큰이 필요하다")).toBeDefined();
    // 거부된 토큰은 버려야 한다 — 남겨 두면 계속 401 을 만든다.
    expect(sessionStorage.getItem("dbmon.token")).toBeNull();
  });
});

describe("다이제스트·통계·플랜·인스턴스 화면", () => {
  it("다이제스트 표가 집계 값을 읽는다", async () => {
    await renderApp("/cloudwatch");
    expect(await screen.findByText("21")).toBeDefined(); // exec count
    expect(screen.getByText("186.77")).toBeDefined(); // total time (s)
    expect(screen.getByText("loadgen")).toBeDefined();
  });

  it("천장에 걸리면 통계 화면이 그 사실을 말한다", async () => {
    await renderApp("/statistics");
    // 조용히 자르지 않는다 — 이 문구가 없으면 "그만큼만 실행됐다" 로 읽힌다.
    expect(await screen.findByText(/조회 상한에 걸려/)).toBeDefined();
    expect(screen.getByText(/읽기 58/)).toBeDefined();
  });

  it("플랜 화면이 계획 그래프를 그린다", async () => {
    await renderApp("/plan");
    // `message` 만 있는 계획도 노드로 나온다. SVG 는 `<text>` 와 툴팁 `<title>`
    // 둘에 같은 문자열을 담으므로 둘 다 세어 준다.
    expect(await screen.findAllByText("No tables used")).toHaveLength(2);
    expect(screen.getByRole("img", { name: /실행계획 노드/ })).toBeDefined();
  });

  it("RDS 화면이 태그와 수집 여부를 보여준다", async () => {
    await renderApp("/rds");
    expect(await screen.findByText("env=dev")).toBeDefined();
    expect(screen.getByText("team=dba")).toBeDefined();
    expect(screen.getByText("수집")).toBeDefined();
  });
});
