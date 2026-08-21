import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Activity, Pause, Play, RefreshCw, X } from "lucide-react";
import { useState } from "react";
import { ErrorNotice } from "./Notices";
import { visibleEnvs } from "./Filters";
import { BTN_GHOST, BTN_GREEN, BTN_RED, LABEL, SELECT } from "./ui";
import {
  fetchCollectorStatus,
  fetchInstances,
  pauseCollector,
  queryKeys,
  resumeCollector,
  runBackfill,
  runDiscovery,
} from "../lib/api";
import { EMPTY, fmtListTime } from "../lib/format";
import {
  ALL_SCOPE,
  canControlScope,
  scopeLabel,
  scopeOptions,
  supportsScopedPause,
} from "../lib/pause";
import type { CollectorStatus, InstanceView } from "../lib/types";

/** 상태를 이만큼마다 다시 읽는다. 참조 구현도 30초였다. */
const POLL_MS = 30_000;

export function useCollectorStatus() {
  return useQuery({
    queryKey: queryKeys.collectorStatus,
    queryFn: ({ signal }) => fetchCollectorStatus(signal),
    refetchInterval: POLL_MS,
  });
}

/**
 * 정지·재개 mutation. **상단 컨트롤과 표의 행 버튼이 같은 것을 쓴다.**
 *
 * 각자 `useMutation` 을 두면 한쪽만 `setQueryData` 를 빠뜨려 **누른 뒤 배지가 안 바뀌는**
 * 화면이 생긴다(폴링 30초를 기다려야 반영된다). 응답이 곧 최신 상태이므로 캐시를
 * 그 자리에서 갈아 끼우는 것이 요점이다.
 */
export function usePauseControls() {
  const queryClient = useQueryClient();
  const onSuccess = (next: CollectorStatus) =>
    queryClient.setQueryData(queryKeys.collectorStatus, next);
  const pause = useMutation({ mutationFn: pauseCollector, onSuccess });
  const resume = useMutation({ mutationFn: resumeCollector, onSuccess });
  return {
    pause,
    resume,
    busy: pause.isPending || resume.isPending,
    error: pause.error ?? resume.error,
  };
}

/**
 * 수집기가 지금 무엇을 하고 있나. **읽기 전용이다.**
 *
 * # 왜 버튼과 분리했는가
 *
 * 조작은 RDS 인스턴스 화면 한 곳에서만 한다(스코프를 고르려면 인스턴스 목록이
 * 필요하고, 같은 버튼이 두 화면에 있으면 "여기서 누른 게 저기에도 적용되나" 를
 * 매번 되묻게 된다). 하지만 **"지금 수집이 도는가" 는 슬로우 쿼리 화면에서도 보여야
 * 한다** — 표가 비어 있을 때 그게 정지 때문인지 데이터가 없어서인지 구분해야 한다.
 */
export function CollectorBadge() {
  const status = useCollectorStatus();
  const s = status.data;
  if (s === undefined) return <span className="text-sm text-gray-500">상태 확인 중…</span>;

  const partial = s.paused_scopes.length > 0 && !s.paused;
  const [color, text] = s.paused
    ? ["text-amber-500", "전체 정지"]
    : partial
      ? ["text-amber-500", `부분 정지 (${s.paused_scopes.length})`]
      : s.is_leader
        ? ["animate-pulse text-green-600", "수집 중"]
        : ["text-gray-400", "standby"];

  return (
    <span className="flex items-center gap-2 text-sm">
      <Activity className={`h-4 w-4 ${color}`} />
      <strong className="font-medium">{text}</strong>
      <span className="text-gray-600">
        인스턴스 {s.collecting}개 · 마지막 tick {fmtListTime(s.last_tick_ms, "KST", Date.now())}
      </span>
    </span>
  );
}

/**
 * 수집 제어. 참조 대시보드의 `Start / Stop Monitoring` 에 대응한다.
 *
 * # 왜 "시작" 이 아니라 "재개" 인가
 *
 * 이 수집기는 리스를 잡은 워커가 항상 돈다 — 켜는 것이 아니라 **멈춘 것을 되돌리는**
 * 것이다. 그래서 라벨을 재개로 두고, 지금 상태를 그대로 보여준다.
 *
 * # 왜 스코프를 고르는가
 *
 * 스위치 하나면 "prd 만 멈춰 두고 dev 는 계속 본다" 를 표현할 수 없어 유지보수
 * 창구가 전부-아니면-전무가 된다. 그래서 전체 / 환경 / 인스턴스 개별을 고른다.
 *
 * # 버튼을 숨기지 않는다
 *
 * 권한이 없으면 비활성화하고 이유를 말한다. 숨기면 "왜 없지" 를 코드에서 찾게 된다.
 */
