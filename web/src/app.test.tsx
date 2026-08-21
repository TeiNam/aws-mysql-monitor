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
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeSocket } from "./lib/fake-socket";

const INSTANCE = "000000000000/ap-northeast-2/mysql84-local";
const SQL = "SELECT count ( * ) FROM orders WHERE id = ?";

const realWebSocket = globalThis.WebSocket;
const realFetch = globalThis.fetch;

/** 401 로 뒤집을 수 있는 가짜 백엔드. */
let unauthorized = false;
/** 서버가 조회 상한에 걸려 **0건**을 준 상황. */
let emptyButTruncated = false;

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
          allocated_storage_gb: 20,
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
    if (emptyButTruncated) {
      return Promise.resolve(json({ items: [], next_cursor: null, has_more: true, total: 0 }));
    }
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
            digest_query_redacted_reason: null,
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
  if (url.startsWith("/api/metrics/fleet")) {
    return Promise.resolve(
      json({
        rows: [
          {
            instance_id: INSTANCE,
            metrics: [
              { name: "CPUUtilization", label: "CPU", unit: "percent", stat: "Average", value: 12.5 },
              {
                name: "FreeableMemory",
                label: "여유 메모리",
                unit: "bytes",
                stat: "Average",
                value: 100_663_296,
              },
              // 값이 **없는** 메트릭. `0` 으로 접히면 "스토리지가 꽉 찼다" 가 된다.
              { name: "FreeStorageSpace", label: "여유 스토리지", unit: "bytes", stat: "Minimum", value: null },
            ],
          },
        ],
        failed_scopes: [],
        period_secs: 900,
        lag_note: "CloudWatch 는 1~3분 지연된다",
      }),
    );
  }
  if (url.startsWith("/api/collector/status")) {
    return Promise.resolve(
      json({
        paused: false,
        paused_since_ms: null,
        paused_scopes: [],
        is_leader: true,
        collecting: 1,
        last_tick_ms: Date.now(),
        last_discovery_ms: Date.now() - 60_000,
        last_backfill_ms: Date.now() - 30_000,
        discovery_requested: false,
        backfill_requested: false,
        worker_id: "all-local",
        scope: "deployment",
        runs_collector: true,
        can_control: true,
        controllable_envs: ["prd", "stg", "dev", "unknown"],
        role: "admin",
      }),
    );
  }
  if (url.startsWith("/api/auth/config")) {
    return Promise.resolve(
      json({
        mode: "local-token",
        cognito_configured: false,
        cognito: { user_pool_id: "", client_id: "", region: null, domain: "" },
        deployment_env: "dev",
      }),
    );
  }
  return Promise.resolve(new Response("not found", { status: 404 }));
}

