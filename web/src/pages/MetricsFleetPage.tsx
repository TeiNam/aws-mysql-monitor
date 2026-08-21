import { useQuery } from "@tanstack/react-query";
import { Database, Gauge } from "lucide-react";
import { useMemo } from "react";
import { useNavigate } from "react-router";
import { Card } from "../components/Card";
import { EmptyRow, ErrorNotice, Note, Pending } from "../components/Notices";
import { PageHeader } from "../components/PageHeader";
import { EnvChip } from "../components/Shell";
import { Sparkline } from "../components/Sparkline";
import {
  CELL_ICON,
  COL_TIGHT,
  COL_TIGHT_CAPPED,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { useLive, useLiveTopics } from "../hooks/useLive";
import { fetchFleetMetrics, fetchInstances, queryKeys } from "../lib/api";
import { EMPTY, fmtInt, fmtRate } from "../lib/format";
import { MAX_TOPICS } from "../lib/live-reduce";
import {
  connReason,
  connSeverity,
  formatConn,
  formatMetric,
  metricSeverity,
  severityClass,
  severityReason,
} from "../lib/metrics";

/**
 * 플릿 메트릭 표 — **한 줄에 두 출처를 섞는다.**
 *
 * # 왜 전부 CloudWatch 에서 가져오지 않는가 (비용이다)
 *
 * 500대 × 15메트릭을 60초로 폴링하면 월 $3,240 이다([06 §2.3]). 그래서
 *
 * | 값 | 출처 | 신선도 | 비용 |
 * |---|---|---|---|
 * | 연결·QPS·슬로우/초·스레드·락 대기 | **자체 수집** (WS 스냅샷) | 5초 | 0 |
 * | CPU·여유 메모리·스토리지/볼륨 | CloudWatch | 15분 | 엔진별 3메트릭 |
 *
 * 연결 수는 CloudWatch 에도 있지만(`DatabaseConnections`) **쓰지 않는다** — 자체 수집이
 * 더 신선하고(CloudWatch 는 1~3분 지연) 무료다.
 *
 * # 왜 두 신선도를 한 표에 두는가
 *
 * 나누면 "CPU 가 튄 시점에 연결이 몇이었나" 를 두 화면을 번갈아 보며 맞춰야 한다.
 * 대신 **열 머리에 출처를 적어** 값이 어긋나 보일 때 이유를 알 수 있게 한다.
 */
export function MetricsFleetPage() {
  const navigate = useNavigate();
  const live = useLive();
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });
  const fleet = useQuery({
    queryKey: queryKeys.fleetMetrics,
    queryFn: ({ signal }) => fetchFleetMetrics(signal),
    // **폴링 주기를 조회 주기에 맞춘다.** 더 자주 물어도 같은 창의 캐시가 오고,
    // 서버가 CloudWatch 를 다시 때리지도 않는다 — 그래도 무의미한 요청은 줄인다.
    refetchInterval: 15 * 60_000,
  });

  const rows = instances.data ?? [];
  // **지표 토픽을 구독해야 자체 수집 열에 값이 온다.** 구독하지 않으면 스냅샷이 비어
  // 전부 `—` 로 보이고, 그건 "값이 0" 이나 "수집이 죽었다" 로 오해된다(실측으로 그랬다).
  //
  // 이 화면은 슬로우 쿼리 방송을 쓰지 않으므로 `status:` 만 구독한다 — 상한(50)을
  // 지표에 다 쓸 수 있다.
  // 상한을 넘는 인스턴스는 앞쪽만 실시간 값을 갖는다(CloudWatch 열은 전부 온다).
  const topics = useMemo(
    () => rows.slice(0, MAX_TOPICS).map((i) => `status:inst=${i.id}`),
    [rows],
  );
  useLiveTopics(topics);
  // 서버 응답을 인스턴스 id 로 색인한다. 순서에 의존하면 한쪽이 필터될 때 어긋난다.
  const byId = new Map((fleet.data?.rows ?? []).map((r) => [r.instance_id, r]));
  // 열 머리는 **엔진이 섞여 있어도 하나로** 만들어야 한다. Aurora 는 `볼륨 여유`,
  // RDS 는 `여유 스토리지` 라 세 번째 열의 이름이 행마다 다르다 → 머리는 중립어로 두고
  // 값 옆에 실제 메트릭 이름을 `title` 로 남긴다.
  const cwCols = ["CPU", "여유 메모리", "스토리지/볼륨"];

  return (
    <div className="space-y-6">
      <PageHeader title="Instance Metrics" subtitle="= CloudWatch + 자체 수집 =" />

      <Card
        title={
          <>
            <Gauge className="h-5 w-5 text-gray-500" /> 모니터링 대상 전체
          </>
        }
        // **설명은 표 아래다**(`note` 를 쓰지 않는다). 넉 줄짜리 설명이 제목과 표
        // 사이에 있으면 매번 그걸 넘어가야 값에 닿는다 — 읽는 것은 한 번이고 값을
        // 보는 것은 매번이다.
      >
        {instances.error !== null ? (
          <ErrorNotice error={instances.error} onRetry={() => void instances.refetch()} />
        ) : instances.isPending ? (
          <Pending label="인스턴스를 불러오는 중…" />
        ) : (
          <div className="overflow-x-auto">
            <table className={TABLE}>
              <thead>
                <tr>
                  <th className={`${TH} ${COL_TIGHT_CAPPED}`}>Instance</th>
                  <th className={`${TH} ${COL_TIGHT}`}>Env</th>
                  {/* **자원부터, 부하는 그 다음, 상태는 맨 뒤.**
                      출처(CloudWatch / 자체 수집) 표기는 머리에서 뺐다 — 12열 표에서
                      셀마다 붙은 작은 배지가 읽기를 방해했다. 대신 표 아래 설명이 어느
                      열이 어느 출처인지 말한다. */}
                  {cwCols.map((c) => (
                    <th key={c} className={`${TH_NUM} ${COL_TIGHT}`}>
                      {c}
                    </th>
                  ))}
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>Conn</th>
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>Threads</th>
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>Lock</th>
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>QPS</th>
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>Slow/s</th>
                  <th className={`${TH} ${COL_TIGHT}`}>QPS 추이</th>
                  <th className={`${TH} ${COL_TIGHT}`}>State</th>
                </tr>
              </thead>
              <tbody className={TBODY}>
                {rows.length === 0 ? (
                  <EmptyRow colSpan={12}>
                    등록된 인스턴스가 없다. 탐색이 아직 돌지 않았거나 필터가 전부 제외했다.
                  </EmptyRow>
                ) : (
                  rows.map((i) => {
                    const m = live.status[i.id];
                    const cw = byId.get(i.id);
                    return (
                      <tr
                        key={i.id}
                        className={`${TR} cursor-pointer`}
                        onClick={() => navigate(`/instance?instance=${encodeURIComponent(i.id)}`)}
                      >
                        <td className={`${TD} ${COL_TIGHT_CAPPED}`} title={i.id}>
                          <span className="flex items-center">
                            <Database className={CELL_ICON} />
                            {i.name}
                          </span>
                        </td>
                        <td className={TD}>
                          <EnvChip env={i.env} />
                        </td>
                        {/* **엔진마다 세 번째 메트릭이 다르다**(RDS 는 여유 스토리지,
                            Aurora 는 볼륨 여유). 값 옆에 실제 메트릭 이름을 `title` 로
                            남긴다 — 머리만 보면 무엇인지 알 수 없다.

                            색은 **분모가 있는 값만** 칠한다(`metricSeverity`). 억지로
                            절대 임계를 쓰면 건강한 인스턴스를 빨갛게 만들고, 그러면
                            아무도 색을 믿지 않게 된다. */}
                        {[0, 1, 2].map((idx) => {
                          const p = cw?.metrics[idx];
                          if (p === undefined) {
                            return (
                              <td key={idx} className={TD_NUM}>
                                {EMPTY}
                              </td>
                            );
                          }
                          const sev = metricSeverity(p.name, p.value, i.allocated_storage_gb);
                          return (
                            <td
                              key={idx}
                              className={`${TD_NUM} ${severityClass(sev)}`}
                              title={
                                severityReason(p.name, p.value, i.allocated_storage_gb) ?? p.name
                              }
                            >
                              {formatMetric(p.value, p.unit)}
                            </td>
                          );
                        })}
                        {/* **연결은 모수를 함께 적는다** — `6 / 60`. 개수만 있으면 색이
                            왜 그 색인지 알 수 없고, 모수는 인스턴스마다 다르다.
                            모수(`@@max_connections`)는 자체 수집이 읽어 온다 —
                            CloudWatch 에는 이 값의 메트릭이 없다. */}
                        <td
                          className={`${TD_NUM} ${severityClass(
                            connSeverity(m?.threads_connected ?? null, m?.max_connections ?? null),
                          )}`}
                          title={connReason(
                            m?.threads_connected ?? null,
                            m?.max_connections ?? null,
                          )}
                        >
                          {formatConn(m?.threads_connected ?? null, m?.max_connections ?? null)}
                        </td>
                        <td className={TD_NUM}>{fmtInt(m?.threads_running)}</td>
                        <td className={TD_NUM}>{fmtInt(m?.lock_waits)}</td>
                        <td className={TD_NUM}>{fmtRate(m?.qps)}</td>
                        <td className={TD_NUM}>{fmtRate(m?.slow_per_sec)}</td>
                        <td className={TD}>
                          <Sparkline
                            values={live.qpsHistory[i.id] ?? []}
                            label={`${i.name} QPS 추이`}
                          />
                        </td>
                        <td className={TD}>{i.state}</td>
                      </tr>
                    );
                  })
                )}
              </tbody>
            </table>
          </div>
        )}
        {/* 실패는 설명보다 먼저 말한다 — 값이 비어 보이는 이유가 그것일 수 있다. */}
        {fleet.error === null ? null : (
          <Note>
            CloudWatch 값을 가져오지 못했다 — 자체 수집 열은 그대로 동작한다. 태스크 롤에{" "}
            <span className="font-mono">cloudwatch:GetMetricData</span> 가 있는지 확인한다.
          </Note>
        )}
        <Note>
          <strong>두 출처를 한 줄에 놓는다.</strong> 연결·QPS·스레드는{" "}
          <strong>자체 수집</strong>(5초, 비용 0)이고, CPU·메모리·스토리지는{" "}
          <strong>CloudWatch</strong>(15분)다. CloudWatch 는 1~3분 지연되므로 두 열의 값이
          어긋나 보일 수 있다 — 실시간은 왼쪽을 본다. 행을 누르면 그 인스턴스의 전체
          메트릭으로 간다.
        </Note>
      </Card>
    </div>
  );
}
