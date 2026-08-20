/**
 * 소켓 배관 테스트.
 *
 * 판정은 `live-reduce.test.ts` 가 본다. 여기서 보는 것은 **순서와 수명**이다 —
 * 이 부분이 틀리면 화면이 조용히 비고(구독을 안 보냄) 그게 "데이터가 없다" 로
 * 보인다. 이 프로젝트에서 열 번 재발한 실패 유형이다.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeSocket } from "./fake-socket";
import { LiveClient } from "./live";

const realWebSocket = globalThis.WebSocket;

const latest = () => FakeSocket.latest();

beforeEach(() => {
  FakeSocket.install();
  vi.useFakeTimers();
  sessionStorage.clear();
});

afterEach(() => {
  vi.useRealTimers();
  globalThis.WebSocket = realWebSocket;
});

/** 관찰자를 붙인다 — 첫 관찰자가 붙을 때 연결한다. */
function connectedClient(): LiveClient {
  const client = new LiveClient("ws://test/api/ws");
  client.subscribe(() => {});
  return client;
}

describe("연결 순서", () => {
  it("첫 메시지는 반드시 auth 다", () => {
    // 서버는 첫 메시지가 `auth` 가 아니면 즉시 끊는다(ws.rs).
    const client = connectedClient();
    client.retain(["slowq:env=dev"]);
    latest().accept();

    expect(latest().parsedSent()[0]).toEqual({ t: "auth" });
  });

  it("토큰이 있으면 auth 에 담는다", () => {
    sessionStorage.setItem("dbmon.token", "abc123");
    connectedClient();
    latest().accept();

    expect(latest().parsedSent()[0]).toEqual({ t: "auth", token: "abc123" });
  });

  it("소켓이 열렸어도 ready 를 받기 전에는 구독을 보내지 않는다", () => {
    // 인증이 확인되지 않은 연결에 구독을 보내고 `sent` 에 적어 두면, 그 연결이
    // 곧 끊길 때 "보냈다" 는 기록만 남아 재접속 후 다시 보내지 못한다.
    const client = connectedClient();
    latest().accept();
    client.retain(["slowq:env=dev"]);

    expect(latest().parsedSent().some((m) => m.t === "subscribe")).toBe(false);

    latest().deliver({
      t: "ready",
      user: { subject: "u", role: "admin", env_scope: ["dev"], can_see_literals: false },
    });
    expect(latest().parsedSent().at(-1)).toEqual({
      t: "subscribe",
      topics: ["slowq:env=dev"],
    });
  });
});

describe("토픽 참조 계수", () => {
  it("두 화면이 같은 토픽을 쓰면 하나가 떠나도 구독을 유지한다", () => {
    // 계수를 세지 않으면 한 화면이 언마운트될 때 다른 화면의 스트림이 끊긴다.
    const client = connectedClient();
    latest().makeReady();
    const releaseA = client.retain(["slowq:env=dev"]);
    client.retain(["slowq:env=dev"]);

    releaseA();
    expect(latest().parsedSent().some((m) => m.t === "unsubscribe")).toBe(false);
  });

  it("마지막 요구가 사라지면 구독을 해제한다", () => {
    const client = connectedClient();
    latest().makeReady();
    const release = client.retain(["slowq:env=dev"]);

    release();
    expect(latest().parsedSent().at(-1)).toEqual({
      t: "unsubscribe",
      topics: ["slowq:env=dev"],
    });
  });

  it("같은 토픽을 두 번 보내지 않는다", () => {
    const client = connectedClient();
    latest().makeReady();
    client.retain(["slowq:env=dev"]);
    client.retain(["slowq:env=dev"]);

    const subscribes = latest()
      .parsedSent()
      .filter((m) => m.t === "subscribe");
    expect(subscribes).toHaveLength(1);
  });
});

describe("재접속", () => {
  it("끊기면 다시 붙고 구독을 복구한다", () => {
    const client = connectedClient();
    latest().makeReady();
    client.retain(["slowq:env=dev"]);
    const first = latest();

    first.close();
    expect(client.getSnapshot().conn).toBe("closed");

    // 지터가 있으므로 최대 지연만큼 돌린다.
    vi.advanceTimersByTime(1_000);
    expect(FakeSocket.instances).toHaveLength(2);

    latest().makeReady();
    expect(latest().parsedSent()).toEqual([
      { t: "auth" },
      { t: "subscribe", topics: ["slowq:env=dev"] },
    ]);
  });

  it("인증까지 못 간 연결이 반복되면 지연이 늘어난다", () => {
    // `open` 에서 백오프를 초기화하면, 업그레이드 직후 서버가 끊는 상황에서
    // **최소 지연으로 무한 재접속**한다.
    connectedClient();
    for (let i = 0; i < 3; i += 1) {
      latest().accept();
      latest().close();
      vi.advanceTimersByTime(4_000);
    }
    const before = FakeSocket.instances.length;

    latest().accept();
    latest().close();
    vi.advanceTimersByTime(900);
    expect(FakeSocket.instances).toHaveLength(before);

    vi.advanceTimersByTime(8_000);
    expect(FakeSocket.instances.length).toBeGreaterThan(before);
  });

  it("unauthorized 면 다시 붙지 않는다", () => {
    // 같은 토큰으로 무한 재접속하면 서버 로그만 더럽히고 화면은 "연결 중" 에서
    // 멈춘 것처럼 보인다.
    connectedClient();
    latest().accept();
    latest().deliver({ t: "error", code: "unauthorized" });
    latest().close();

    vi.advanceTimersByTime(60_000);
    expect(FakeSocket.instances).toHaveLength(1);
  });

  it("다시 연결 버튼은 닫히는 중인 소켓도 처리한다", () => {
    // 서버가 `unauthorized` 를 보낸 직후에는 소켓이 아직 열려 있다. 그때
    // 버튼이 아무 일도 하지 않으면 사용자는 화면이 멈춘 것으로 본다.
    const client = connectedClient();
    latest().accept();
    latest().deliver({ t: "error", code: "unauthorized" });
    expect(client.getSnapshot().conn).toBe("unauthorized");

    client.reconnectNow();
    vi.advanceTimersByTime(1_000);
    expect(FakeSocket.instances).toHaveLength(2);
  });
});

describe("하트비트", () => {
  it("무응답 종료(60초)보다 자주 ping 한다", () => {
    connectedClient();
    latest().makeReady();

    vi.advanceTimersByTime(31_000);
    expect(latest().parsedSent().at(-1)).toEqual({ t: "ping" });
  });

  it("끊긴 뒤에는 ping 을 보내지 않는다", () => {
    // 타이머를 정리하지 않으면 죽은 소켓에 계속 쓴다.
    connectedClient();
    latest().makeReady();
    const socket = latest();
    socket.close();
    const sentAfterClose = socket.sent.length;

    vi.advanceTimersByTime(120_000);
    expect(socket.sent).toHaveLength(sentAfterClose);
  });
});
