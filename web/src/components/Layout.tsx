import { NavLink, Outlet } from "react-router";
import { useLive } from "../hooks/useLive";
import { liveClient } from "../lib/live";
import type { ConnState, LiveSnapshot } from "../lib/live-reduce";

const NAV = [
  { to: "/", label: "플릿" },
  { to: "/slow-queries", label: "슬로우 쿼리" },
  { to: "/live", label: "실시간" },
] as const;

export function Layout() {
  const live = useLive();

  return (
    <div className="min-h-screen bg-zinc-950 text-zinc-100">
      <header className="border-b border-zinc-800 bg-zinc-900/80 backdrop-blur">
        <div className="mx-auto flex max-w-7xl flex-wrap items-center gap-x-6 gap-y-2 px-4 py-3">
          <span className="font-semibold tracking-tight">dbmon</span>
          <nav aria-label="주요 화면" className="flex gap-1">
            {NAV.map((item) => (
              <NavLink
                key={item.to}
                to={item.to}
                // `end` 없으면 `/` 링크가 모든 경로에서 활성으로 표시된다.
                end={item.to === "/"}
                className={({ isActive }) =>
                  `rounded px-3 py-1.5 text-sm ${
                    isActive ? "bg-zinc-800 text-zinc-50" : "text-zinc-400 hover:text-zinc-100"
                  }`
                }
              >
                {item.label}
              </NavLink>
            ))}
          </nav>
          <div className="ml-auto flex items-center gap-3">
            <UserBadge user={live.user} />
            <ConnBadge conn={live.conn} />
          </div>
        </div>
        <DeniedBanner denied={live.denied} />
      </header>

      <main className="mx-auto max-w-7xl px-4 py-6">
        <Outlet />
      </main>
    </div>
  );
}

const CONN_LABEL: Record<ConnState, string> = {
  idle: "대기",
  connecting: "연결 중",
  open: "실시간",
  closed: "연결 끊김",
  unauthorized: "인증 실패",
};

const CONN_TONE: Record<ConnState, string> = {
  idle: "bg-zinc-700",
  connecting: "bg-amber-400",
  open: "bg-emerald-400",
  closed: "bg-rose-500",
  unauthorized: "bg-rose-500",
};

function ConnBadge({ conn }: { conn: ConnState }) {
  const needsRetry = conn === "unauthorized" || conn === "closed";
  return (
    <span className="flex items-center gap-2 text-xs text-zinc-400">
      {/* 상태 변화는 눈으로만 보이면 안 된다 — 스크린 리더에도 알린다. */}
      <span aria-live="polite" className="flex items-center gap-1.5">
        <span className={`size-2 rounded-full ${CONN_TONE[conn]}`} aria-hidden="true" />
        {CONN_LABEL[conn]}
      </span>
      {needsRetry ? (
        <button
          type="button"
          onClick={() => liveClient.reconnectNow()}
          className="rounded border border-zinc-700 px-2 py-0.5 hover:bg-zinc-800"
        >
          다시 연결
        </button>
      ) : null}
    </span>
  );
}

function UserBadge({ user }: { user: LiveSnapshot["user"] }) {
  if (user === null) return null;
  return (
    <span className="text-xs text-zinc-400" title={`subject=${user.subject}`}>
      {user.role}
      <span className="text-zinc-600"> · </span>
      {user.env_scope.join(", ")}
      {user.can_see_literals ? (
        <span className="ml-2 rounded bg-amber-500/15 px-1.5 py-0.5 text-amber-300">
          리터럴 열람
        </span>
      ) : null}
    </span>
  );
}

/**
 * 거부된 구독을 **알린다.** 조용히 빼면 "데이터가 없다" 와 "볼 권한이 없다" 를
 * 구분할 수 없다 — 서버가 `denied` 를 따로 주는 이유가 그것이다.
 */
function DeniedBanner({ denied }: { denied: readonly string[] }) {
  if (denied.length === 0) return null;
  return (
    <p className="border-t border-amber-500/30 bg-amber-500/10 px-4 py-2 text-xs text-amber-200">
      구독이 거부된 토픽 {denied.length}개: <span className="font-mono">{denied.join(", ")}</span>
      <span className="text-amber-200/70">
        {" "}
        — 환경 스코프 밖이거나 등록부에 없는 인스턴스다.
      </span>
    </p>
  );
}
