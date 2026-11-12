import { describe, expect, it } from "vitest";
import { lockWaitReason } from "./lock-wait";
import type { SlowQueryView } from "./types";

const base: SlowQueryView = {
  record_id: "i:1:1",
  instance_id: "i",
  env: "dev",
  state: "finalized",
  thread_id: 1,
  started_at_ms: 0,
  duration_ms: 10_000,
  duration_source: "slowlog",
  last_seen_at_ms: null,
  capture_source: "merged",
  app_digest: "d",
  statement_type: "update",
  schema_name: "shop",
  db_user: "app",
  sql_text: null,
  sql_redacted_reason: "not_stored",
  sql_text_truncated: false,
  rows_examined: 1,
  rows_sent: 0,
  lock_time_ms: null,
  has_plan: false,
  abandoned_reason: null,
};

describe("lockWaitReason", () => {
  it("락 시간이 실행 시간의 절반 이상이면 피해자다", () => {
    expect(lockWaitReason({ ...base, lock_time_ms: 5_000 })).toContain("50%");
    expect(lockWaitReason({ ...base, lock_time_ms: 4_999 })).toBeNull();
  });

  /** **모르는 것을 0 으로 읽지 않는다** — 실행 중 레코드는 `LOCK_TIME` 이 없다. */
  it("락 시간을 모르면 판정하지 않는다", () => {
    expect(lockWaitReason({ ...base, lock_time_ms: null })).toBeNull();
    expect(lockWaitReason({ ...base, duration_ms: 0, lock_time_ms: 0 })).toBeNull();
  });

  it("블로커를 관측했으면 락 시간과 무관하게 피해자다", () => {
    const reason = lockWaitReason({
      ...base,
      state: "inflight",
      lock_wait: {
        observed_at_ms: 1,
        locked_table: "shop.orders",
        locked_index: "PRIMARY",
        lock_mode: "X",
        blocking_pid: 31,
        blocking_user: "app",
        blocking_host: null,
        blocking_command: "Sleep",
        blocking_time_s: 40,
        blocking_statements: [],
      },
    });
    expect(reason).toContain("PID 31");
    expect(reason).toContain("Sleep");
  });
});
