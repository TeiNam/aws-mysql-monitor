import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, type ReactNode } from "react";
import { Link, useSearchParams } from "react-router";
import { StateBadge } from "../components/Badges";
import { ErrorNotice, Note, Pending } from "../components/Notices";
import { CARD, LINK, MONO, ROW, TABLE, TD, TD_NUM, TH, TH_NUM } from "../components/styles";
import { useLiveLagged } from "../hooks/useLive";
import { fetchInstances, fetchSlowQueries, queryKeys, type ListQuery } from "../lib/api";
import { EMPTY, fmtDuration, fmtInt, fmtClock, shortInstance } from "../lib/format";
import type { SlowQueryView } from "../lib/types";

/** 목록 조회 상한. 백엔드 `MAX_LIMIT` 과 같다 — 더 요구해도 서버가 자른다. */
const MAX_LIMIT = 200;
const DEFAULT_LIMIT = 50;
const LIMIT_OPTIONS = [25, 50, 100, 200] as const;

/** URL 은 사용자 입력이다. 범위를 벗어난 값은 기본값으로 되돌린다. */
function parseLimit(raw: string | null): number {
  if (raw === null) return DEFAULT_LIMIT;
  const n = Number.parseInt(raw, 10);
  if (!Number.isFinite(n)) return DEFAULT_LIMIT;
  return Math.min(Math.max(n, 1), MAX_LIMIT);
}

/**
 * 슬로우 쿼리 목록. `GET /api/slow-queries`.
 *
 * 필터를 URL 에 담는다(FR-UI-19) — 그래야 링크로 공유할 수 있고 새로고침이
 * 상태를 잃지 않는다.
 */
export function SlowQueries() {
  const [params, setParams] = useSearchParams();
  const env = params.get("env") ?? "";
  const instance = params.get("instance") ?? "";
  const limit = parseLimit(params.get("limit"));

  const query: ListQuery = useMemo(
    () => ({
      limit,
      // 빈 문자열을 보내면 서버가 `invalid_env` 로 답한다. 없는 필터는 **아예 빼야** 한다.
      ...(env === "" ? {} : { env }),
      ...(instance === "" ? {} : { instance }),
    }),
    [env, instance, limit],
  );

  const list = useQuery({
    queryKey: queryKeys.slowQueries(query),
    queryFn: ({ signal }) => fetchSlowQueries(query, signal),
  });
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });

  // 방송이 밀리면 유실된 쿼리는 **다시 방송되지 않는다.** HTTP 로 다시 읽는다.
  const laggedCount = useLiveLagged();
  const refetch = list.refetch;
  useEffect(() => {
    if (laggedCount === 0) return;
    void refetch();
  }, [laggedCount, refetch]);

  const envOptions = useMemo(
    () => [...new Set((instances.data ?? []).map((i) => i.env))].sort(),
    [instances.data],
  );

  function update(key: string, value: string) {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  }

  return (
    <section>
      <div className="mb-4 flex flex-wrap items-end justify-between gap-3">
        <h1 className="text-lg font-semibold tracking-tight">슬로우 쿼리</h1>
        <div className="flex flex-wrap items-end gap-3">
          <Field label="환경">
            <select
              className={SELECT}
              value={env}
              onChange={(e) => update("env", e.target.value)}
            >
              <option value="">전체</option>
              {envOptions.map((value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
          </Field>
          <Field label="인스턴스">
            <select
              className={SELECT}
              value={instance}
              onChange={(e) => update("instance", e.target.value)}
            >
              <option value="">전체</option>
              {(instances.data ?? []).map((i) => (
                <option key={i.id} value={i.id}>
                  {shortInstance(i.id)}
                </option>
              ))}
            </select>
          </Field>
          <Field label="개수">
            <select
              className={SELECT}
              value={String(limit)}
              onChange={(e) => update("limit", e.target.value)}
            >
              {/*
                URL 이 `?limit=37` 처럼 목록에 없는 값을 줄 수 있다. 그 값은
                유효하므로(서버가 1..200 을 받는다) **버리지 않고 항목으로
                보여 준다** — 안 그러면 select 가 다른 숫자를 표시해 거짓말한다.
              */}
              {(LIMIT_OPTIONS.includes(limit as (typeof LIMIT_OPTIONS)[number])
                ? LIMIT_OPTIONS
                : [...LIMIT_OPTIONS, limit].sort((a, b) => a - b)
              ).map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </Field>
          <button
            type="button"
            onClick={() => void list.refetch()}
            disabled={list.isFetching}
            className="rounded border border-zinc-700 px-3 py-1.5 text-sm hover:bg-zinc-800 disabled:opacity-50"
          >
            {list.isFetching ? "조회 중…" : "새로 고침"}
          </button>
        </div>
      </div>

      {list.error !== null ? (
        <ErrorNotice error={list.error} onRetry={() => void list.refetch()} />
      ) : list.isPending ? (
        <Pending label="조회 중…" />
      ) : (
        <>
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
                  <th className={TH_NUM}>조사 행</th>
                  <th className={TH_NUM}>반환 행</th>
                  <th className={TH}>SQL</th>
                </tr>
              </thead>
              <tbody>
                {list.data.items.length === 0 ? (
                  <tr>
                    <td className={`${TD} text-zinc-500`} colSpan={9}>
                      조회 구간(최근 24시간)에 기록이 없다.
                    </td>
                  </tr>
                ) : (
                  list.data.items.map((item) => <QueryRow key={item.record_id} item={item} />)
                )}
              </tbody>
            </table>
          </div>

          {list.data.has_more ? (
            <Note>
              상한 {limit}건에 걸렸다. 백엔드가 아직 <span className="font-mono">next_cursor</span>{" "}
              를 발급하지 않으므로 <strong>다음 페이지가 없다</strong> — 인스턴스나 환경으로
              좁히거나 개수를 올려야 한다.
            </Note>
          ) : null}
        </>
      )}
    </section>
  );
}

const SELECT =
  "rounded border border-zinc-700 bg-zinc-900 px-2 py-1.5 text-sm text-zinc-100 focus:border-sky-500 focus:outline-none";

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="flex flex-col gap-1 text-xs text-zinc-500">
      {label}
      {children}
    </label>
  );
}

function QueryRow({ item }: { item: SlowQueryView }) {
  return (
    <tr className={ROW}>
      <td className={`${TD} whitespace-nowrap`}>
        <Link className={LINK} to={`/slow-queries/${encodeURIComponent(item.record_id)}`}>
          {fmtClock(item.started_at_ms)}
        </Link>
      </td>
      <td className={TD} title={item.instance_id}>
        {shortInstance(item.instance_id)}
      </td>
      <td className={TD}>
        <StateBadge kind="query" value={item.state} />
      </td>
      <td className={TD_NUM} title={`측정 소스: ${item.duration_source}`}>
        {fmtDuration(item.duration_ms)}
      </td>
      <td className={TD}>{item.statement_type}</td>
      <td className={TD}>{item.schema_name ?? EMPTY}</td>
      <td className={TD_NUM}>{fmtInt(item.rows_examined)}</td>
      <td className={TD_NUM}>{fmtInt(item.rows_sent)}</td>
      <td className={`${TD} ${MONO} max-w-md truncate text-zinc-400`}>
        {item.sql_text ?? redactionLabel(item.sql_redacted_reason)}
      </td>
    </tr>
  );
}

function redactionLabel(reason: SlowQueryView["sql_redacted_reason"]): string {
  return reason === "insufficient_role" ? "(열람 권한 없음)" : "(저장되지 않음)";
}
