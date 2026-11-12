/**
 * 락·트랜잭션 탭 — **지금 누가 누구를 막고 있나.**
 *
 * # 왜 Database Insights 로는 안 되는가
 *
 * DI 의 락 분석은 PostgreSQL 전용이다. MySQL 에서는 대기 이벤트와 횟수만 보이고 **쥐고 있는
 * 쪽이 안 보인다** — 블로커는 대개 커밋을 잊은 유휴 세션이라 DB 부하에도 처리목록에도 아무것도
 * 하지 않는 세션(`Sleep`)으로 나온다. 그래서 블로커가 **무엇을 실행했는지**를 함께 보여준다.
 *
 * # 왜 이 탭을 열었을 때만 묻는가
 *
 * 서버가 대상 DB 에 직접 붙는다. 상태 스냅샷이라 사람이 볼 때만 필요하고, 서버가 3초 캐시해
 * 여러 사람이 열어도 연결이 쏟아지지 않는다(`crates/dbmon/src/locks.rs`).
 */

import { useQuery } from "@tanstack/react-query";
import { type ReactNode, useState } from "react";
import { fetchInstanceLocks, queryKeys } from "../lib/api";
import { EMPTY, fmtClock, fmtDuration, fmtInt } from "../lib/format";
import type { LockSnapshot, LockWaitView, MetadataLockView, TransactionView } from "../lib/types";
import { EmptyRow, ErrorNotice, Note, Pending } from "./Notices";
import { SqlModal } from "./SqlModal";
import {
  BADGE_WARN,
  CELL_X,
  COL_GROW,
  COL_TIGHT,
  LINK,
  MONO,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "./ui";

/** 다시 묻는 주기. 서버 캐시(3초)보다 길어야 매번 새 스냅샷을 받는다. */
const REFRESH_MS = 5_000;

interface OpenSql {
  title: string;
  sql: string;
}
type ShowSql = (sql: OpenSql) => void;

export function LocksPanel({ instanceId }: { instanceId: string }) {
  const locks = useQuery({
    queryKey: queryKeys.instanceLocks(instanceId),
    queryFn: ({ signal }) => fetchInstanceLocks(instanceId, signal),
    refetchInterval: REFRESH_MS,
  });
  const [openSql, setOpenSql] = useState<OpenSql | null>(null);

  // **실패하면 앞 스냅샷을 보이지 않는다.** 남겨 두면 이미 풀린 락을 "아직 막고 있다" 로 읽는다.
  if (locks.error !== null) {
    return <ErrorNotice error={locks.error} onRetry={() => void locks.refetch()} />;
  }
  if (locks.data === undefined) return <Pending label="대상 DB 에서 락 상태를 읽는 중…" />;
  const snap = locks.data;

  return (
    <div className="space-y-6">
      <SnapshotNote snap={snap} />
      <LockWaits rows={snap.lock_waits} onShow={setOpenSql} />
      <Transactions rows={snap.transactions} onShow={setOpenSql} />
      <MetadataLocks rows={snap.metadata_locks} onShow={setOpenSql} />
      <Note>
        SQL 은 저장 정책과 무관하게 <strong>항상 마스킹된</strong> 정규 텍스트다. 유휴 세션의 SQL 은
        지금 도는 것이 아니라 <strong>마지막으로 끝낸 문장</strong>이다. 서버가 문장을
        1,024바이트에서 자르고(
        <span className="font-mono">performance_schema_max_sql_text_length</span>
        ), 블로커가 실행한 문장은 스레드당 최근 문장(기본 10개,{" "}
        <span className="font-mono">events_statements_history</span>)까지만 남는다 — 오래 열린
        트랜잭션은 앞 문장이 빠질 수 있다.
      </Note>
      {openSql === null ? null : (
        <SqlModal title={openSql.title} sql={openSql.sql} onClose={() => setOpenSql(null)} />
      )}
    </div>
  );
}

function SnapshotNote({ snap }: { snap: LockSnapshot }) {
  return (
    <Note>
      조회 시각 {fmtClock(snap.captured_at_ms, "KST")} (KST) · {REFRESH_MS / 1000}초마다 다시 묻는다
      — 이 탭이 열려 있을 때만 대상 DB 에 붙는다.
      {snap.truncated ? (
        <strong className="ml-2 text-amber-700">⚠ 조회 상한에 걸려 일부만 보인다.</strong>
      ) : null}
      {snap.sql_withheld > 0 ? (
        <span className="ml-2 text-amber-700">
          ⚠ 마스킹을 믿을 수 없는 SQL {fmtInt(snap.sql_withheld)}건은 숨겼다(잘려서 인용부호가
          닫히지 않은 문장).
        </span>
      ) : null}
    </Note>
  );
}

function LockWaits({ rows, onShow }: { rows: LockWaitView[]; onShow: ShowSql }) {
  return (
    <Section
      title="행 락 대기"
      count={rows.length}
      hint="InnoDB 행 락을 기다리는 세션과, 그 락을 쥔 세션(블로커)이 트랜잭션에서 실행한 문장."
      head={
        <tr>
          <th className={TH_NUM}>대기 PID</th>
          <th className={TH_NUM}>대기</th>
          <th className={TH}>잠긴 곳</th>
          <th className={TH_NUM}>블로커</th>
          <th className={TH}>블로커 계정</th>
          <th className={TH}>블로커 세션</th>
          <th className={TH}>블로커 문장</th>
          <th className={`${TH} ${COL_GROW}`}>대기 중인 SQL</th>
        </tr>
      }
    >
      {rows.length === 0 ? (
        <EmptyRow colSpan={8}>지금 행 락을 기다리는 세션이 없다.</EmptyRow>
      ) : (
        rows.map((w) => (
          <tr key={`${w.waiting_pid}-${w.blocking_pid ?? "?"}`} className={TR}>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>{w.waiting_pid}</td>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>{secs(w.waiting_time_s)}</td>
            <td className={`${TD} ${COL_TIGHT}`}>
              {w.locked_table ?? EMPTY}
              <span className={`ml-2 ${MONO} text-gray-500`}>
                {[w.locked_index, w.lock_mode].filter((v) => v !== null).join(" · ")}
              </span>
            </td>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>{w.blocking_pid ?? EMPTY}</td>
            <td className={`${TD} ${COL_TIGHT}`}>{who(w.blocking_user, w.blocking_host)}</td>
            <td className={`${TD} ${COL_TIGHT}`}>
              <Session
                command={w.blocking_command}
                timeS={w.blocking_time_s}
                idle={w.blocking_idle}
              />
            </td>
            <StatementsCell
              statements={w.blocking_statements}
              title={`블로커 ${w.blocking_pid ?? EMPTY} 의 트랜잭션 문장 (오래된 순)`}
              onShow={onShow}
            />
            <SqlCell sql={w.waiting_sql} title={`대기 ${w.waiting_pid} 의 SQL`} onShow={onShow} />
          </tr>
        ))
      )}
    </Section>
  );
}

function Transactions({ rows, onShow }: { rows: TransactionView[]; onShow: ShowSql }) {
  return (
    <Section
      title="장기·유휴 트랜잭션"
      count={rows.length}
      hint="1초 넘게 열린 트랜잭션, 오래된 순. 유휴인데 행을 잠갔으면 다른 세션을 막고 있을 수 있다."
      head={
        <tr>
          <th className={TH_NUM}>PID</th>
          <th className={TH}>상태</th>
          <th className={TH_NUM}>열린 지</th>
          <th className={TH_NUM}>잠근 행 / 수정 행</th>
          <th className={TH}>계정</th>
          <th className={TH}>DB</th>
          <th className={TH}>세션</th>
          <th className={TH}>문장</th>
          <th className={`${TH} ${COL_GROW}`}>현재/마지막 SQL</th>
        </tr>
      }
    >
      {rows.length === 0 ? (
        <EmptyRow colSpan={9}>1초 넘게 열린 트랜잭션이 없다.</EmptyRow>
      ) : (
        rows.map((t, i) => (
          <tr key={t.pid ?? `unknown-${i}`} className={TR}>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>{t.pid ?? EMPTY}</td>
            <td className={`${TD} ${COL_TIGHT}`}>{t.state ?? EMPTY}</td>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>{secs(t.age_s)}</td>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>
              {fmtInt(t.rows_locked)}
              <span className="text-gray-400"> / </span>
              {fmtInt(t.rows_modified)}
            </td>
            <td className={`${TD} ${COL_TIGHT}`}>{who(t.user, t.host)}</td>
            <td className={`${TD} ${COL_TIGHT}`}>{t.db ?? EMPTY}</td>
            <td className={`${TD} ${COL_TIGHT}`}>
              <Session command={t.command} timeS={t.time_s} idle={t.idle} />
            </td>
            <StatementsCell
              statements={t.statements}
              title={`PID ${t.pid ?? EMPTY} 의 트랜잭션 문장 (오래된 순)`}
              onShow={onShow}
            />
            <SqlCell sql={t.current_sql} title={`PID ${t.pid ?? EMPTY} 의 SQL`} onShow={onShow} />
          </tr>
        ))
      )}
    </Section>
  );
}

function MetadataLocks({ rows, onShow }: { rows: MetadataLockView[]; onShow: ShowSql }) {
  return (
    <Section
      title="메타데이터 락 대기"
      count={rows.length}
      hint="대기(PENDING)가 걸린 테이블의 MDL 전부 — 보유자는 대개 DDL 앞에서 트랜잭션을 열어 둔 유휴 세션이다."
      head={
        <tr>
          <th className={TH}>테이블</th>
          <th className={TH}>락</th>
          <th className={TH}>상태</th>
          <th className={TH_NUM}>PID</th>
          <th className={TH}>계정</th>
          <th className={TH}>세션</th>
          <th className={`${TH} ${COL_GROW}`}>SQL</th>
        </tr>
      }
    >
      {rows.length === 0 ? (
        <EmptyRow colSpan={7}>메타데이터 락을 기다리는 테이블이 없다.</EmptyRow>
      ) : (
        rows.map((m, i) => (
          <tr key={`${m.table ?? "?"}-${m.pid ?? "?"}-${m.lock_type ?? "?"}-${i}`} className={TR}>
            <td className={`${TD} ${COL_TIGHT}`}>{m.table ?? EMPTY}</td>
            <td className={`${TD} ${COL_TIGHT}`}>
              <span className={MONO}>{m.lock_type ?? EMPTY}</span>
              {m.lock_duration === null ? null : (
                <span className={`ml-2 ${MONO} text-gray-500`}>{m.lock_duration}</span>
              )}
            </td>
            <td className={`${TD} ${COL_TIGHT}`}>
              {m.pending ? <WarnBadge>대기</WarnBadge> : (m.lock_status ?? EMPTY)}
            </td>
            <td className={`${TD_NUM} ${COL_TIGHT}`}>{m.pid ?? EMPTY}</td>
            <td className={`${TD} ${COL_TIGHT}`}>{who(m.user, m.host)}</td>
            <td className={`${TD} ${COL_TIGHT}`}>
              <Session command={m.command} timeS={m.time_s} idle={m.command === "Sleep"} />
            </td>
            <SqlCell sql={m.sql} title={`PID ${m.pid ?? EMPTY} 의 SQL`} onShow={onShow} />
          </tr>
        ))
      )}
    </Section>
  );
}

interface SectionProps {
  title: string;
  count: number;
  hint: string;
  head: ReactNode;
  children: ReactNode;
}

function Section({ title, count, hint, head, children }: SectionProps) {
  return (
    <section>
      <h3 className="text-sm font-bold text-gray-900">
        {title} <span className="font-normal text-gray-600">{fmtInt(count)}건</span>
      </h3>
      <p className="mt-0.5 mb-2 text-xs text-gray-600">{hint}</p>
      <div className="overflow-x-auto">
        <table className={TABLE}>
          <thead>{head}</thead>
          <tbody className={TBODY}>{children}</tbody>
        </table>
      </div>
    </section>
  );
}

/** 세션 상태와 그 상태로 지난 시간. 유휴는 배지로 — **색만으로 말하지 않는다.** */
function Session({
  command,
  timeS,
  idle,
}: {
  command: string | null;
  timeS: number | null;
  idle: boolean;
}) {
  return (
    <span className="inline-flex items-center gap-1.5">
      {idle ? (
        <WarnBadge title="문장은 끝났는데 커밋하지 않았다 — 트랜잭션과 락을 계속 쥔다">
          유휴
        </WarnBadge>
      ) : null}
      <span className={MONO}>{command ?? EMPTY}</span>
      <span className="text-gray-500">{secs(timeS)}</span>
    </span>
  );
}

function WarnBadge({ title, children }: { title?: string; children: ReactNode }) {
  return (
    <span className={BADGE_WARN} title={title}>
      {children}
    </span>
  );
}

/** 한 줄 SQL. 누르면 전문이 뜬다. */
function SqlCell({ sql, title, onShow }: { sql: string | null; title: string; onShow: ShowSql }) {
  return (
    // `max-w-0` 이 있어야 긴 SQL 이 표를 밀어내지 않고 잘린다(슬로우 쿼리 표와 같다).
    <td className={`${COL_GROW} ${CELL_X} max-w-0 py-2 text-sm text-gray-700`}>
      {sql === null ? (
        <span className="text-gray-500">{EMPTY}</span>
      ) : (
        <button
          type="button"
          className={`block w-full truncate text-left ${MONO} ${LINK}`}
          onClick={() => onShow({ title, sql })}
        >
          {sql}
        </button>
      )}
    </td>
  );
}

function StatementsCell({
  statements,
  title,
  onShow,
}: {
  statements: string[];
  title: string;
  onShow: ShowSql;
}) {
  return (
    <td className={`${TD} ${COL_TIGHT}`}>
      {statements.length === 0 ? (
        <span className="text-gray-500" title="이력에 남은 문장이 없다">
          {EMPTY}
        </span>
      ) : (
        <button
          type="button"
          className={LINK}
          onClick={() => onShow({ title, sql: statements.join(";\n") })}
        >
          문장 {fmtInt(statements.length)}개
        </button>
      )}
    </td>
  );
}

function secs(s: number | null): string {
  return fmtDuration(s === null ? null : s * 1000);
}

function who(user: string | null, host: string | null): string {
  if (user === null) return EMPTY;
  return host === null ? user : `${user}@${host}`;
}
