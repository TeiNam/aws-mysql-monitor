/**
 * 락 대기 피해자 표시 — **느린 이유가 쿼리가 아니라 다른 세션의 락이다.**
 *
 * 그때는 인덱스·플랜을 고쳐도 소용없고, 막고 있던 세션(대개 커밋을 잊은 유휴 트랜잭션)을
 * 찾아야 한다. 판정 근거는 [`lockWaitReason`] 이다.
 */

import type { ReactNode } from "react";
import { Link } from "react-router";
import { EMPTY, fmtDuration } from "../lib/format";
import { lockWaitReason } from "../lib/lock-wait";
import { formatSql } from "../lib/sql";
import type { SlowQueryView } from "../lib/types";
import { BADGE_WARN, LINK } from "./ui";

/** 목록의 표시. 근거는 `title` 로 — 색과 글자가 함께 간다. */
export function LockWaitBadge({ query }: { query: SlowQueryView }) {
  const reason = lockWaitReason(query);
  if (reason === null) return null;
  return (
    <span className={`ml-1 ${BADGE_WARN}`} title={reason}>
      락 대기
    </span>
  );
}

/** 상세. 피해자가 아니면 그리지 않는다. */
export function LockWaitDetail({ query }: { query: SlowQueryView }) {
  const reason = lockWaitReason(query);
  if (reason === null) return null;
  const w = query.lock_wait ?? null;
  const locksTab = `/instance?instance=${encodeURIComponent(query.instance_id)}&tab=locks`;

  return (
    <section className="mt-4 rounded-md border border-amber-200 bg-amber-50/50 p-3">
      <h4 className="text-sm font-medium text-amber-900">락 대기</h4>
      <p className="mt-1 text-xs text-amber-800">{reason}</p>
      {w === null ? (
        <p className="mt-1 text-xs text-gray-600">
          블로커는 관측하지 못했다 — 대기가 실행 중 관측 사이에 끝났다.
        </p>
      ) : (
        <>
          <dl className="mt-2 grid grid-cols-1 gap-x-6 gap-y-1 text-xs md:grid-cols-3">
            <Pair label="잠긴 곳">
              {w.locked_table ?? EMPTY}{" "}
              <span className="font-mono text-gray-500">
                {[w.locked_index, w.lock_mode].filter((v) => v !== null).join(" · ")}
              </span>
            </Pair>
            <Pair label="블로커">
              PID {w.blocking_pid ?? EMPTY}
              {w.blocking_user === null ? null : ` · ${w.blocking_user}`}
              {w.blocking_host === null ? null : `@${w.blocking_host}`}
            </Pair>
            <Pair label="블로커 세션">
              <span className="font-mono">{w.blocking_command ?? EMPTY}</span>{" "}
              {fmtDuration(w.blocking_time_s === null ? null : w.blocking_time_s * 1000)}
            </Pair>
          </dl>
          {w.blocking_statements.length === 0 ? (
            <p className="mt-2 text-xs text-gray-600">
              블로커가 실행한 문장은 남지 않았다 — 정책이 SQL 을 저장하지 않거나 이력에서 밀려났다.
            </p>
          ) : (
            <>
              <p className="mt-2 text-xs text-gray-600">블로커 트랜잭션이 실행한 문장 (오래된 순)</p>
              <pre className="mt-1 overflow-x-auto rounded bg-white p-2 font-mono text-xs whitespace-pre-wrap text-gray-800">
                {formatSql(w.blocking_statements.join(";\n"))}
              </pre>
            </>
          )}
        </>
      )}
      <p className="mt-2 text-xs">
        <Link to={locksTab} className={LINK}>
          지금 막고 있는 세션은 락 탭에서 본다 →
        </Link>
      </p>
    </section>
  );
}

function Pair({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt className="text-gray-500">{label}</dt>
      <dd className="text-gray-800">{children}</dd>
    </div>
  );
}
