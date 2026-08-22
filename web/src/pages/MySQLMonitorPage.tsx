import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Activity,
  Calendar,
  Clock,
  Database,
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
import { PageHeader } from "../components/PageHeader";
import { Card } from "../components/Card";
import {
  CollectorBadge,
  CollectorFacts,
  useCollectorStatus,
} from "../components/CollectorControls";
import { EnvFilter, InstanceFilter, InstanceSearch } from "../components/Filters";
import { EmptyRow, ErrorNotice, Note, Pending } from "../components/Notices";
import { Pagination } from "../components/Pagination";
import { EnvChip } from "../components/Shell";
import { displayDurationMs, isRunning } from "../lib/elapsed";
import { SqlModal } from "../components/SqlModal";
import { StateBadge } from "../components/StateBadge";
import {
  BTN_GHOST,
  CELL_ICON,
  CELL_X,
  COL_GROW,
  COL_TIGHT,
  COL_TIGHT_CAPPED,
  LABEL,
  SELECT,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { useInstances } from "../hooks/useInstances";
import { useLive, useLiveMissed, useLiveSlowqSeen, useLiveTopics } from "../hooks/useLive";
import { fetchSlowQueries, queryKeys } from "../lib/api";
import { EMPTY, fmtInt, fmtListTime, shortInstance, type Timezone } from "../lib/format";
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
  const env = params.get("env") ?? "";
  const instanceLike = params.get("instance_like") ?? "";
  const tz: Timezone = params.get("tz") === "UTC" ? "UTC" : "KST";
  const page = Math.max(1, Number.parseInt(params.get("page") ?? "1", 10) || 1);

  const [autoRefresh, setAutoRefresh] = useState(true);
  const [interval, setIntervalSecs] = useState(30);
  const [countdown, setCountdown] = useState(30);
  const [openSql, setOpenSql] = useState<SlowQueryView | null>(null);

  const queryClient = useQueryClient();
  // **필터는 조회 키에 들어간다.** 안 넣으면 필터를 바꿔도 캐시된 앞 결과가 그려진다.
  const listParams = useMemo(
    () => ({
      limit: WINDOW,
      ...(instance === "" ? {} : { instance }),
      ...(env === "" ? {} : { env }),
      ...(instanceLike === "" ? {} : { instance_like: instanceLike }),
    }),
    [instance, env, instanceLike],
  );

  const list = useQuery({
    queryKey: queryKeys.slowQueries(listParams),
    queryFn: ({ signal }) => fetchSlowQueries(listParams, signal),
  });
  // **머리말의 리전 범위가 적용된 목록**이다(`useInstances`). 화면마다 직접
  // 조회하면 리전 필터를 한 곳만 빠뜨려도 그 화면에서 범위 밖이 보인다.
  const instances = useInstances();

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
  //
  // ⚠ `slowq.length` 가 아니라 **누적 수**를 본다. 길이는 상한(200)에서 멈추므로
  // 그 뒤로는 방송이 와도 신호가 바뀌지 않는다.
  const missedCount = useLiveMissed();
  const slowqSeen = useLiveSlowqSeen();
  useEffect(() => {
    if (slowqSeen === 0 && missedCount === 0) return;
    void queryClient.invalidateQueries({ queryKey: queryKeys.slowQueriesAll });
  }, [slowqSeen, missedCount, queryClient]);

  function update(key: string, value: string) {
    updateMany({ [key]: value });
  }

  /** 여러 값을 **한 번에** 바꾼다 — env 를 바꾸며 인스턴스를 지울 때 두 번 쓰면
   *  중간 상태(새 env + 옛 인스턴스)로 한 번 조회가 나간다. */
  function updateMany(patch: Record<string, string>) {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(patch)) {
      if (value === "") next.delete(key);
      else next.set(key, value);
    }
    // 필터가 바뀌면 첫 페이지로. 3페이지를 보다 필터를 바꾸면 빈 페이지가 된다.
    if (!Object.keys(patch).every((k) => k === "page")) next.delete("page");
    setParams(next, { replace: true });
  }

  const items = list.data?.items ?? [];
  // **결과가 줄면 페이지를 당긴다.** 3페이지를 보다 자동 새로고침으로 건수가 줄면
  // 빈 표가 나오고, 그건 "기록이 없다" 로 읽힌다.
  const lastPage = Math.max(1, Math.ceil(items.length / PAGE_SIZE));
  const safePage = Math.min(page, lastPage);
  const visible = items.slice((safePage - 1) * PAGE_SIZE, safePage * PAGE_SIZE);

  // **진행 중 행의 경과 시간은 여기서 흐른다.**
  //
  // 저장된 `duration_ms` 는 마지막으로 저장된 시점의 값이다. 수집기는 실행계획을 확보하면
  // 그 레코드를 다시 쓰지 않으므로(하트비트는 관측 시각만 올린다) 10분째 도는 쿼리가
  // `2.0s` 로 남는다 — 09 §3.4 가 약속한 동작이 구현되지 않은 상태였다(24라운드).
  //
  // 진행 중 행이 보일 때만 타이머를 돈다. 항상 돌리면 정적인 표에서 매초 리렌더한다.
  const hasRunning = visible.some((q) => isRunning(q.state));
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    if (!hasRunning) return;
    const id = window.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => window.clearInterval(id);
  }, [hasRunning]);

  return (
    <div className="space-y-6">
      <PageHeader
        title="MySQL Real-time Slow Query Monitor"
        subtitle="= AWS Aurora for MySQL & RDS ="
      />

      <ScraperStatus instances={instances.data ?? []} />

      <Card
        title={
          <>
            <Database className="h-5 w-5 text-gray-500" /> Slow Queries
          </>
        }
        actions={
          <>
            <EnvFilter
              instances={instances.data ?? []}
              env={env}
              instance={instance}
              instanceLike={instanceLike}
              onChange={(patch) =>
                updateMany({
                  ...(patch.env === undefined ? {} : { env: patch.env }),
                  ...(patch.instance === undefined ? {} : { instance: patch.instance }),
                })
              }
            />
            <InstanceFilter
              instances={instances.data ?? []}
              env={env}
              instance={instance}
              instanceLike={instanceLike}
              onChange={(patch) => update("instance", patch.instance ?? "")}
            />
            <InstanceSearch
              instances={instances.data ?? []}
              env={env}
              value={instanceLike}
              onChange={(like) => update("instance_like", like)}
            />
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
                    {/* **짧은 열은 내용 폭만.** 남는 폭은 Query 가 받는다 — 표에서
                        정보량이 가장 큰 열이 가장 넓어야 한다. */}
                    <th className={`${TH} ${COL_TIGHT}`}>Start Time</th>
                    <th className={`${TH} ${COL_TIGHT_CAPPED}`}>Instance</th>
                    <th className={`${TH} ${COL_TIGHT_CAPPED}`}>Database</th>
                    <th className={`${TH} ${COL_TIGHT_CAPPED}`}>User</th>
                    <th className={`${TH} ${COL_TIGHT}`}>State</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Thread</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Time</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Rows</th>
                    <th className={`${TH} ${COL_GROW}`}>Query</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {visible.length === 0 ? (
                    // **"없다" 와 "못 봤다" 를 구분한다.** 서버가 조회 상한에 걸리면
                    // (인스턴스가 많거나 구간이 길 때) 읽은 표본이 환경 필터에서 전부
                    // 빠질 수 있다. 그때 "기록이 없다" 로 말하면 조사하던 사람이
                    // 문제가 없다고 결론 내린다. 상한 표시는 `Pagination` 이 담당하는데
                    // 그건 0건에서 렌더되지 않으므로 여기서 말한다.
                    <EmptyRow colSpan={9}>
                      {list.data.has_more
                        ? "조회 상한에 걸려 이 조건에 맞는 기록을 찾지 못했다 — 기록이 없다는 뜻은 아니다. 인스턴스나 구간을 좁혀서 다시 본다."
                        : "조회 구간(최근 24시간)에 기록이 없다. 임계값을 넘는 쿼리가 실행되면 여기 쌓인다."}
                    </EmptyRow>
                  ) : (
                    visible.map((q) => (
                      <tr key={q.record_id} className={`${TR} cursor-pointer`} onClick={() => setOpenSql(q)}>
                        <td className={`${TD} ${COL_TIGHT}`}>
                          <span className="flex items-center">
                            <Calendar className={CELL_ICON} />
                            {fmtListTime(q.started_at_ms, tz, nowMs)}
                          </span>
                        </td>
                        <td className={`${TD} ${COL_TIGHT_CAPPED}`} title={q.instance_id}>
                          <span className="flex items-center gap-1">
                            <Database className={CELL_ICON} />
                            {shortInstance(q.instance_id)}
                            <EnvChip env={q.env} />
                          </span>
                        </td>
                        <td className={`${TD} ${COL_TIGHT}`}>
                          <span className="flex items-center">
                            <Server className={CELL_ICON} />
                            {q.schema_name ?? EMPTY}
                          </span>
                        </td>
                        <td className={`${TD} ${COL_TIGHT}`}>
                          <span className="flex items-center">
                            <User className={CELL_ICON} />
                            {q.db_user ?? EMPTY}
                          </span>
                        </td>
                        <td className={`${TD} ${COL_TIGHT}`}>
                          <StateBadge state={q.state} reason={q.abandoned_reason} />
                        </td>
                        <td className={`${TD_NUM} ${COL_TIGHT}`}>
                          <span className="flex items-center justify-end">
                            <Hash className={CELL_ICON} />
                            {q.thread_id}
                          </span>
                        </td>
                        <td
                          className={`${TD_NUM} ${COL_TIGHT}`}
                          title={
                            isRunning(q.state)
                              ? `아직 실행 중 — 경과는 시작 시각 기준이다 (측정 소스: ${q.duration_source})`
                              : `측정 소스: ${q.duration_source}`
                          }
                        >
                          <span className="flex items-center justify-end">
                            <Clock className={CELL_ICON} />
                            {(displayDurationMs(q, nowMs) / 1000).toFixed(1)}s
                          </span>
                        </td>
                        <td className={`${TD_NUM} ${COL_TIGHT}`} title="조사 행 / 반환 행">
                          {fmtInt(q.rows_examined)}
                          <span className="text-gray-400"> / </span>
                          {fmtInt(q.rows_sent)}
                        </td>
                        {/* 남는 폭을 전부 받는다. `max-w-0` 가 있어야 긴 SQL 이 표를
                            밀어내지 않고 잘린다. */}
                        <td className={`${COL_GROW} ${CELL_X} max-w-0 py-2 text-sm text-gray-700`}>
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
              page={safePage}
              pageSize={PAGE_SIZE}
              total={items.length}
              truncated={list.data.has_more}
              onChange={(n) => update("page", String(n))}
            />
            <Note>
              <strong>진행 중</strong>은 실행시간이 지금까지의 값이고,{" "}
              <strong>추적 끊김</strong>은 하한이다(언제 끝났는지 모른다). 행을 누르면 <strong>저장된</strong> SQL 을 본다 — 마스킹·절단·권한 제한이 그대로 표시된다. 시각은 {tz} 기준이고, 자동 새로고침은{" "}
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
 *
 * # 인스턴스별 지표 표는 여기 없다
 *
 * **Metrics 탭이 같은 목록을 더 잘 보여준다** — CloudWatch 열(CPU·메모리·스토리지)까지
 * 붙는다. 같은 표를 두 화면에 두면 한쪽만 고치게 되고, 이 화면의 본론(슬로우 쿼리 목록)이
 * 그만큼 아래로 밀린다.
 *
 * 그래도 이 조각은 남는다. **슬로우 쿼리 방송 구독이 여기 있고**, 없으면 목록이 주기를
 * 기다려야만 갱신된다.
 */
function ScraperStatus({ instances }: { instances: InstanceView[] }) {
  const live = useLive();
  const status = useCollectorStatus();
  // **슬로우 쿼리 방송만 구독한다.**
  //
  // 구독하지 않으면 새 슬로우 쿼리 방송이 오지 않아 "주기를 기다리지 않고 즉시 다시
  // 읽는다" 가 거짓말이 된다. 환경은 등록부에서 얻는다 — 목록을 손으로 적으면 없는
  // 환경을 구독해 `denied` 만 받는다.
  //
  // 지표 토픽(`status:inst=`)은 **더 이상 구독하지 않는다.** 인스턴스별 지표 표가
  // Metrics 탭으로 갔으므로 받아도 쓰지 않는다. 덤으로 상한(50) 다툼이 사라졌다 —
  // 전에는 인스턴스가 50대를 넘으면 방송 구독이 밀려 거부됐다.
  const topics = useMemo(
    () => [...new Set(instances.map((i) => i.env))].sort().map((e) => `slowq:env=${e}`),
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
      // 배지만 둔다 — 조작은 RDS 인스턴스 화면 한 곳에서 한다(`CollectorBadge` 주석).
      actions={<CollectorBadge />}
      note={
        <>
          인스턴스 {instances.length}개 · 수집 대상 {collecting}개 — 인스턴스별 지표는{" "}
          <strong>Metrics</strong> 탭에 있다. 이 수집기는{" "}
          <strong>리스를 잡은 워커가 돈다</strong> — "정지" 는 리스를 놓는 것이 아니라 수집
          태스크를 멈추는 것이고, <strong>그 구간의 슬로우 쿼리는 기록되지 않는다.</strong>{" "}
          정지·재개는 <strong>RDS Instance Management</strong> 화면에서 하고, 설정 저장소에
          있으므로 워커 전체에 적용된다.
        </>
      }
    >
      <CollectorFacts status={status.data} />
    </Card>
  );
}
