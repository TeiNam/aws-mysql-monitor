import { useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import { NavLink, Outlet } from "react-router";
import { useLive } from "../hooks/useLive";
import { liveClient } from "../lib/live";
import type { ConnState, LiveSnapshot } from "../lib/live-reduce";

const NAV = [
  { to: "/", label: "플릿" },
  { to: "/slow-queries", label: "슬로우 쿼리" },
  { to: "/live", label: "실시간" },
] as const;

/**
 * 껍데기.
 *
 * **여기서 실시간 스냅샷을 구독하지 않는다.** 구독하면 5초마다 오는 지표 하나에
 * `<Outlet />` 아래 화면 전체가 리렌더된다. 실시간 값이 필요한 조각([`LiveStatus`],
 * [`LiveBanners`])만 잎에서 구독한다.
 */
export function Layout() {
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
            <LiveStatus />
          </div>
        </div>
        <LiveBanners />
      </header>

      <main className="mx-auto max-w-7xl px-4 py-6">
        <Outlet />
      </main>
    </div>
  );
}

/** 사용자·연결 상태. 실시간 메시지마다 리렌더되는 것은 이 조각뿐이다. */
function LiveStatus() {
  const live = useLive();
  const queryClient = useQueryClient();

  // **인증이 거부되면 캐시도 버린다.**
  //
  // 스트림 데이터는 스냅샷이 비우지만(`live-reduce`), HTTP 로 받아 둔 목록·상세는
  // react-query 캐시에 남는다. 백엔드는 fail closed 인데 화면만 옛 데이터를 계속
  // 보여 주면 강등된 사용자가 그걸 계속 본다.
  useEffect(() => {
    if (live.conn === "unauthorized") queryClient.clear();
  }, [live.conn, queryClient]);

  return (
    <>
      <UserBadge user={live.user} />
      <ConnBadge conn={live.conn} />
    </>
  );
}

/** 알림 배너. 같은 이유로 잎에서 구독한다. */
function LiveBanners() {
  const live = useLive();
  return (
    <>
      <DeniedBanner denied={live.denied} />
      <StreamErrorBanner code={live.errorCode} />
    </>
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
 * 스트림 프로토콜 오류.
 *
 * `unauthorized` 는 연결 배지가 이미 말하고 있으므로 중복해서 띄우지 않는다.
 * 나머지는 **프론트와 백엔드가 어긋났다는 신호**다 — 조용히 삼키면 "데이터가
 * 안 온다" 로만 보이고, 그게 이 프로젝트에서 반복된 오진의 출발점이었다.
 */
const STREAM_ERRORS: Record<string, string> = {
  malformed: "서버가 우리 메시지를 이해하지 못했다. 프로토콜 버전이 어긋났다.",
  auth_timeout: "인증 메시지가 5초 안에 닿지 않았다.",
  binary_not_supported: "이진 프레임을 보냈다 — 클라이언트 버그다.",
  already_authenticated: "인증이 중복 전송됐다 — 클라이언트 버그다.",
};

function StreamErrorBanner({ code }: { code: string | null }) {
  if (code === null || code === "unauthorized") return null;
  return (
    <p
      className="border-t border-rose-500/30 bg-rose-500/10 px-4 py-2 text-xs text-rose-200"
      role="alert"
    >
      실시간 스트림 오류: <span className="font-mono">{code}</span>
      {STREAM_ERRORS[code] === undefined ? null : ` — ${STREAM_ERRORS[code]}`}
    </p>
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
