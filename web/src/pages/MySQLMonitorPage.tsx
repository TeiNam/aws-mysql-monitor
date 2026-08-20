import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Activity,
  Calendar,
  Clock,
  Database,
  Filter,
  Globe,
  Hash,
  Pause,
  Play,
  RefreshCw,
  Server,
  User,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { Card } from "../components/Card";
import {
  CollectorControls,
  CollectorFacts,
  useCollectorStatus,
} from "../components/CollectorControls";
import { EmptyRow, ErrorNotice, Note, Pending } from "../components/Notices";
import { Pagination } from "../components/Pagination";
import { EnvChip } from "../components/Shell";
import { Sparkline } from "../components/Sparkline";
import { SqlModal } from "../components/SqlModal";
import {
  BTN_GHOST,
  CELL_ICON,
  LABEL,
  PAGE_TITLE,
  SELECT,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { useLive, useLiveMissed, useLiveTopics } from "../hooks/useLive";
import { fetchInstances, fetchSlowQueries, queryKeys } from "../lib/api";
import { EMPTY, fmtInt, fmtListTime, fmtRate, shortInstance, type Timezone } from "../lib/format";
import { MAX_TOPICS } from "../lib/live-reduce";
import type { InstanceView, SlowQueryView } from "../lib/types";

/** 참조 대시보드와 같은 새로고침 간격. */
const REFRESH_INTERVALS = [
  { label: "5초", value: 5 },
  { label: "15초", value: 15 },
  { label: "30초", value: 30 },
  { label: "1분", value: 60 },
  { label: "5분", value: 300 },
  { label: "10분", value: 600 },
] as const;

const PAGE_SIZE = 20;
/** 서버에서 한 번에 받아 클라이언트가 나눠 보여줄 최대 건수 (백엔드 `MAX_LIMIT`). */
const WINDOW = 500;

export function MySQLMonitorPage() {
  const [params, setParams] = useSearchParams();
  const instance = params.get("instance") ?? "";
  const tz: Timezone = params.get("tz") === "UTC" ? "UTC" : "KST";
  const page = Math.max(1, Number.parseInt(params.get("page") ?? "1", 10) || 1);

  const [autoRefresh, setAutoRefresh] = useState(true);
  const [interval, setIntervalSecs] = useState(30);
  const [countdown, setCountdown] = useState(30);
  const [openSql, setOpenSql] = useState<SlowQueryView | null>(null);

  const queryClient = useQueryClient();
  const listParams = useMemo(
    () => ({ limit: WINDOW, ...(instance === "" ? {} : { instance }) }),
    [instance],
  );

  const list = useQuery({
    queryKey: queryKeys.slowQueries(listParams),
    queryFn: ({ signal }) => fetchSlowQueries(listParams, signal),
  });
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });

  // ── 자동 새로고침 ──────────────────────────────────────────────────────────
  // 참조 구현과 같은 카운트다운. **`refetch` 가 아니라 무효화**를 쓴다 — 필터를
  // 바꾼 뒤에도 같은 타이머가 새 조회를 갱신해야 한다.
  useEffect(() => {
    if (!autoRefresh) return;
    setCountdown(interval);
    const id = window.setInterval(() => {
      setCountdown((left) => {
        if (left > 1) return left - 1;
        void queryClient.invalidateQueries({ queryKey: queryKeys.slowQueriesAll });
        return interval;
      });
    }, 1_000);
    return () => window.clearInterval(id);
  }, [autoRefresh, interval, queryClient]);

  // 방송이 오면 **기다리지 않고** 다시 읽는다. 참조 구현은 고정 주기 폴링만 했다.
  const missedCount = useLiveMissed();
  const live = useLive();
  const slowqSeen = live.slowq.length;
  useEffect(() => {
    if (slowqSeen === 0 && missedCount === 0) return;
    void queryClient.invalidateQueries({ queryKey: queryKeys.slowQueriesAll });
  }, [slowqSeen, missedCount, queryClient]);

  function update(key: string, value: string) {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(key);
    else next.set(key, value);
    // 필터가 바뀌면 첫 페이지로. 3페이지를 보다 필터를 바꾸면 빈 페이지가 된다.
    if (key !== "page") next.delete("page");
    setParams(next, { replace: true });
  }

  const items = list.data?.items ?? [];
  const nowMs = Date.now();
  const visible = items.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE);

  return (
    <div className="space-y-6">
      <h1 className={PAGE_TITLE}>MySQL Real-time Slow Query Monitor</h1>
      <p className="-mt-4 text-sm text-gray-600 italic">= AWS Aurora for MySQL &amp; RDS =</p>

      <ScraperStatus instances={instances.data ?? []} tz={tz} />

      <Card
        title={
          <>
            <Database className="h-5 w-5 text-gray-500" /> Slow Queries
          </>
        }
        actions={
          <>
            <label className={LABEL}>
              <Filter className="h-4 w-4 text-gray-500" />
              <select
                className={SELECT}
                value={instance}
                onChange={(e) => update("instance", e.target.value)}
                aria-label="인스턴스 선택"
              >
                <option value="">All Instances</option>
                {(instances.data ?? []).map((i) => (
                  <option key={i.id} value={i.id}>
                    {i.name}
                  </option>
                ))}
              </select>
            </label>
            <label className={LABEL}>
              <Globe className="h-4 w-4 text-gray-500" />
              <select
                className={SELECT}
                value={tz}
                onChange={(e) => update("tz", e.target.value)}
                aria-label="시간대 선택"
              >
                <option value="KST">KST</option>
                <option value="UTC">UTC</option>
              </select>
            </label>
            <select
              className={SELECT}
              value={interval}
              disabled={!autoRefresh}
              onChange={(e) => setIntervalSecs(Number.parseInt(e.target.value, 10))}
              aria-label="새로고침 간격"
            >
              {REFRESH_INTERVALS.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
            <button
              type="button"
              className={BTN_GHOST}
              onClick={() => setAutoRefresh((on) => !on)}
              title={autoRefresh ? "자동 새로고침 중지" : "자동 새로고침 시작"}
            >
              {autoRefresh ? (
                <>
                  <Pause className="h-4 w-4" /> {countdown}초
                </>
              ) : (
                <>
                  <Play className="h-4 w-4" /> 자동
                </>
              )}
            </button>
            <button
              type="button"
              className={BTN_GHOST}
              onClick={() => void list.refetch()}
              disabled={list.isFetching}
              aria-label="수동 새로고침"
            >
              <RefreshCw className={`h-4 w-4 ${list.isFetching ? "animate-spin" : ""}`} />
            </button>
          </>
        }
      >
        {list.error !== null ? (
          <ErrorNotice error={list.error} onRetry={() => void list.refetch()} />
        ) : list.isPending ? (
          <Pending label="조회 중…" />
        ) : (
          <>
            <div className="overflow-x-auto">
              <table className={TABLE}>
                <thead>
                  <tr>
                    <th className={TH}>Start Time</th>
                    <th className={TH}>Instance</th>
                    <th className={TH}>Database</th>
                    <th className={TH}>User</th>
                    <th className={TH_NUM}>Thread</th>
                    <th className={TH_NUM}>Time</th>
                    <th className={TH_NUM}>Rows</th>
                    <th className={TH}>Query</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {visible.length === 0 ? (
                    <EmptyRow colSpan={8}>
                      조회 구간(최근 24시간)에 기록이 없다. 임계값을 넘는 쿼리가 실행되면 여기
                      쌓인다.
                    </EmptyRow>
                  ) : (
                    visible.map((q) => (
                      <tr key={q.record_id} className={`${TR} cursor-pointer`} onClick={() => setOpenSql(q)}>
                        <td className={TD}>
                          <span className="flex items-center">
                            <Calendar className={CELL_ICON} />
                            {fmtListTime(q.started_at_ms, tz, nowMs)}
                          </span>
                        </td>
                        <td className={TD} title={q.instance_id}>
                          <span className="flex items-center gap-1">
                            <Database className={CELL_ICON} />
                            {shortInstance(q.instance_id)}
                            <EnvChip env={q.env} />
                          </span>
                        </td>
                        <td className={TD}>
                          <span className="flex items-center">
                            <Server className={CELL_ICON} />
                            {q.schema_name ?? EMPTY}
                          </span>
                        </td>
                        <td className={TD}>
                          <span className="flex items-center">
                            <User className={CELL_ICON} />
                            {q.db_user ?? EMPTY}
                          </span>
                        </td>
                        <td className={TD_NUM}>
                          <span className="flex items-center justify-end">
                            <Hash className={CELL_ICON} />
                            {q.thread_id}
                          </span>
                        </td>
                        <td className={TD_NUM} title={`측정 소스: ${q.duration_source}`}>
                          <span className="flex items-center justify-end">
                            <Clock className={CELL_ICON} />
                            {(q.duration_ms / 1000).toFixed(1)}s
                          </span>
                        </td>
                        <td className={TD_NUM} title="조사 행 / 반환 행">
                          {fmtInt(q.rows_examined)}
                          <span className="text-gray-400"> / </span>
                          {fmtInt(q.rows_sent)}
                        </td>
                        <td className="max-w-[520px] px-3 py-2 text-sm text-gray-700">
                          <div className="truncate font-mono text-xs">
                            {q.sql_text ??
                              (q.sql_redacted_reason === "insufficient_role"
                                ? "(열람 권한 없음)"
                                : "(저장되지 않음)")}
                          </div>
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>

            <Pagination
              page={page}
              pageSize={PAGE_SIZE}
              total={items.length}
              truncated={list.data.has_more}
              onChange={(n) => update("page", String(n))}
            />
            <Note>
              행을 누르면 전체 SQL 을 본다. 시각은 {tz} 기준이고, 자동 새로고침은{" "}
              {autoRefresh ? `${interval}초` : "꺼짐"} — 새 슬로우 쿼리가 방송되면 주기를
              기다리지 않고 즉시 다시 읽는다.
            </Note>
          </>
        )}
      </Card>

      {openSql === null ? null : (
        <SqlModal
          title={`SQL — ${shortInstance(openSql.instance_id)} · thread ${openSql.thread_id}`}
          sql={openSql.sql_text}
          reason={openSql.sql_redacted_reason}
          truncated={openSql.sql_text_truncated}
          onClose={() => setOpenSql(null)}
        />
      )}
    </div>
  );
}

/**
 * 수집기 상태.
 *
 * 참조 대시보드는 여기에 `Start / Stop Monitoring` 버튼과 `Status: running` 을 뒀다.
 * 이 백엔드는 **리스를 잡은 워커가 항상 수집한다** — 켜고 끄는 개념이 아니라
 * 리더 선출이다. 그래서 버튼을 흉내내지 않고, 실제로 흐르는 것을 보여준다:
 * 실시간 지표가 오고 있으면 그게 곧 "수집 중" 의 증거다.
 */
function ScraperStatus({ instances, tz }: { instances: InstanceView[]; tz: Timezone }) {
  const live = useLive();
  const status = useCollectorStatus();
  const topics = useMemo(
    () => instances.slice(0, MAX_TOPICS).map((i) => `status:inst=${i.id}`),
    [instances],
  );
  useLiveTopics(topics);

  const collecting = instances.filter((i) => i.collectible).length;
  const streaming = live.conn === "open";

  return (
    <Card
      title={
        <>
          <Activity className={`h-5 w-5 ${streaming ? "animate-pulse text-green-600" : "text-gray-400"}`} />
          MySQL Slow Query Scraper
        </>
      }
      actions={<CollectorControls />}
      note={
        <>
          인스턴스 {instances.length}개 · 수집 대상 {collecting}개. 이 수집기는{" "}
          <strong>리스를 잡은 워커가 돈다</strong> — "정지" 는 리스를 놓는 것이 아니라 수집
          태스크를 멈추는 것이고, <strong>그 구간의 슬로우 쿼리는 기록되지 않는다.</strong>
          정지는 이 워커(<span className="font-mono">{status.data?.worker_id ?? "?"}</span>)에만
          적용된다.
        </>
      }
    >
      <div className="mb-4 border-b border-gray-200 pb-4">
        <CollectorFacts status={status.data} />
      </div>
      <div className="overflow-x-auto">
        <table className={TABLE}>
          <thead>
            <tr>
              <th className={TH}>Instance</th>
              <th className={TH}>Env</th>
              <th className={TH}>State</th>
              <th className={TH_NUM}>QPS</th>
              <th className={TH_NUM}>Slow/s</th>
              <th className={TH_NUM}>Threads</th>
              <th className={TH_NUM}>Conn</th>
              <th className={TH_NUM}>Lock waits</th>
              <th className={TH}>QPS 추이</th>
              <th className={TH_NUM}>Updated</th>
            </tr>
          </thead>
          <tbody className={TBODY}>
            {instances.length === 0 ? (
              <EmptyRow colSpan={10}>
                등록된 인스턴스가 없다. 탐색(discovery)이 아직 돌지 않았거나 필터가 전부
                제외했다.
              </EmptyRow>
            ) : (
              instances.map((i) => {
                const m = live.status[i.id];
                const history = live.qpsHistory[i.id] ?? [];
                return (
                  <tr key={i.id} className={TR}>
                    <td className={TD} title={i.id}>
                      {i.name}
                    </td>
                    <td className={TD}>
                      <EnvChip env={i.env} />
                    </td>
                    <td className={TD}>{i.state}</td>
                    <td
                      className={TD_NUM}
                      title={m?.rate_gap_reason === null ? undefined : `비율 없음: ${m?.rate_gap_reason}`}
                    >
                      {fmtRate(m?.qps)}
                    </td>
                    <td className={TD_NUM}>{fmtRate(m?.slow_per_sec)}</td>
                    <td className={TD_NUM}>{fmtInt(m?.threads_running)}</td>
                    <td className={TD_NUM}>{fmtInt(m?.threads_connected)}</td>
                    <td className={TD_NUM}>{fmtInt(m?.lock_waits)}</td>
                    <td className={TD}>
                      <Sparkline values={history} label={`${i.name} QPS 추이`} />
                    </td>
                    <td className={`${TD_NUM} text-gray-500`}>
                      {m === undefined ? EMPTY : fmtListTime(m.at_ms, tz, Date.now())}
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
      <Note>
        <span className="font-mono">{EMPTY}</span> 는 값이 0 이 아니라 <strong>아직 비율을 낼
        수 없다</strong>는 뜻이다(첫 샘플·카운터 초기화). 끊긴 구간은 추이 선이 끊긴다.
      </Note>
    </Card>
  );
}
