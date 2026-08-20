import { describe, expect, it } from "vitest";
import {
  HISTORY_LEN,
  INITIAL_SNAPSHOT,
  MAX_LIVE_ROWS,
  applyMessage,
  parseServerMessage,
  type LiveSnapshot,
} from "./live-reduce";
import type { LiveMetrics, ServerMessage, SlowQueryBroadcast } from "./types";

const NOW = 1_700_000_000_000;

function broadcast(overrides: Partial<SlowQueryBroadcast> = {}): SlowQueryBroadcast {
  return {
    record_id: "000000000000/ap-northeast-2/mysql84-local:12:1700000000",
    instance_id: "000000000000/ap-northeast-2/mysql84-local",
    env: "dev",
    state: "inflight",
    started_at_ms: NOW,
    duration_ms: 1376,
    app_digest: "d1",
    statement_type: "select",
    schema_name: "shop",
    sql_preview: "SELECT sleep ( ? )",
    ...overrides,
  };
}

function metrics(overrides: Partial<LiveMetrics> = {}): LiveMetrics {
  return {
    at_ms: NOW,
    qps: 8.98,
    slow_per_sec: 0,
    threads_running: 3,
    threads_connected: 5,
    lock_waits: 0,
    rate_gap_reason: null,
    ...overrides,
  };
}

function apply(snapshot: LiveSnapshot, ...msgs: ServerMessage[]): LiveSnapshot {
  return msgs.reduce((acc, msg) => applyMessage(acc, msg, NOW), snapshot);
}

describe("슬로우 쿼리 upsert", () => {
  it("같은 record_id 를 덮어쓰고 자리를 옮기지 않는다", () => {
    // `finalized` 가 두 번 오는 것이 관측된 동작이고, 뒤엣것이 정확한 값이다
    // (docs/21-resume.md §3.1). 행이 매번 맨 위로 튀면 표를 읽을 수 없다.
    const older = broadcast({ record_id: "a:1:1", started_at_ms: NOW - 1000 });
    const newer = broadcast({ record_id: "b:2:2" });
    const s = apply(
      INITIAL_SNAPSHOT,
      { t: "slowq", data: older },
      { t: "slowq", data: newer },
      { t: "slowq", data: { ...older, state: "finalized", duration_ms: 5587 } },
      { t: "slowq", data: { ...older, state: "finalized", duration_ms: 7002 } },
    );

    expect(s.slowq).toHaveLength(2);
    // 새 행이 위로 오고, 갱신은 제자리에서 일어난다.
    expect(s.slowq[0]?.record_id).toBe("b:2:2");
    expect(s.slowq[1]?.record_id).toBe("a:1:1");
    expect(s.slowq[1]?.duration_ms).toBe(7002);
    expect(s.slowq[1]?.state).toBe("finalized");
  });

  it("상한을 넘으면 오래된 것부터 버린다", () => {
    const messages: ServerMessage[] = Array.from({ length: MAX_LIVE_ROWS + 10 }, (_, i) => ({
      t: "slowq",
      data: broadcast({ record_id: `r:${i}:0` }),
    }));
    const s = apply(INITIAL_SNAPSHOT, ...messages);

    expect(s.slowq).toHaveLength(MAX_LIVE_ROWS);
    // 마지막에 들어온 것이 맨 위, 가장 오래된 것은 사라졌다.
    expect(s.slowq[0]?.record_id).toBe(`r:${MAX_LIVE_ROWS + 9}:0`);
    expect(s.slowq.some((r) => r.record_id === "r:0:0")).toBe(false);
  });
});

describe("실시간 지표", () => {
  it("인스턴스별 최신값과 이력을 쌓고 null 을 0 으로 바꾸지 않는다", () => {
    const inst = "000000000000/ap-northeast-2/mysql84-local";
    const s = apply(
      INITIAL_SNAPSHOT,
      { t: "status", instance_id: inst, metrics: metrics({ qps: null, rate_gap_reason: "first_sample" }) },
      { t: "status", instance_id: inst, metrics: metrics({ qps: 13.4 }) },
    );

    expect(s.status[inst]?.qps).toBe(13.4);
    // 첫 표본의 `null` 이 이력에 그대로 남아야 스파크라인이 없는 구간을 잇지 않는다.
    expect(s.qpsHistory[inst]).toEqual([null, 13.4]);
  });

  it("이력이 상한을 넘으면 오래된 표본을 버린다", () => {
    const inst = "i";
    const messages: ServerMessage[] = Array.from({ length: HISTORY_LEN + 5 }, (_, i) => ({
      t: "status",
      instance_id: inst,
      metrics: metrics({ qps: i }),
    }));
    const s = apply(INITIAL_SNAPSHOT, ...messages);

    expect(s.qpsHistory[inst]).toHaveLength(HISTORY_LEN);
    expect(s.qpsHistory[inst]?.[HISTORY_LEN - 1]).toBe(HISTORY_LEN + 4);
  });
});

describe("연결 상태", () => {
  it("unauthorized 는 재접속을 막는 상태로 남는다", () => {
    const s = apply(INITIAL_SNAPSHOT, { t: "error", code: "unauthorized" });
    expect(s.conn).toBe("unauthorized");
    expect(s.user).toBeNull();
  });

  it("stream_lagged 는 재조회 신호를 남긴다", () => {
    // 유실된 방송은 다시 오지 않는다. 화면이 HTTP 로 다시 읽어야 한다.
    const s = apply(INITIAL_SNAPSHOT, { t: "error", code: "stream_lagged" });
    expect(s.laggedAt).toBe(NOW);
  });

  it("거부된 토픽을 버리지 않는다", () => {
    const s = apply(INITIAL_SNAPSHOT, {
      t: "subscribed",
      topics: ["slowq:env=dev"],
      denied: ["slowq:env=prd"],
    });
    expect(s.denied).toEqual(["slowq:env=prd"]);
  });

  it("pong 은 같은 객체를 돌려준다", () => {
    // 참조가 바뀌면 `useSyncExternalStore` 가 30초마다 전체를 리렌더한다.
    const s = applyMessage(INITIAL_SNAPSHOT, { t: "pong" }, NOW);
    expect(s).toBe(INITIAL_SNAPSHOT);
  });
});

describe("프레임 파싱", () => {
  it("규정된 형태를 통과시킨다", () => {
    expect(parseServerMessage('{"t":"pong"}')).toEqual({ t: "pong" });
    expect(parseServerMessage('{"t":"error","code":"malformed"}')).toEqual({
      t: "error",
      code: "malformed",
    });
    expect(
      parseServerMessage('{"t":"subscribed","topics":["slowq:env=dev"],"denied":[]}'),
    ).not.toBeNull();
  });

  it("모르는 형태와 깨진 프레임을 버린다", () => {
    for (const raw of [
      "not json",
      "[]",
      "null",
      '{"t":"unknown"}',
      '{"t":"slowq"}',
      '{"t":"slowq","data":{"record_id":"a"}}', // duration_ms 없음
      '{"t":"status","instance_id":"i"}', // metrics 없음
      '{"t":"subscribed","topics":"dev","denied":[]}',
      '{"t":"error"}',
    ]) {
      expect(parseServerMessage(raw), raw).toBeNull();
    }
  });
});
