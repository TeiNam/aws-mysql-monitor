import { useQuery, useQueryClient } from "@tanstack/react-query";
import { BarChart3, CloudCog, Database, ExternalLink, Server, Share2 } from "lucide-react";
import { useEffect } from "react";
import { NavLink, Outlet, useLocation } from "react-router";
import { useLive } from "../hooks/useLive";
import { fetchAwsInfo, queryKeys } from "../lib/api";
import { liveClient } from "../lib/live";
import type { ConnState } from "../lib/live-reduce";
import { PAGE } from "./ui";

/**
 * 탭을 바꾸면 **화면 맨 위에서 시작한다.**
 *
 * # 왜 필요한가 (실측)
 *
 * 라우터는 스크롤을 건드리지 않는다. 그래서 시작 위치가 **그 화면을 전에 본 적이
 * 있는지에 따라 달라졌다**:
 *
 * | 상황 | 결과 |
 * |---|---|
 * | 첫 방문 (데이터 로딩 중 = 짧은 화면) | 브라우저가 스크롤을 0 으로 조인다 |
 * | 다시 방문 (조회 캐시가 있어 즉시 전체 높이) | **이전 스크롤 600px 이 그대로 유지** |
 *
 * 즉 같은 탭을 눌러도 어떤 때는 맨 위, 어떤 때는 중간에서 시작한다.
 *
 * # `pathname` 만 본다
 *
 * 검색 파라미터(필터·월·선택한 레코드)까지 보면 **필터를 만질 때마다 화면이 위로
 * 튄다** — 이름 조각을 한 글자 칠 때마다 점프하는 셈이다. 그건 지금 문제보다 나쁘다.
 *
 * 대가: 뒤로 가기에서도 맨 위로 간다(브라우저의 위치 복원을 덮는다). 5탭 도구에서는
 * "항상 같은 자리에서 시작" 이 더 중요하다고 판단했다.
 */
function useScrollToTopOnTabChange() {
  const { pathname } = useLocation();
  useEffect(() => {
    // 즉시 이동한다. `smooth` 는 탭 전환에서 굼벵이처럼 느껴진다.
    window.scrollTo(0, 0);
  }, [pathname]);
}

/** 참조 대시보드와 같은 5탭. 순서까지 같다 — 손이 기억하는 위치다. */
const NAV = [
  { to: "/mysql", label: "MySQL Monitor", Icon: Database },
  { to: "/plan", label: "Plan Visualization", Icon: Share2 },
  { to: "/cloudwatch", label: "CloudWatch", Icon: CloudCog },
  { to: "/statistics", label: "Statistics", Icon: BarChart3 },
  { to: "/rds", label: "RDS Instances", Icon: Server },
] as const;

/**
 * 껍데기 — 상단 내비게이션, AWS 정보, 푸터.
 *
 * **여기서 실시간 스냅샷을 구독하지 않는다.** 구독하면 5초마다 오는 지표 하나에
 * `<Outlet />` 아래 화면 전체가 리렌더된다. 실시간 값이 필요한 조각만 잎에서 본다.
 */
export function Shell() {
  useScrollToTopOnTabChange();

  return (
    <div className="flex min-h-screen flex-col bg-gray-100">
      <header className="bg-white shadow-md">
        <div className={`${PAGE} flex h-16 items-center justify-between`}>
          <div className="flex items-center gap-8">
            <span className="flex items-center gap-2 text-xl font-bold text-gray-900">
              <Database className="h-6 w-6 text-blue-600" />
              MySQL Query Monitor
            </span>
            <nav aria-label="주요 화면" className="flex gap-6">
              {NAV.map(({ to, label, Icon }) => (
                <NavLink
                  key={to}
                  to={to}
                  className={({ isActive }) =>
                    `inline-flex items-center gap-2 border-b-2 pt-1 pb-0.5 text-sm font-medium ${
                      isActive
                        ? "border-blue-600 text-blue-700"
                        : "border-transparent text-gray-700 hover:text-blue-600"
                    }`
                  }
                >
                  <Icon className="h-4 w-4" />
                  {label}
                </NavLink>
              ))}
            </nav>
          </div>
          <div className="flex items-center gap-4">
            <AwsBadge />
            <StreamBadge />
          </div>
        </div>
        <StreamNotices />
      </header>

      <main className={`${PAGE} flex-1 py-6`}>
        <Outlet />
      </main>

      <footer className="mt-auto bg-white shadow-inner">
        <div className={`${PAGE} flex flex-col items-center gap-1 py-4 text-sm text-gray-600`}>
          <span>MySQL 슬로우 쿼리 모니터 — Rust + React</span>
          <span className="flex items-center gap-2">
            Created by TeiNam
            <a
              href="https://github.com/TeiNam"
              target="_blank"
              rel="noopener noreferrer"
              className="text-gray-500 hover:text-gray-800"
              aria-label="GitHub"
            >
              <ExternalLink className="h-4 w-4" />
            </a>
          </span>
        </div>
      </footer>
    </div>
  );
}