beforeEach(() => {
  unauthorized = false;
  emptyButTruncated = false;
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
    // **이 화면에는 배지만 있다.** 조작은 RDS 화면 한 곳에서 한다 — 여기 버튼이
    // 남아 있으면 "어느 화면에서 누른 것이 무엇에 적용되나" 를 되묻게 된다.
    expect(await screen.findByText("수집 중")).toBeDefined();
    expect(screen.queryByText("수집 정지")).toBeNull();
    expect(screen.queryByLabelText("정지 대상 선택")).toBeNull();
    // 상태를 표에 보여준다 — 진행 중·추적 끊김·확정이 같아 보이면 하한을
    // 확정값으로 읽는다.
    expect(screen.getByText("확정")).toBeDefined();
    // 워커 식별자는 상태 줄과 사실 표 두 곳에 나온다 — 어느 워커를 멈추는지
    // 헷갈리면 안 되는 값이라 일부러 두 번 적는다.
    expect(screen.getAllByText(/all-local/).length).toBeGreaterThanOrEqual(1);
  });

  /**
   * **인스턴스 목록은 이 화면에 없다.** Metrics 탭이 같은 목록에 CloudWatch 열까지
   * 붙여 보여주므로, 두 화면에 같은 표를 두면 한쪽만 고치게 된다.
   */
  it("인스턴스 지표 표를 여기 두지 않는다", async () => {
    await renderApp("/mysql");
    expect(await screen.findByText(SQL)).toBeDefined();
    expect(screen.queryByText("QPS 추이")).toBeNull();
    // 수집기 사실(워커·리더·마지막 tick)은 남는다 — 슬로우 쿼리를 보다가
    // "지금 수집이 도는가" 를 묻게 되는 곳이 여기다.
    expect(screen.getAllByText(/all-local/).length).toBeGreaterThanOrEqual(1);
  });

  /**
   * **0건이 "없다" 로 읽히면 안 된다.**
   *
   * 서버는 인스턴스가 많거나 구간이 길면 표본을 잘라 읽고 `has_more=true` 로
   * 말한다. 그 표본이 환경 필터에서 전부 빠지면 0건이 온다. 상한 표시는
   * `Pagination` 이 하는데 그건 0건에서 렌더되지 않으므로, 표 안에서 말해야 한다 —
   * 안 그러면 조사하던 사람이 "문제 없음" 으로 결론 낸다.
   */
  it("서버가 상한에 걸려 0건을 주면 '기록이 없다' 로 말하지 않는다", async () => {
    emptyButTruncated = true;
    await renderApp("/mysql");
    expect(await screen.findByText(/조회 상한에 걸려/)).toBeDefined();
    expect(screen.queryByText(/조회 구간\(최근 24시간\)에 기록이 없다/)).toBeNull();
  });
});

/**
 * 플릿 메트릭 화면. **한 줄에 두 출처가 섞인다** — CloudWatch(15분)와 자체 수집(5초).
 * 타입만 맞으면 컴파일은 통과하므로, 표가 두 응답의 필드를 실제로 읽는지 DOM 에서 본다.
 */