export function CollectorControls() {
  const status = useCollectorStatus();
  // **인스턴스 목록을 여기서 읽는다.** 같은 `queryKey` 라 화면이 이미 읽었으면
  // 요청이 한 번 더 나가지 않는다(react-query 캐시). 프롭으로 받으면 이 컴포넌트를
  // 쓰는 화면마다 배선이 늘어난다. 칩 이름(인스턴스 → 사람이 읽는 이름)에 필요하다.
  const instanceQuery = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });
  const instances: readonly InstanceView[] = instanceQuery.data ?? [];
  const queryClient = useQueryClient();
  const [scope, setScope] = useState(ALL_SCOPE);

  const { pause, resume, busy: pauseBusy, error: pauseError } = usePauseControls();
  const discover = useMutation({
    mutationFn: runDiscovery,
    onSuccess: (next: CollectorStatus) =>
      queryClient.setQueryData(queryKeys.collectorStatus, next),
  });

  const s = status.data;
  const busy = pauseBusy || discover.isPending;
  // **인스턴스는 여기 없다.** 개별 정지는 표의 행 버튼이 한다 — 등록부가 500대면
  // 셀렉터가 500줄이 되고, 그중 하나를 찾는 것보다 표에서 그 행을 보는 것이 빠르다.
  const options = scopeOptions(visibleEnvs(instances), s);
  const pausedKeys = new Set((s?.paused_scopes ?? []).map((p) => p.scope));
  const selectedPaused = pausedKeys.has(scope);
  const canSelected = canControlScope(scope, s, instances);

  // **왜 못 누르는지 구분해 말한다.** 대응이 다르다 — 하나는 역할을 받아야 하고,
  // 하나는 그 환경 스코프를 받아야 한다.
  const pauseReason = canSelected
    ? undefined
    : s === undefined
      ? "상태를 아직 읽지 못했다"
      : !supportsScopedPause(s)
        ? // **여기서 막지 않으면 전체가 멈춘다.** 구 서버는 스코프 본문을 무시한다.
          `이 서버는 스코프 정지를 모른다 (scope=${s.scope}) — 워커를 새 버전으로 올려야 한다`
        : s.controllable_envs.length === 0
          ? `권한이 없다 (역할: ${s.role}, operator 이상 필요)`
          : `이 스코프의 환경 권한이 없다 (가능: ${s.controllable_envs.join(", ")})`;
  // 즉시 탐색은 **이 워커가 수집기여야** 한다 — 프로세스 플래그를 세우는 요청이다.
  const discoverReason = !(s?.can_control ?? false)
    ? `권한이 없다 (역할: ${s?.role ?? "?"}, 전 환경 스코프 필요)`
    : s?.runs_collector === false
      ? "이 워커는 수집기가 아니다 (role=api) — 수집 워커에서 조작한다"
      : "탐색을 지금 한 번 돌린다";
  const canDiscover = (s?.can_control ?? false) && s?.runs_collector !== false;

  const error = pauseError ?? discover.error;

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap items-center gap-3">
        <CollectorBadge />

        <label className={LABEL}>
          <select
            className={SELECT}
            value={scope}
            onChange={(e) => setScope(e.target.value)}
            aria-label="정지 대상 선택"
          >
            {options
              .filter((o) => o.group === "all")
              .map((o) => (
                <option key={o.key} value={o.key} disabled={o.disabled}>
                  {o.label}
                </option>
              ))}
            <ScopeGroup label="환경" options={options.filter((o) => o.group === "env")} />
          </select>
        </label>

        {selectedPaused ? (
          <button
            type="button"
            className={BTN_GREEN}
            disabled={busy || !canSelected}
            title={pauseReason}
            onClick={() => resume.mutate(scope)}
          >
            <Play className="h-4 w-4" /> 수집 재개
          </button>
        ) : (
          <button
            type="button"
            className={BTN_RED}
            disabled={busy || !canSelected}
            title={pauseReason}
            onClick={() => pause.mutate(scope)}
          >
            <Pause className="h-4 w-4" /> 수집 정지
          </button>
        )}
        <button
          type="button"
          className={BTN_GHOST}
          disabled={busy || !canDiscover}
          title={discoverReason}
          onClick={() => discover.mutate()}
        >
          <RefreshCw className={`h-4 w-4 ${discover.isPending ? "animate-spin" : ""}`} />
          인스턴스 수집
        </button>
      </div>

      {/* **멈춰 있는 것을 전부 보여준다.** 셀렉터만 두면 "지금 무엇이 멈춰 있나" 를
          하나씩 골라 보며 확인해야 한다 — 그건 화면이 답해야 하는 질문이다. */}
      {s !== undefined && s.paused_scopes.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2 text-xs">
          <span className="text-gray-600">정지 중:</span>
          {s.paused_scopes.map((p) => {
            const can = canControlScope(p.scope, s, instances);
            return (
              <span
                key={p.scope}
                className="inline-flex items-center gap-1 rounded-full bg-amber-100 px-2 py-0.5 text-amber-900"
                title={`${p.scope} · ${fmtListTime(p.since_ms, "KST", Date.now())} 부터`}
              >
                {scopeLabel(p.scope, instances)}
                <button
                  type="button"
                  className="text-amber-700 hover:opacity-70 disabled:cursor-not-allowed disabled:opacity-50"
                  disabled={busy || !can}
                  title={can ? "이 스코프만 재개한다" : "이 스코프의 환경 권한이 없다"}
                  aria-label={`${scopeLabel(p.scope, instances)} 재개`}
                  onClick={() => resume.mutate(p.scope)}
                >
                  <X className="h-3 w-3" />
                </button>
              </span>
            );
          })}
        </div>
      ) : null}

      {error === null ? null : <ErrorNotice error={error} />}
    </div>
  );
}

