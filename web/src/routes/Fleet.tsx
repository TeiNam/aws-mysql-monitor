import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { Link } from "react-router";
import { StateBadge } from "../components/Badges";
import { ErrorNotice, Note, Pending } from "../components/Notices";
import { Sparkline } from "../components/Sparkline";
import {
  CARD,
  LABEL,
  LINK,
  MONO,
  MUTED,
  ROW,
  SCROLL_BOX,
  TABLE,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
} from "../components/styles";
import { useLive, useLiveTopics } from "../hooks/useLive";
import { fetchInstances, queryKeys } from "../lib/api";
import { EMPTY, fmtClock, fmtInt, fmtRate, shortInstance } from "../lib/format";
import { HISTORY_LEN, MAX_TOPICS } from "../lib/live-reduce";
import type { InstanceView, LiveMetrics } from "../lib/types";

/** 플릿 개요. `GET /api/instances` + WS `status:inst=…`. */
export function Fleet() {
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });
  const live = useLive();

  const rows = instances.data ?? EMPTY_INSTANCES;
  // 구독 상한을 넘기면 **앞에서 자르고 아래에 알린다.**
  const topics = useMemo(
    () => rows.slice(0, MAX_TOPICS).map((i) => `status:inst=${i.id}`),
    [rows],
  );
  useLiveTopics(topics);

  if (instances.isPending) return <Pending label="인스턴스를 불러오는 중…" />;
  if (instances.error !== null) {
    return <ErrorNotice error={instances.error} onRetry={() => void instances.refetch()} />;
  }

  return (
    <section>
      <h1 className="mb-4 text-lg font-semibold tracking-tight">플릿 개요</h1>
      <Summary instances={rows} status={live.status} />

      <div className={`${SCROLL_BOX} mt-4`}>
        <table className={TABLE}>
          <thead>
            <tr>
              <th className={TH}>인스턴스</th>
              <th className={TH}>환경</th>
              <th className={TH}>상태</th>
              <th className={TH}>엔진</th>
              <th className={TH_NUM}>QPS</th>
              <th className={TH_NUM}>슬로우/초</th>
              <th className={TH_NUM}>실행 중</th>
              <th className={TH_NUM}>접속</th>
              <th className={TH_NUM}>락 대기</th>
              <th className={TH}>QPS 추이</th>
              <th className={TH_NUM}>갱신</th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td className={`${TD} ${MUTED}`} colSpan={11}>
                  등록된 인스턴스가 없다. 탐색(discovery)이 아직 돌지 않았거나 필터가 전부
                  제외했다.
                </td>
              </tr>
            ) : (
              rows.map((instance) => (
                <InstanceRow
                  key={instance.id}
                  instance={instance}
                  metrics={live.status[instance.id]}
                  history={live.qpsHistory[instance.id] ?? EMPTY_HISTORY}
                />
              ))
            )}
          </tbody>
        </table>
      </div>

      {rows.length > MAX_TOPICS ? (
        <Note>
          인스턴스가 {rows.length}개다. 구독 상한({MAX_TOPICS})까지만 실시간 지표를 받는다 —
          나머지 행의 지표 칸은 비어 있다.
        </Note>
      ) : null}
      <Note>
        지표는 5초 주기 샘플이고 추이는 최근 {HISTORY_LEN}표본이다(끊긴 구간은 선이
        끊긴다). <span className="font-mono">{EMPTY}</span> 는 값이 0 이
        아니라 <strong>아직 비율을 낼 수 없다</strong>는 뜻이다(첫 샘플·카운터 초기화).
      </Note>
    </section>
  );
}

/** 매 렌더 새 배열을 만들지 않도록 모듈 수준에 둔다 (`useMemo` 가 헛돌지 않게). */
const EMPTY_HISTORY: readonly (number | null)[] = [];
const EMPTY_INSTANCES: readonly InstanceView[] = [];

interface InstanceRowProps {
  instance: InstanceView;
  metrics: LiveMetrics | undefined;
  history: readonly (number | null)[];
}

function InstanceRow({ instance, metrics, history }: InstanceRowProps) {
  const gap = metrics?.rate_gap_reason ?? null;
  return (
    <tr className={ROW}>
      <td className={TD}>
        <Link
          className={LINK}
          to={`/slow-queries?instance=${encodeURIComponent(instance.id)}`}
          title={instance.id}
        >
          {shortInstance(instance.id)}
        </Link>
        {instance.collectible ? null : (
          <span className="ml-2 text-xs text-zinc-400">수집 대상 아님</span>
        )}
      </td>
      <td className={TD}>{instance.env}</td>
      <td className={TD}>
        <StateBadge kind="instance" value={instance.state} />
      </td>
      <td className={`${TD} ${MONO} text-zinc-400`}>
        {instance.engine} {instance.engine_version}
      </td>
      <td className={TD_NUM} title={gap === null ? undefined : `비율 없음: ${gap}`}>
        {fmtRate(metrics?.qps)}
      </td>
      <td className={TD_NUM}>{fmtRate(metrics?.slow_per_sec)}</td>
      <td className={TD_NUM}>{fmtInt(metrics?.threads_running)}</td>
      <td className={TD_NUM}>{fmtInt(metrics?.threads_connected)}</td>
      <td className={TD_NUM}>{fmtInt(metrics?.lock_waits)}</td>
      <td className={TD}>
        <Sparkline
          values={history}
          label={`${shortInstance(instance.id)} 최근 QPS 추이`}
        />
      </td>
      <td className={`${TD_NUM} ${MUTED}`}>{fmtClock(metrics?.at_ms)}</td>
    </tr>
  );
}

interface SummaryProps {
  instances: readonly InstanceView[];
  status: Readonly<Record<string, LiveMetrics>>;
}

function Summary({ instances, status }: SummaryProps) {
  // 한 번만 훑는다. 인스턴스 수는 작지만 필터를 세 번 도는 습관이 표를 키운다.
  //
  // **표본이 없으면 합계도 `null`** 이다. `0` 으로 초기화하면 "지표가 아직
  // 안 왔다" 가 "스레드가 0개다" 로 보인다 — 옆 칸의 `—` 와도 모순된다.
  let collecting = 0;
  let qpsTotal: number | null = null;
  let running: number | null = null;
  for (const instance of instances) {
    if (instance.state === "collecting") collecting += 1;
    const metrics = status[instance.id];
    if (metrics === undefined) continue;
    if (metrics.qps !== null) qpsTotal = (qpsTotal ?? 0) + metrics.qps;
    if (metrics.threads_running !== null) running = (running ?? 0) + metrics.threads_running;
  }

  return (
    <dl className="grid grid-cols-2 gap-3 sm:grid-cols-4">
      <Tile label="인스턴스" value={fmtInt(instances.length)} />
      <Tile label="수집 중" value={fmtInt(collecting)} />
      <Tile label="합계 QPS" value={fmtRate(qpsTotal)} />
      <Tile label="실행 중 스레드" value={fmtInt(running)} />
    </dl>
  );
}

function Tile({ label, value }: { label: string; value: string }) {
  return (
    <div className={`${CARD} p-3`}>
      <dt className={LABEL}>{label}</dt>
      <dd className="mt-1 text-2xl tabular-nums">{value}</dd>
    </div>
  );
}
