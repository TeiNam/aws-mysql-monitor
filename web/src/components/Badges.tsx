/**
 * 상태 배지.
 *
 * **모르는 값을 숨기지 않는다.** 백엔드 열거형이 늘어나면(`InstanceState` 는 이미
 * 7가지다) 매핑에 없는 값이 오는데, 그때 빈 칸을 그리면 "상태가 없다" 로 읽힌다.
 * 매핑에 없으면 원문을 회색으로 그대로 보여 준다.
 */

const BASE = "inline-flex items-center rounded px-1.5 py-0.5 text-xs whitespace-nowrap ring-1";
const UNKNOWN_TONE = "bg-zinc-700/30 text-zinc-300 ring-zinc-600/40";

/** 슬로우 쿼리 상태. 값은 `SlowQueryView.state` (`inflight`·`finalized`·`abandoned`). */
const QUERY_STATE: Record<string, { label: string; tone: string }> = {
  inflight: {
    label: "진행 중",
    tone: "bg-amber-500/15 text-amber-300 ring-amber-500/30",
  },
  finalized: {
    label: "확정",
    tone: "bg-emerald-500/15 text-emerald-300 ring-emerald-500/30",
  },
  abandoned: {
    // 유령 쿼리가 아니라 **관측이 끊긴 사실**이다. 실패로 읽히지 않게 색을 구분한다.
    label: "추적 끊김",
    tone: "bg-violet-500/15 text-violet-300 ring-violet-500/30",
  },
};

/** 인스턴스 상태. 값은 `InstanceView.state`. */
const INSTANCE_STATE: Record<string, { label: string; tone: string }> = {
  collecting: { label: "수집 중", tone: "bg-emerald-500/15 text-emerald-300 ring-emerald-500/30" },
  pending: { label: "대기", tone: "bg-sky-500/15 text-sky-300 ring-sky-500/30" },
  degraded: { label: "일부 소스 중단", tone: "bg-amber-500/15 text-amber-300 ring-amber-500/30" },
  unreachable: { label: "접속 불가", tone: "bg-rose-500/15 text-rose-300 ring-rose-500/30" },
  unsupported: { label: "미지원 버전", tone: UNKNOWN_TONE },
  disabled: { label: "수집 끔", tone: UNKNOWN_TONE },
};

interface BadgeProps {
  value: string;
  kind: "query" | "instance";
}

export function StateBadge({ value, kind }: BadgeProps) {
  const table = kind === "query" ? QUERY_STATE : INSTANCE_STATE;
  const known = table[value];
  return (
    <span className={`${BASE} ${known?.tone ?? UNKNOWN_TONE}`} title={value}>
      {known?.label ?? value}
    </span>
  );
}
