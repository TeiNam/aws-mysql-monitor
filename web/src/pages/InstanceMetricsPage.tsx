import { useQuery } from "@tanstack/react-query";
import { Clock, Gauge } from "lucide-react";
import { useSearchParams } from "react-router";
import { useInstances } from "../hooks/useInstances";
import { Card } from "../components/Card";
import { ErrorNotice, Note, Pending } from "../components/Notices";
import { PageHeader } from "../components/PageHeader";
import { EnvChip } from "../components/Shell";
import { Sparkline } from "../components/Sparkline";
import { LABEL, SELECT } from "../components/ui";
import { fetchInstanceMetrics, queryKeys } from "../lib/api";
import { fmtListTime } from "../lib/format";
import { formatMetric } from "../lib/metrics";
import type { MetricSeries } from "../lib/types";

/** 조회 범위. **CloudWatch 의 최소 period 규칙과 맞물린다** — 서버가 자동 조정한다. */
const RANGES = [
  { label: "1시간", ms: 3_600_000 },
  { label: "3시간", ms: 3 * 3_600_000 },
  { label: "12시간", ms: 12 * 3_600_000 },
  { label: "3일", ms: 3 * 86_400_000 },
  { label: "2주", ms: 14 * 86_400_000 },
  { label: "30일", ms: 30 * 86_400_000 },
] as const;

/**
 * 인스턴스 하나의 CloudWatch 메트릭 전체.
 *
 * # 엔진에 따라 개수가 다르다
 *
 * 공통 13개 + RDS 4개 = 17, 공통 13개 + Aurora 14개 = 27. **없는 메트릭은 요청하지
 * 않는다** — 빈 값이 와도 요청 수로 과금되기 때문이다(카탈로그가 엔진으로 가른다).
 *
 * # 왜 화면을 열 때만 조회하는가
 *
 * 전 인스턴스를 상시 폴링하면 이 경로가 청구서를 지배한다([06 §2.3]). 그래서 상세는
 * **보고 있을 때만** 가져오고 서버가 60초 창으로 캐시한다 — 여러 사람이 같은 화면을 봐도
 * CloudWatch 는 1회만 맞는다.
 */
