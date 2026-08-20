/**
 * 슬로우 쿼리 상태 배지.
 *
 * # 왜 필요한가 (참조 대시보드에는 없다)
 *
 * 참조 구현은 상태 개념이 없었다 — 잡힌 쿼리는 다 끝난 쿼리였다. 이 수집기는
 * **진행 중**(선행 저장)과 **추적 끊김**(F4 — 관측이 중단된 사실)을 구분해 저장한다.
 * 그걸 화면에서 지우면 세 가지가 같아 보인다:
 *
 * - `inflight` — 아직 돌고 있다. 실행시간은 **지금까지**의 값이다
 * - `abandoned` — 관측이 끊겼다. 실행시간은 **하한**이고 언제 끝났는지 모른다
 * - `finalized` — 종료를 관측했다. 실행시간이 확정값이다
 *
 * 세 값을 같은 모양으로 보여주면 하한을 확정값으로 읽는다.
 */

const TONE: Record<string, { label: string; className: string }> = {
  inflight: {
    label: "진행 중",
    className: "bg-amber-50 text-amber-800 ring-amber-200",
  },
  finalized: {
    label: "확정",
    className: "bg-green-50 text-green-700 ring-green-200",
  },
  abandoned: {
    // 쿼리가 실패한 것이 아니라 **관측이 끊긴 것**이다. 실패 색을 쓰지 않는다.
    label: "추적 끊김",
    className: "bg-violet-50 text-violet-700 ring-violet-200",
  },
};

interface StateBadgeProps {
  state: string;
  /** 끊긴 사유. `finalized` 인데 값이 있으면 "한 번 끊겼다가 확정됨" 이다. */
  reason?: string | null;
}

export function StateBadge({ state, reason }: StateBadgeProps) {
  const tone = TONE[state] ?? {
    label: state,
    className: "bg-gray-100 text-gray-700 ring-gray-300",
  };
  const interrupted = state !== "abandoned" && reason !== null && reason !== undefined;

  return (
    <span className="inline-flex items-center gap-1 whitespace-nowrap">
      <span
        className={`rounded px-1.5 py-0.5 text-xs font-medium ring-1 ${tone.className}`}
        title={reason === null || reason === undefined ? state : `${state} · ${reason}`}
      >
        {tone.label}
      </span>
      {interrupted ? (
        // 확정됐지만 중간에 관측이 끊긴 이력이 있다. 지우지 않고 작게 남긴다 —
        // 정확 지표를 의심할 근거가 된다.
        <span className="text-xs text-gray-500" title={`관측이 한 번 끊겼다: ${reason}`}>
          ⚠ {reason}
        </span>
      ) : null}
    </span>
  );
}
