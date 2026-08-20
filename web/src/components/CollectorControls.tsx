import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Activity, Pause, Play, RefreshCw } from "lucide-react";
import { ErrorNotice } from "./Notices";
import { BTN_GHOST, BTN_GREEN, BTN_RED } from "./ui";
import {
  fetchCollectorStatus,
  pauseCollector,
  queryKeys,
  resumeCollector,
  runBackfill,
  runDiscovery,
} from "../lib/api";
import { EMPTY, fmtListTime } from "../lib/format";
import type { CollectorStatus } from "../lib/types";

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
 * 수집 제어 버튼. 참조 대시보드의 `Start / Stop Monitoring` 에 대응한다.
 *
 * # 왜 "시작" 이 아니라 "재개" 인가
 *
 * 이 수집기는 리스를 잡은 워커가 항상 돈다 — 켜는 것이 아니라 **멈춘 것을 되돌리는**
 * 것이다. 그래서 라벨을 재개로 두고, 지금 상태를 그대로 보여준다.
 *
 * # 버튼을 숨기지 않는다
 *
 * 권한이 없으면 비활성화하고 이유를 말한다. 숨기면 "왜 없지" 를 코드에서 찾게 된다.
 */
export function CollectorControls() {
  const status = useCollectorStatus();
  const queryClient = useQueryClient();

  // **훅을 헬퍼 함수로 감싸지 않는다.** 호출 순서가 코드에 보여야 하고,
  // 감싸면 조건부 호출이 끼어들 여지가 생긴다.
  const onSuccess = (next: CollectorStatus) =>
    queryClient.setQueryData(queryKeys.collectorStatus, next);
  const pause = useMutation({ mutationFn: pauseCollector, onSuccess });
  const resume = useMutation({ mutationFn: resumeCollector, onSuccess });
  const discover = useMutation({ mutationFn: runDiscovery, onSuccess });

  const s = status.data;
  const busy = pause.isPending || resume.isPending || discover.isPending;
  const canControl = s?.can_control ?? false;
  const reason = canControl ? undefined : `권한이 없다 (역할: ${s?.role ?? "?"} — operator 이상 필요)`;

  return (
    <div className="flex flex-wrap items-center gap-3">
      {s === undefined ? (
        <span className="text-sm text-gray-500">상태 확인 중…</span>
      ) : (
        <>
          <span className="flex items-center gap-2 text-sm">
            <Activity
              className={`h-4 w-4 ${
                s.paused ? "text-amber-500" : s.is_leader ? "animate-pulse text-green-600" : "text-gray-400"
              }`}
            />
            <strong className="font-medium">
              {s.paused ? "일시정지" : s.is_leader ? "수집 중" : "standby"}
            </strong>
            <span className="text-gray-600">
              인스턴스 {s.collecting}개 · 마지막 tick {fmtListTime(s.last_tick_ms, "KST", Date.now())}
            </span>
          </span>

          {s.paused ? (
            <button
              type="button"
              className={BTN_GREEN}
              disabled={busy || !canControl}
              title={reason}
              onClick={() => resume.mutate()}
            >
              <Play className="h-4 w-4" /> 수집 재개
            </button>
          ) : (
            <button
              type="button"
              className={BTN_RED}
              disabled={busy || !canControl}
              title={reason}
              onClick={() => pause.mutate()}
            >
              <Pause className="h-4 w-4" /> 수집 정지
            </button>
          )}
          <button
            type="button"
            className={BTN_GHOST}
            disabled={busy || !canControl}
            title={reason ?? "탐색을 지금 한 번 돌린다"}
            onClick={() => discover.mutate()}
          >
            <RefreshCw className={`h-4 w-4 ${discover.isPending ? "animate-spin" : ""}`} />
            인스턴스 수집
          </button>
        </>
      )}
      {pause.error === null && resume.error === null && discover.error === null ? null : (
        <ErrorNotice error={pause.error ?? resume.error ?? discover.error} />
      )}
    </div>
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