/** 선택지가 없으면 `<optgroup>` 자체를 만들지 않는다 — 빈 그룹은 셀렉터를 어지럽힌다. */
function ScopeGroup({
  label,
  options,
}: {
  label: string;
  options: readonly { key: string; label: string; disabled: boolean }[];
}) {
  if (options.length === 0) return null;
  return (
    <optgroup label={label}>
      {options.map((o) => (
        <option key={o.key} value={o.key} disabled={o.disabled}>
          {o.label}
        </option>
      ))}
    </optgroup>
  );
}

/** 상태 상세. 카드 본문에 놓는다. */
export function CollectorFacts({ status }: { status: CollectorStatus | undefined }) {
  if (status === undefined) return null;
  const at = (ms: number | null) => (ms === null ? EMPTY : fmtListTime(ms, "KST", Date.now()));
  return (
    <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm md:grid-cols-4">
      <Fact label="워커">{status.worker_id}</Fact>
      <Fact label="리더">{status.is_leader ? "예" : "아니오"}</Fact>
      <Fact label="마지막 탐색">{at(status.last_discovery_ms)}</Fact>
      <Fact label="마지막 백필">{at(status.last_backfill_ms)}</Fact>
      {status.paused_since_ms === null ? null : (
        <Fact label="정지 시작">{at(status.paused_since_ms)}</Fact>
      )}
    </dl>
  );
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <dt className="text-xs font-medium tracking-wide text-gray-500 uppercase">{label}</dt>
      <dd className="mt-0.5 text-gray-800">{children}</dd>
    </div>
  );
}

/** 백필을 지금 돌리는 버튼. CloudWatch 화면에 놓는다. */
export function BackfillButton() {
  const status = useCollectorStatus();
  const queryClient = useQueryClient();
  const run = useMutation({
    mutationFn: runBackfill,
    onSuccess: (next) => queryClient.setQueryData(queryKeys.collectorStatus, next),
  });
  const canControl = status.data?.can_control ?? false;

  return (
    <div className="flex flex-wrap items-center gap-3">
      <button
        type="button"
        className={BTN_GHOST}
        disabled={run.isPending || !canControl}
        title={
          canControl
            ? "다음 주기를 기다리지 않고 지금 슬로우로그를 읽는다"
            : status.data?.runs_collector === false
              ? "이 워커는 수집기가 아니다 (role=api)"
              : `권한이 없다 (역할: ${status.data?.role ?? "?"})`
        }
        onClick={() => run.mutate()}
      >
        <RefreshCw className={`h-4 w-4 ${run.isPending ? "animate-spin" : ""}`} />
        지금 수집
      </button>
      <span className="text-sm text-gray-600">
        마지막 백필: {fmtListTime(status.data?.last_backfill_ms ?? null, "KST", Date.now())}
        {status.data?.backfill_requested === true ? " · 요청 대기 중" : ""}
      </span>
      {run.error === null ? null : <ErrorNotice error={run.error} />}
    </div>
  );
}
