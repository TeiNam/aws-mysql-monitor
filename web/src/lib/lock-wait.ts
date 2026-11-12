/**
 * 슬로우 쿼리가 **락 대기의 피해자인가.**
 *
 * 느린 이유가 쿼리 자체가 아니라 다른 세션이 쥔 락이면, 인덱스·플랜을 들여다봐도 답이 없다.
 * 그래서 목록에서 먼저 가른다. 근거는 둘이다.
 *
 * 1. **블로커를 관측했다** (`lock_wait`) — 수집기가 실행 중에 대기를 직접 봤다.
 * 2. **끝난 뒤의 `LOCK_TIME` 이 실행 시간의 절반 이상이다.** 행 락 대기는 문장이 끝나야
 *    더해지므로 끝난 값만 온다. MySQL 8.0.28 이전에는 행 락 대기를 담지 않아 이 근거로는
 *    놓칠 수 있다(거짓 양성은 아니다).
 */

import { fmtDuration } from "./format";
import type { SlowQueryView } from "./types";

/** 실행 시간 중 락 대기가 이 비율 이상이면 피해자로 본다. */
export const LOCK_VICTIM_RATIO = 0.5;

/** 피해자면 그 근거 한 줄, 아니면 `null`. */
export function lockWaitReason(q: SlowQueryView): string | null {
  const w = q.lock_wait;
  if (w !== null && w !== undefined) {
    const who = w.blocking_user === null ? "" : ` ${w.blocking_user}`;
    return `행 락을 기다리는 중에 관측됐다 — 블로커 PID ${w.blocking_pid ?? "?"}${who} (${w.blocking_command ?? "?"})`;
  }
  const lock = q.lock_time_ms;
  if (lock === null || q.duration_ms <= 0 || lock < q.duration_ms * LOCK_VICTIM_RATIO) return null;
  const pct = Math.round((lock / q.duration_ms) * 100);
  return `실행 시간의 ${pct}% 가 락 대기다 (${fmtDuration(lock)} / ${fmtDuration(q.duration_ms)})`;
}