describe("플릿 메트릭", () => {
  it("CloudWatch 값과 실시간 지표를 한 행에 채운다", async () => {
    await renderApp("/metrics");
    expect(await screen.findByText("mysql84-local")).toBeDefined();
    // CloudWatch 열: 단위는 서버가 알려준 것을 따른다(퍼센트 / 이진 바이트).
    expect(screen.getByText("12.5%")).toBeDefined();
    expect(screen.getByText("96MB")).toBeDefined();

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
        max_connections: 60,
        lock_waits: 0,
        rate_gap_reason: null,
      },
    });

    expect(await screen.findByText("13.4")).toBeDefined();
    // **연결은 모수와 함께 적는다.** 개수만 보면 `5` 가 한가한지 포화 직전인지 알 수 없다.
    expect(await screen.findByText("5 / 60")).toBeDefined();
  });

  /** 모수가 없으면 **개수만** 적는다 — `5 / —` 는 "모수가 0" 으로 읽힌다. */
  it("모수를 모르면 연결 개수만 적는다", async () => {
    await renderApp("/metrics");
    expect(await screen.findByText("mysql84-local")).toBeDefined();

    const socket = FakeSocket.latest();
    socket.makeReady();
    socket.deliver({
      t: "status",
      instance_id: INSTANCE,
      metrics: {
        at_ms: Date.now(),
        qps: 1,
        slow_per_sec: 0,
        threads_running: 3,
        threads_connected: 5,
        // 옛 서버는 이 값을 보내지 않는다.
        max_connections: null,
        lock_waits: 0,
        rate_gap_reason: null,
      },
    });

    expect(await screen.findByText("5")).toBeDefined();
    expect(screen.queryByText(/5 \//)).toBeNull();
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

  /**
   * **기본은 표(plan) 탭이다.** 그래프는 한 번 눌러야 나온다.
   *
   * 탭 순서를 바꿨을 때 이 테스트가 "그래프가 없다" 로 깨졌다 — 기본 뷰가 무엇인지를
   * 고정하고 있었다는 뜻이고, 그건 고정해 둘 값어치가 있다. 그래서 둘 다 확인한다.
   */
  it("플랜 화면은 표를 먼저 보여주고, 그래프 탭으로 바꿀 수 있다", async () => {
    await renderApp("/plan");
    // 표: `message` 만 있는 계획도 행으로 나온다(조건 열에 그대로).
    expect(await screen.findByRole("columnheader", { name: "Node Type" })).toBeDefined();
    expect(await screen.findAllByText("No tables used")).toHaveLength(1);
    // 아직 계획 그래프는 없다.
    expect(screen.queryByRole("img", { name: /실행계획 노드/ })).toBeNull();

    // 그래프 탭으로. **`fireEvent` 로 누른다** — DOM 의 `.click()` 은 상태 갱신이
    // `act()` 밖에서 일어나 다음 단정까지 렌더가 반영되지 않을 수 있다(실제로 그래서
    // 표가 그대로 보였다).
    fireEvent.click(await screen.findByRole("tab", { name: "그래프" }));
    // SVG 는 `<text>` 와 툴팁 `<title>` 둘에 같은 문자열을 담으므로 둘 다 세어 준다.
    expect(await screen.findAllByText("No tables used")).toHaveLength(2);
    expect(screen.getByRole("img", { name: /실행계획 노드/ })).toBeDefined();
  });

  /**
   * 옵션 탭이 실제로 테마를 바꾸는지 — nav → 화면 → `<html class="dark">` 배선을 본다.
   * 조각마다 테스트가 있어도 이 사슬이 끊기면 "눌러도 안 바뀐다" 가 된다.
   */
  it("옵션 화면에서 다크를 고르면 문서에 적용된다", async () => {
    await renderApp("/options");
    const dark = (await screen.findByLabelText(/다크/)) as HTMLInputElement;
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    dark.click();
    expect(document.documentElement.classList.contains("dark")).toBe(true);

    (screen.getByLabelText(/라이트/) as HTMLInputElement).click();
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  it("RDS 화면이 태그와 수집 여부를 보여준다", async () => {
    await renderApp("/rds");
    expect(await screen.findByText("env=dev")).toBeDefined();
    expect(screen.getByText("team=dba")).toBeDefined();
    expect(screen.getByText("수집")).toBeDefined();
  });

  /**
   * 정지 조작은 **이 화면에만** 있다. 위쪽 셀렉터는 전체·환경만 담고, 개별 인스턴스는
   * 표의 행 버튼이 맡는다 — 500대 등록부에서 셀렉터가 500줄이 되지 않게 한다.
   */
  it("RDS 화면에서 전체·환경은 셀렉터로, 개별 인스턴스는 행 버튼으로 정지한다", async () => {
    await renderApp("/rds");
    const select = (await screen.findByLabelText("정지 대상 선택")) as HTMLSelectElement;
    // **상태가 오기 전에는 전부 비활성이다** — 권한을 모르는 채로 누를 수 있게 두면
    // 403 을 받는다. 그래서 상태가 온 뒤에 판정한다.
    await screen.findByText("수집 중");
    const options = [...select.options].map((o) => ({ value: o.value, disabled: o.disabled }));

    // 전체는 전 환경 스코프 admin 이므로 누를 수 있다.
    expect(options[0]).toEqual({ value: "*", disabled: false });
    // 등록부에 보이는 환경(`env=dev` 태그 하나)이 선택지가 된다. 코드에 박은 목록이
    // 아니라 응답에서 만들므로 스코프 밖 환경이 제시되지 않는다.
    expect(options.map((o) => o.value)).toContain("env:dev");
    // **인스턴스는 셀렉터에 없다.**
    expect(options.some((o) => o.value.startsWith("id:"))).toBe(false);

    // 멈춘 것이 없으므로 상단 버튼은 "정지" 다.
    expect(screen.getByText("수집 정지")).toBeDefined();
    // 표의 행마다 개별 정지 버튼이 있다.
    expect(screen.getByRole("button", { name: /^정지$/ })).toBeDefined();
  });
});