/** 어느 계정·리전을 보고 있는가. 계정 착각이 이런 도구에서 가장 비싼 실수다. */
function AwsBadge() {
  const info = useQuery({
    queryKey: queryKeys.awsInfo,
    queryFn: ({ signal }) => fetchAwsInfo(signal),
    staleTime: Infinity,
  });

  if (info.data === undefined) {
    return <span className="text-sm text-gray-500">AWS 정보 확인 중…</span>;
  }
  const { account_id, region, deployment_env } = info.data;
  return (
    <span className="text-sm text-gray-700">
      <strong className="font-medium">Account:</strong> {account_id}
      <span className="mx-1.5 text-gray-300">|</span>
      <strong className="font-medium">Region:</strong> {region}
      <span className="mx-1.5 text-gray-300">|</span>
      <EnvChip env={deployment_env} />
    </span>
  );
}

/**
 * 환경 색을 **전 화면 고정**한다 ([09 §9]): prd 빨강, stg 주황, dev 파랑.
 *
 * 색만으로 구분하지 않고 글자를 함께 둔다 — 색각 이상에서도 읽혀야 한다.
 */
export function EnvChip({ env }: { env: string }) {
  const tone =
    env === "prd"
      ? "bg-red-50 text-red-700 ring-red-200"
      : env === "stg"
        ? "bg-orange-50 text-orange-700 ring-orange-200"
        : env === "dev"
          ? "bg-blue-50 text-blue-700 ring-blue-200"
          : "bg-gray-100 text-gray-700 ring-gray-300";
  return (
    <span className={`rounded px-1.5 py-0.5 text-xs font-medium ring-1 ${tone}`}>{env}</span>
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
  idle: "bg-gray-400",
  connecting: "bg-amber-400",
  open: "bg-green-500",
  closed: "bg-red-500",
  unauthorized: "bg-red-500",
};

/** WS 연결 상태. 실시간 메시지마다 리렌더되는 것은 이 조각뿐이다. */
function StreamBadge() {
  const live = useLive();
  const queryClient = useQueryClient();
  const needsRetry = live.conn === "closed" || live.conn === "unauthorized";

  // **인증이 거부되면 화면의 데이터도 버린다.**
  //
  // 스트림 데이터는 스냅샷이 비우지만(`live-reduce`), HTTP 로 받아 둔 목록·통계는
  // react-query 가 들고 있다. 백엔드는 fail closed 인데 화면만 옛 데이터를 계속
  // 보여 주면 강등된 사용자가 그걸 계속 본다.
  //
  // ⚠ `clear()` 가 아니라 `resetQueries()` 다. `clear()` 는 캐시에서 지우지만
  // **이미 마운트된 화면은 마지막 결과를 계속 그린다** — 표가 그대로 남는다.
  // 이 회귀를 `app.test.tsx` 가 잡는다(화면 재구성 때 실제로 빠뜨렸다).
  useEffect(() => {
    if (live.conn === "unauthorized") void queryClient.resetQueries();
  }, [live.conn, queryClient]);
  return (
    <span className="flex items-center gap-2 text-xs text-gray-600">
      <span aria-live="polite" className="flex items-center gap-1.5">
        <span className={`size-2 rounded-full ${CONN_TONE[live.conn]}`} aria-hidden="true" />
        {CONN_LABEL[live.conn]}
      </span>
      {needsRetry ? (
        <button
          type="button"
          onClick={() => liveClient.reconnectNow()}
          className="rounded border border-gray-300 px-2 py-0.5 hover:bg-gray-100"
        >
          다시 연결
        </button>
      ) : null}
    </span>
  );
}

const STREAM_ERRORS: Record<string, string> = {
  malformed: "서버가 우리 메시지를 이해하지 못했다. 프로토콜 버전이 어긋났다.",
  malformed_frame: "서버 메시지를 읽지 못했다. 프론트와 백엔드 버전이 어긋났을 수 있다.",
  auth_timeout: "인증 메시지가 5초 안에 닿지 않았다.",
  binary_not_supported: "이진 프레임을 보냈다 — 클라이언트 버그다.",
  already_authenticated: "인증이 중복 전송됐다 — 클라이언트 버그다.",
};

/**
 * 스트림 경고. **조용히 삼키지 않는다** — 거부된 구독과 프로토콜 오류는
 * "데이터가 없다" 와 전혀 다른 사실이다.
 */
function StreamNotices() {
  const live = useLive();
  const code = live.errorCode;
  return (
    <>
      {live.denied.length === 0 ? null : (
        <p className={`${PAGE} border-t border-amber-200 bg-amber-50 py-2 text-xs text-amber-800`}>
          구독이 거부된 토픽 {live.denied.length}개:{" "}
          <span className="font-mono">{live.denied.join(", ")}</span> — 환경 스코프 밖이거나
          등록부에 없는 인스턴스다.
        </p>
      )}
      {code === null || code === "unauthorized" ? null : (
        <p
          className={`${PAGE} border-t border-red-200 bg-red-50 py-2 text-xs text-red-800`}
          role="alert"
        >
          실시간 스트림 오류: <span className="font-mono">{code}</span>
          {STREAM_ERRORS[code] === undefined ? null : ` — ${STREAM_ERRORS[code]}`}
        </p>
      )}
    </>
  );
}
