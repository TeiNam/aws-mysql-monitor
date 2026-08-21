import { describe, expect, it } from "vitest";
import {
  HISTORY_LEN,
  INITIAL_SNAPSHOT,
  MAX_LIVE_ROWS,
  applyMessage,
  forgetDenied,
  markStreamGap,
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
    max_connections: 100,
    lock_waits: 0,
    rate_gap_reason: null,
    ...overrides,
  };
}

function apply(snapshot: LiveSnapshot, ...msgs: ServerMessage[]): LiveSnapshot {
  return msgs.reduce((acc, msg) => applyMessage(acc, msg), snapshot);
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

  it("누적 수신 수는 상한에서 멈추지 않는다", () => {
    // 화면은 이 값으로 "새 쿼리가 왔다" 를 감지한다. 배열 길이를 쓰면 상한에
    // 닿는 순간 신호가 죽어 재조회가 영구히 멈춘다.
    const messages: ServerMessage[] = Array.from({ length: MAX_LIVE_ROWS + 10 }, (_, i) => ({
      t: "slowq",
      data: broadcast({ record_id: `r:${i}:0` }),
    }));
    const s = apply(INITIAL_SNAPSHOT, ...messages);
    expect(s.slowq).toHaveLength(MAX_LIVE_ROWS);
    expect(s.slowqSeen).toBe(MAX_LIVE_ROWS + 10);

    // 같은 레코드의 갱신도 "받았다" 로 센다 — 상태 전이(진행 중 → 확정)도 새 정보다.
    const again = applyMessage(s, { t: "slowq", data: broadcast({ record_id: "r:0:0" }) });
    expect(again.slowqSeen).toBe(MAX_LIVE_ROWS + 11);
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

  it("연결이 끊기면 이력에 구멍을 남긴다", () => {
    // 이게 없으면 10분 뒤 재연결한 첫 표본이 끊기기 전 표본과 선으로 이어져
    // **관측이 없던 구간에 선이 그려진다.**
    const inst = "i";
    const connected = apply(INITIAL_SNAPSHOT, {
      t: "status",
      instance_id: inst,
      metrics: metrics({ qps: 10 }),
    });
    const dropped = markStreamGap(connected);
    expect(dropped.qpsHistory[inst]).toEqual([10, null]);

    // 재접속을 여러 번 실패해도 `null` 로만 채워 데이터를 밀어내지 않는다.
    expect(markStreamGap(dropped).qpsHistory[inst]).toEqual([10, null]);
  });

  it("표본이 빈 구간을 시각으로 감지해 구멍을 넣는다", () => {
    // 서버는 `status` 밀림을 조용히 버린다. 표본이 사라진 것을 이력에서 알 수
    // 없으면 선이 관측되지 않은 시간을 건너뛴다.
    const inst = "i";
    const s = apply(
      INITIAL_SNAPSHOT,
      { t: "status", instance_id: inst, metrics: metrics({ at_ms: NOW, qps: 10 }) },
      // 5초 뒤 — 정상 간격
      { t: "status", instance_id: inst, metrics: metrics({ at_ms: NOW + 5_000, qps: 11 }) },
      // 1분 뒤 — 표본 열 개가 사라졌다
      { t: "status", instance_id: inst, metrics: metrics({ at_ms: NOW + 65_000, qps: 12 }) },
    );
    expect(s.qpsHistory[inst]).toEqual([10, 11, null, 12]);
  });

  it("순서가 뒤집힌 표본도 구멍으로 본다", () => {
    const inst = "i";
    const s = apply(
      INITIAL_SNAPSHOT,
      { t: "status", instance_id: inst, metrics: metrics({ at_ms: NOW, qps: 10 }) },
      { t: "status", instance_id: inst, metrics: metrics({ at_ms: NOW - 5_000, qps: 11 }) },
    );
    expect(s.qpsHistory[inst]).toEqual([10, null, 11]);
  });

  it("이력이 없어도 놓쳤을 수 있다는 표시는 남긴다", () => {
    // 슬로우 쿼리 방송은 지표와 무관하게 흐르고 있었다.
    const gapped = markStreamGap(INITIAL_SNAPSHOT);
    expect(gapped.gapPending).toBe(true);
    // 이미 표시돼 있으면 같은 객체 — 헛된 리렌더를 만들지 않는다.
    expect(markStreamGap(gapped)).toBe(gapped);
  });

  it("놓침은 끊긴 시점이 아니라 다시 붙은 시점에 센다", () => {
    // 끊긴 순간에 세면 화면이 그때 HTTP 재조회를 하고, 그 조회는 서버가 죽어
    // 있는 동안 나가서 실패한다. 그리고 재접속에는 신호가 없어 목록이 낡는다.
    const dropped = markStreamGap(
      apply(INITIAL_SNAPSHOT, {
        t: "status",
        instance_id: "i",
        metrics: metrics({ qps: 10 }),
      }),
    );
    expect(dropped.missedCount).toBe(0);
    expect(dropped.gapPending).toBe(true);

    const back = applyMessage(dropped, {
      t: "ready",
      user: { subject: "u", role: "admin", env_scope: ["dev"], can_see_literals: false },
    });
    expect(back.missedCount).toBe(1);
    expect(back.gapPending).toBe(false);

    // 끊김 없이 다시 `ready` 를 받아도 더 세지 않는다.
    const again = applyMessage(back, {
      t: "ready",
      user: { subject: "u", role: "admin", env_scope: ["dev"], can_see_literals: false },
    });
    expect(again.missedCount).toBe(1);
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
  it("unauthorized 는 재접속을 막고 남은 데이터를 버린다", () => {
    // 백엔드는 fail closed 다. 화면만 옛 데이터를 들고 있으면 강등된 사용자가
    // 그걸 계속 본다.
    const filled = apply(
      INITIAL_SNAPSHOT,
      { t: "slowq", data: broadcast() },
      { t: "status", instance_id: "i", metrics: metrics() },
      { t: "error", code: "stream_lagged" },
    );
    const s = applyMessage(filled, { t: "error", code: "unauthorized" });

    expect(s.conn).toBe("unauthorized");
    expect(s.user).toBeNull();
    expect(s.slowq).toEqual([]);
    expect(s.status).toEqual({});
    expect(s.qpsHistory).toEqual({});
    // 재조회 신호는 유지한다 — 지우면 목록이 갱신되지 않는다.
    expect(s.missedCount).toBe(1);
  });

  it("거부된 토픽은 뒤에 온 성공 응답이 지우지 않는다", () => {
    // 서버는 매 응답에 그 요청의 거부만 담는다. 그대로 덮으면 경고가 사라지고
    // 사용자는 "왜 이 인스턴스 지표만 없나" 를 코드에서 찾게 된다.
    const s = apply(
      INITIAL_SNAPSHOT,
      { t: "subscribed", topics: ["slowq:env=dev"], denied: ["status:inst=x"] },
      { t: "subscribed", topics: ["slowq:env=dev", "slowq:env=stg"], denied: [] },
    );
    expect(s.denied).toEqual(["status:inst=x"]);

    // 나중에 실제로 구독되면 목록에서 빠진다.
    const after = applyMessage(s, {
      t: "subscribed",
      topics: ["slowq:env=dev", "status:inst=x"],
      denied: [],
    });
    expect(after.denied).toEqual([]);
  });

  it("새 연결(ready)은 거부 목록을 비운다", () => {
    const s = apply(INITIAL_SNAPSHOT, {
      t: "subscribed",
      topics: [],
      denied: ["slowq:env=prd"],
    });
    const reconnected = applyMessage(s, {
      t: "ready",
      user: { subject: "u", role: "admin", env_scope: ["prd"], can_see_literals: true },
    });
    expect(reconnected.denied).toEqual([]);
  });

  it("stream_lagged 는 재조회 신호를 셈으로 남긴다", () => {
    // 유실된 방송은 다시 오지 않는다. 화면이 HTTP 로 다시 읽어야 한다.
    // 같은 밀리초에 두 번 밀려도 두 번으로 세져야 재조회가 빠지지 않는다.
    const s = apply(
      INITIAL_SNAPSHOT,
      { t: "error", code: "stream_lagged" },
      { t: "error", code: "stream_lagged" },
    );
    expect(s.missedCount).toBe(2);
    // 스스로 복구되는 상황이므로 오류 배너를 띄우지 않는다.
    expect(s.errorCode).toBeNull();
  });

  it("프로토콜 오류 코드를 삼키지 않는다", () => {
    const s = apply(INITIAL_SNAPSHOT, { t: "error", code: "malformed" });
    expect(s.errorCode).toBe("malformed");
  });

  it("거부된 토픽을 버리지 않는다", () => {
    const s = apply(INITIAL_SNAPSHOT, {
      t: "subscribed",
      topics: ["slowq:env=dev"],
      denied: ["slowq:env=prd"],
    });
    expect(s.denied).toEqual(["slowq:env=prd"]);
  });

  it("요구를 거둔 토픽은 거부 목록에서 빠진다", () => {
    // 실시간 화면을 떠난 뒤에도 배너가 남으면, 지금 요구하지도 않는 토픽 얘기를
    // 재접속까지 하고 있게 된다.
    const s = apply(INITIAL_SNAPSHOT, {
      t: "subscribed",
      topics: [],
      denied: ["slowq:env=prd", "status:inst=x"],
    });
    const after = forgetDenied(s, ["slowq:env=prd"]);
    expect(after.denied).toEqual(["status:inst=x"]);
    // 바뀔 것이 없으면 같은 객체를 돌려준다(헛된 리렌더 방지).
    expect(forgetDenied(after, ["없는-토픽"])).toBe(after);
  });

  it("pong 은 같은 객체를 돌려준다", () => {
    // 참조가 바뀌면 `useSyncExternalStore` 가 30초마다 전체를 리렌더한다.
    const s = applyMessage(INITIAL_SNAPSHOT, { t: "pong" });
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

  it("slowq 의 화면이 읽는 필드가 타입까지 맞아야 한다", () => {
    // `sql_preview` 가 객체면 React 가 죽고, `started_at_ms` 가 없으면 `Intl` 이
    // 조용히 **현재 시각**을 그린다.
    expect(
      parseServerMessage(
        '{"t":"slowq","data":{"record_id":"a","instance_id":"i","duration_ms":1}}',
      ),
      "started_at_ms 없음",
    ).toBeNull();

    const msg = parseServerMessage(
      '{"t":"slowq","data":{"record_id":"a","instance_id":"i","duration_ms":1,' +
        '"started_at_ms":2,"env":"nope","sql_preview":{"x":1},"statement_type":null}}',
    );
    expect(msg).toEqual({
      t: "slowq",
      data: {
        record_id: "a",
        instance_id: "i",
        env: "unknown",
        state: "unknown",
        started_at_ms: 2,
        duration_ms: 1,
        app_digest: "",
        statement_type: "unknown",
        schema_name: null,
        sql_preview: null,
      },
    });
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
      '{"t":"status","instance_id":"i","metrics":{}}', // at_ms 없음 → 1970년을 그리지 않는다
      '{"t":"subscribed","topics":"dev","denied":[]}',
      '{"t":"error"}',
    ]) {
      expect(parseServerMessage(raw), raw).toBeNull();
    }
  });

  /**
   * **화면이 읽는 필드가 없으면 프레임을 버린다.** `env_scope` 가 없으면 헤더가
   * `undefined.join()` 으로 죽고, 그러면 프레임 하나가 화면 전체를 날린다.
   */
  it("ready 에 화면이 읽는 필드가 빠지면 버린다", () => {
    const full = {
      t: "ready",
      user: { subject: "u", role: "viewer", env_scope: ["dev"], can_see_literals: false },
    };
    expect(parseServerMessage(JSON.stringify(full))).not.toBeNull();

    for (const missing of ["subject", "role", "env_scope", "can_see_literals"]) {
      const user: Record<string, unknown> = { ...full.user };
      delete user[missing];
      expect(parseServerMessage(JSON.stringify({ t: "ready", user })), missing).toBeNull();
    }
  });

  it("지표의 숫자가 아닌 값은 null 로 정규화한다", () => {
    // `undefined` 가 산술에 들어가면 `NaN` 이 되고, 차트에 구멍이 아니라
    // 쓰레기가 그려진다.
    const msg = parseServerMessage(
      '{"t":"status","instance_id":"i","metrics":{"at_ms":1,"qps":"9","threads_running":3}}',
    );
    expect(msg).toEqual({
      t: "status",
      instance_id: "i",
      metrics: {
        at_ms: 1,
        qps: null,
        slow_per_sec: null,
        threads_running: 3,
        threads_connected: null,
    max_connections: null,
        lock_waits: null,
        rate_gap_reason: null,
      },
    });
  });
});