export function InstanceMetricsPage() {
  const [params, setParams] = useSearchParams();
  const selected = params.get("instance") ?? "";
  const rangeMs = Number.parseInt(params.get("range") ?? "", 10) || RANGES[1].ms;

  // **머리말의 리전 범위가 적용된 목록**이다(`useInstances`). 화면마다 직접
  // 조회하면 리전 필터를 한 곳만 빠뜨려도 그 화면에서 범위 밖이 보인다.
  const instances = useInstances();
  const list = instances.data ?? [];
  // 고른 것이 없으면 첫 인스턴스를 본다. **없는 id 를 다른 것으로 갈아치우지 않는다** —
  // 오래된 링크로 들어왔을 때 엉뚱한 인스턴스의 메트릭을 보여주면 안 된다.
  const picked = selected === "" ? list[0]?.id : list.find((i) => i.id === selected)?.id;
  const missing = selected !== "" && picked === undefined && !instances.isPending;
  const instance = list.find((i) => i.id === picked);

  const metrics = useQuery({
    queryKey: queryKeys.instanceMetrics(picked ?? "", rangeMs),
    queryFn: ({ signal }) => fetchInstanceMetrics(picked ?? "", rangeMs, signal),
    enabled: picked !== undefined,
    // 서버가 60초 창으로 캐시한다 — 그보다 자주 물어도 CloudWatch 를 다시 때리지 않는다.
    refetchInterval: 60_000,
  });

  function update(key: string, value: string) {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  }

  return (
    <div className="space-y-6">
      <PageHeader title="Instance Detail" subtitle="= CloudWatch 전체 메트릭 =" />

      <Card
        title={
          <>
            <Gauge className="h-5 w-5 text-gray-500" />
            {instance === undefined ? "인스턴스" : instance.name}
            {instance === undefined ? null : <EnvChip env={instance.env} />}
          </>
        }
        actions={
          <>
            <label className={LABEL}>
              <select
                className={SELECT}
                value={picked ?? ""}
                onChange={(e) => update("instance", e.target.value)}
                aria-label="인스턴스 선택"
              >
                {list.map((i) => (
                  <option key={i.id} value={i.id}>
                    {i.name} ({i.env})
                  </option>
                ))}
              </select>
            </label>
            <label className={LABEL}>
              <Clock className="h-4 w-4 text-gray-500" />
              <select
                className={SELECT}
                value={String(rangeMs)}
                onChange={(e) => update("range", e.target.value)}
                aria-label="조회 범위"
              >
                {RANGES.map((r) => (
                  <option key={r.ms} value={r.ms}>
                    {r.label}
                  </option>
                ))}
              </select>
            </label>
          </>
        }
        note={
          metrics.data === undefined ? (
            <>엔진에 맞는 메트릭만 조회한다 — 없는 메트릭은 요청하지 않는다(빈 값에도 과금된다).</>
          ) : (
            <>
              엔진 <span className="font-mono">{metrics.data.engine}</span> ·{" "}
              {metrics.data.series.length}개 메트릭 · 해상도{" "}
              <strong>{metrics.data.period_secs}초</strong>
              {metrics.data.period_adjusted ? (
                <span className="ml-2 text-amber-700">
                  (요청 구간이 오래돼 CloudWatch 가 이 해상도만 허용한다 — 15~63일 전은 300초,
                  63일 전보다 오래되면 3600초)
                </span>
              ) : null}
              . 마지막 갱신{" "}
              {fmtListTime(metrics.data.to_ms, "KST", Date.now())}
            </>
          )
        }
      >
        {missing ? (
          <Note>
            주소로 지정한 인스턴스가 목록에 없다 — 삭제됐거나 환경 스코프 밖이다.{" "}
            <strong>다른 인스턴스의 메트릭을 대신 보여주지 않는다.</strong>
          </Note>
        ) : metrics.error !== null ? (
          <ErrorNotice error={metrics.error} onRetry={() => void metrics.refetch()} />
        ) : metrics.isPending || metrics.data === undefined ? (
          <Pending label="메트릭을 불러오는 중…" />
        ) : (
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {metrics.data.series.map((s) => (
              <MetricTile key={s.name} series={s} />
            ))}
          </div>
        )}
      </Card>
    </div>
  );
}

/**
 * 메트릭 하나. 최신값 + 추이 선.
 *
 * **빈 계열을 숨기지 않는다.** `ReplicaLag`(소스 인스턴스), `BurstBalance`(gp3),
 * `EngineCPUUtilization`(일부 인스턴스 클래스)처럼 **그 구성에서 발행되지 않는** 메트릭이
 * 있다. 숨기면 "이 메트릭이 없는 건가, 값이 0인가, 화면이 깨진 건가" 를 알 수 없다.
 */
function MetricTile({ series }: { series: MetricSeries }) {
  const latest = series.values.at(-1) ?? null;
  const empty = series.values.length === 0;
  return (
    <div className="rounded-lg border border-gray-200 bg-white p-3">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-medium text-gray-600" title={series.name}>
          {series.label}
        </span>
        <span className="font-mono text-[10px] text-gray-400">{series.stat}</span>
      </div>
      <div className="mt-1 text-lg font-semibold text-gray-900 tabular-nums">
        {formatMetric(latest, series.unit)}
      </div>
      <div className="mt-1">
        {empty ? (
          <span className="text-xs text-gray-500">이 구성에서 발행되지 않는다</span>
        ) : (
          <Sparkline
            values={series.values}
            label={`${series.label} 추이`}
            width={180}
            height={32}
          />
        )}
      </div>
    </div>
  );
}
