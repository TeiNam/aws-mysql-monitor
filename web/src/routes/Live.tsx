import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { Link, useSearchParams } from "react-router";
import { StateBadge } from "../components/Badges";
import { ErrorNotice, Note } from "../components/Notices";
import { CARD, LINK, MONO, ROW, TABLE, TD, TD_NUM, TH, TH_NUM } from "../components/styles";
import { useLive, useLiveTopics } from "../hooks/useLive";
import { fetchInstances, queryKeys } from "../lib/api";
import { EMPTY, fmtClock, fmtDuration, shortInstance } from "../lib/format";
import { MAX_LIVE_ROWS } from "../lib/live-reduce";
import type { SlowQueryBroadcast } from "../lib/types";

/**
 * 실시간 스트림. WS `slowq:env=…`.
 *
 * # 왜 목록 화면과 따로 있는가
 *
 * 방송 페이로드는 목록 응답보다 **필드가 적다**(T-22: 원문 리터럴을 담지 않는다).
 * 같은 표에 섞으면 조사 행·락 시간 칸이 이유 없이 비고, 그게 "데이터가 없다" 로
 * 읽힌다. 여기서는 방송이 실제로 주는 것만 보여 준다.
 */
export function Live() {
  const [params, setParams] = useSearchParams();
  const env = params.get("env") ?? "";

  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });

  // 구독할 환경을 **등록부에서 얻는다.** 목록을 손으로 적으면 없는 환경을 구독해
  // `denied` 만 받는다.
  const envOptions = useMemo(
    () => [...new Set((instances.data ?? []).map((i) => i.env))].sort(),
    [instances.data],
  );
  const topics = useMemo(
    () => (env === "" ? envOptions : [env]).map((e) => `slowq:env=${e}`),
    [env, envOptions],
  );
  useLiveTopics(topics);

  const live = useLive();
  const rows = env === "" ? live.slowq : live.slowq.filter((r) => r.env === env);

  return (
    <section>
      <div className="mb-4 flex flex-wrap items-end justify-between gap-3">
        <h1 className="text-lg font-semibold tracking-tight">
          실시간
          <span className="ml-2 text-sm font-normal text-zinc-500">
            {rows.length}건 수신
          </span>
        </h1>
        <label className="flex flex-col gap-1 text-xs text-zinc-500">
          환경
          <select
            className="rounded border border-zinc-700 bg-zinc-900 px-2 py-1.5 text-sm text-zinc-100 focus:border-sky-500 focus:outline-none"
            value={env}
            onChange={(e) => {
              const next = new URLSearchParams(params);
              if (e.target.value === "") next.delete("env");
              else next.set("env", e.target.value);
              setParams(next, { replace: true });
            }}
          >
            <option value="">전체</option>
            {envOptions.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
      </div>

      {/* 등록부 조회가 실패하면 구독할 환경을 모른다 — 조용히 빈 표를 두지 않는다. */}
      {instances.error === null ? null : (
        <div className="mb-4">
          <ErrorNotice error={instances.error} onRetry={() => void instances.refetch()} />
        </div>
      )}

      <div className={`${CARD} overflow-x-auto`}>
        <table className={TABLE}>
          <thead>
            <tr>
              <th className={TH}>시작</th>
              <th className={TH}>인스턴스</th>
              <th className={TH}>상태</th>
              <th className={TH_NUM}>실행시간</th>
              <th className={TH}>유형</th>
              <th className={TH}>스키마</th>
              <th className={TH}>SQL (마스킹)</th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td className={`${TD} text-zinc-500`} colSpan={7}>
                  {/*
                    **구독하고 있지 않은데 "구독 중" 이라고 말하지 않는다.**
                    구독할 환경은 등록부에서 얻으므로, 등록부 조회가 실패하면
                    토픽이 하나도 없고 스트림은 영구히 비어 있다.
                  */}
                  {topics.length === 0
                    ? instances.isPending
                      ? "구독할 환경을 확인하는 중…"
                      : "구독한 토픽이 없다 — 등록부에서 환경을 얻지 못했다."
                    : live.conn === "open"
                      ? "구독 중이다. 임계값을 넘는 쿼리가 실행되면 여기 나타난다."
                      : "연결되면 여기에 실시간으로 쌓인다."}
                </td>
              </tr>
            ) : (
              rows.map((row) => <LiveRow key={row.record_id} row={row} />)
            )}
          </tbody>
        </table>
      </div>

      <Note>
        같은 쿼리가 <strong>진행 중 → 확정</strong> 으로 갱신된다(같은{" "}
        <span className="font-mono">record_id</span> 를 덮어쓴다). 정렬은 시작 시각이 아니라{" "}
        <strong>수신 순서</strong>다 — 갱신 때마다 행이 튀지 않게 자리를 지킨다. 화면은 최근{" "}
        {MAX_LIVE_ROWS}건만 유지하고, 그 이전 것은 목록 화면에서 조회한다.
      </Note>
    </section>
  );
}

function LiveRow({ row }: { row: SlowQueryBroadcast }) {
  return (
    <tr className={ROW}>
      <td className={`${TD} whitespace-nowrap`}>
        <Link className={LINK} to={`/slow-queries/${encodeURIComponent(row.record_id)}`}>
          {fmtClock(row.started_at_ms)}
        </Link>
      </td>
      <td className={TD} title={row.instance_id}>
        {shortInstance(row.instance_id)}
      </td>
      <td className={TD}>
        <StateBadge kind="query" value={row.state} />
      </td>
      <td className={TD_NUM}>{fmtDuration(row.duration_ms)}</td>
      <td className={TD}>{row.statement_type}</td>
      <td className={TD}>{row.schema_name ?? EMPTY}</td>
      <td className={`${TD} ${MONO} max-w-lg truncate text-zinc-400`}>
        {row.sql_preview ?? (
          // 방송은 원문을 절대 담지 않는다(T-22). 정책이 원문이면 여기가 빈다.
          <span className="text-zinc-600">원문 정책 — 상세에서 조회</span>
        )}
      </td>
    </tr>
  );
}
