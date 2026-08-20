/**
 * 화면 통합 테스트 — **인증이 거부되면 화면에서 데이터가 사라져야 한다.**
 *
 * # 왜 DOM 까지 내려와서 보는가
 *
 * 단위 테스트로는 이 결함을 못 잡는다. 스냅샷은 비워졌고 캐시도 지웠는데
 * **이미 마운트된 화면이 마지막 결과를 계속 그리고 있었다** (`queryClient.clear()`
 * 는 관찰자를 되돌리지 않는다). 실제로 브라우저에서 토큰을 무효화하고 확인한
 * 결함이고, 표에 34행의 SQL 이 그대로 남아 있었다.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeSocket } from "./lib/fake-socket";

const INSTANCE = "000000000000/ap-northeast-2/mysql84-local";
const SQL = "SELECT sleep ( ? )";

const realWebSocket = globalThis.WebSocket;
const realFetch = globalThis.fetch;

/** 401 로 뒤집을 수 있는 가짜 백엔드. */
let unauthorized = false;

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

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
  if (url.startsWith("/api/instances")) {
    return Promise.resolve(
      jsonResponse([
        {
          id: INSTANCE,
          env: "dev",
          state: "collecting",
          engine: "mysql",
          engine_version: "8.4.6",
          endpoint: "127.0.0.1",
          collectible: true,
        },
      ]),
    );
  }
  if (url.startsWith("/api/slow-queries")) {
    return Promise.resolve(
      jsonResponse({
        items: [
          {
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
            rows_examined: 1,
            rows_sent: 1,
            lock_time_ms: 0,
            has_plan: false,
            abandoned_reason: null,
          },
        ],
        next_cursor: null,
        has_more: false,
      }),
    );
  }
  if (url.startsWith("/api/auth/config")) {
    return Promise.resolve(
      jsonResponse({ mode: "local-token", cognito_configured: false, deployment_env: "dev" }),
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

describe("인증 거부", () => {
  it("스트림이 거부되면 표에 남아 있던 SQL 이 사라지고 토큰 안내가 나온다", async () => {
    await renderApp("/slow-queries");
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

describe("플릿 개요", () => {
  it("실시간 지표가 오면 표에 채운다", async () => {
    await renderApp("/");
    expect(await screen.findByText("mysql84-local")).toBeDefined();

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

    // 두 곳에 나타난다 — 합계 타일과 인스턴스 행. 하나만 찾으면 어느 쪽이
    // 비었는지 모른다.
    expect(await screen.findAllByText("13.4")).toHaveLength(2);
  });

  it("구독이 거부되면 배너로 알린다", async () => {
    await renderApp("/");
    expect(await screen.findByText("mysql84-local")).toBeDefined();

    const socket = FakeSocket.latest();
    socket.makeReady();
    socket.deliver({ t: "subscribed", topics: [], denied: [`status:inst=${INSTANCE}`] });

    expect(await screen.findByText(/구독이 거부된 토픽/)).toBeDefined();
  });
});
