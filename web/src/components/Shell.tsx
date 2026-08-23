import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  BarChart3,
  CloudCog,
  Database,
  Gauge,
  Globe,
  LineChart,
  Scale,
  Server,
  Settings,
  Share2,
} from "lucide-react";
import { useEffect, useMemo } from "react";
import { NavLink, Outlet, useLocation } from "react-router";
import { useInstances } from "../hooks/useInstances";
import { useTokenRefresh } from "../hooks/useTokenRefresh";
import { useLive } from "../hooks/useLive";
import { fetchAwsInfo, queryKeys } from "../lib/api";
import { liveClient } from "../lib/live";
import type { ConnState } from "../lib/live-reduce";
import { ALL_REGIONS, regionOfInstanceId, useRegionPicker } from "../lib/region-scope";
import { regionLabel } from "../lib/regions";
import { GithubMark } from "./GithubMark";
import { PAGE } from "./ui";

/** 공개 저장소 주소. 푸터가 출처를 말하는 유일한 자리다. */
const REPO_SLUG = "TeiNam/aws-mysql-monitor";
const REPO_URL = `https://github.com/${REPO_SLUG}`;

/**
 * 표시할 라이선스와 그 근거 위치.
 *
 * # 지금은 `LICENSE` 파일이 없다
 *
 * README 가 "Not yet licensed. Until a `LICENSE` file lands, all rights reserved" 라고
 * 적어 뒀고, `Cargo.toml` 은 `Proprietary` 다. 화면에 `MIT` 같은 것을 적으면 **없는 허가를
 * 준 것처럼 읽힌다** — 공개 저장소에서 그건 되돌리기 어렵다.
 *
 * 그래서 지금은 있는 사실만 말하고 README 의 라이선스 절을 가리킨다. 라이선스를 정하면
 * 이 두 상수와 `LICENSE` 파일만 바꾸면 된다.
 */
const LICENSE_LABEL = "All rights reserved";
const LICENSE_HREF = "#license";

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

/**
 * 상단 탭. **순서가 작업 흐름이다** — 대상을 고르고(RDS) → 플릿 상태(Metrics) →
 * 그 인스턴스(Instance) → 느린 쿼리(Slow Query) → 그 계획(Plan) → 원문 로그(Slow Log).
 *
 * 참조 대시보드는 슬로우 쿼리가 첫 탭이었다. 메트릭 화면이 생기면서 "무엇이 이상한가" 를
 * 먼저 보고 들어가는 순서로 바꿨고, 그보다 앞에 **무엇을 수집할지 정하는 화면**이 온다.
 *
 * **첫 탭이 첫 화면은 아니다.** `/` 는 `/metrics` 로 간다(`App.tsx`) — 등록은 한 번,
 * 상태 확인은 매일이다.
 *
 * **설정은 여기 없다.** 조사하는 화면들과 성격이 다르므로 오른쪽 상태 배지 옆에 둔다
 * (`OptionsLink`) — 탭 줄은 "무엇을 볼까" 만 담는다.
 */
const NAV = [
  // **RDS 가 맨 앞이다.** 등록·수집 시작·정지가 여기 있다. 아무것도 수집하지 않는
  // 상태에서 다른 탭은 전부 빈 화면이므로, 흐름의 출발점이 이 화면이다.
  { to: "/rds", label: "RDS", Icon: Server },
  { to: "/metrics", label: "Metrics", Icon: Gauge },
  { to: "/instance", label: "Instance", Icon: LineChart },
  { to: "/mysql", label: "Slow Query", Icon: Database },
  { to: "/plan", label: "Plan", Icon: Share2 },
  // **"CloudWatch" 가 아니라 "Slow Log" 다.** 이 화면은 슬로우로그 백필이고, 메트릭
  // 화면이 따로 생기면서 그 이름이 무엇을 가리키는지 알 수 없게 됐다.
  { to: "/cloudwatch", label: "Slow Log", Icon: CloudCog },
  { to: "/statistics", label: "Statistics", Icon: BarChart3 },
] as const;

/**
 * 껍데기 — 상단 내비게이션, AWS 정보, 푸터.
 *
 * **여기서 실시간 스냅샷을 구독하지 않는다.** 구독하면 5초마다 오는 지표 하나에
 * `<Outlet />` 아래 화면 전체가 리렌더된다. 실시간 값이 필요한 조각만 잎에서 본다.
 */
export function Shell() {
  useScrollToTopOnTabChange();
  // **Cognito 세션을 살려 둔다.** 없으면 60분 뒤 화면이 401 로 닫히고, 리프레시
  // 토큰(8시간)이 세션에 남아 있는데도 다시 로그인해야 한다.
  useTokenRefresh();

  return (
    <div className="flex min-h-screen flex-col bg-gray-100">
      <header className="bg-white shadow-md">
        <div className={`${PAGE} flex h-16 items-center justify-between`}>
          {/* **줄바꿈을 막는다.** 탭이 늘어나면 flex 가 라벨을 두 줄로 접는데, 그러면
              머리말 높이가 흔들리고 계정·리전 값이 겹친다. 넘칠 때는 접는 것보다
              가로로 밀리는 편이 낫다(`min-w-[1280px]` 안에서는 넘치지 않는다). */}
          <div className="flex min-w-0 items-center gap-5">
            {/* **이름 글자를 버려 탭에 폭을 넘긴다.** `MySQL Query Monitor` 는 222px,
                `dbmon` 은 96px 를 먹었다. 1280px 창에서 그 폭은 탭 간격으로 쓰는 편이
                낫다 — 무엇을 보는 도구인지는 브라우저 탭 제목과 화면 머리말이 이미
                말하고, 이름은 아무도 읽지 않지만 탭은 매번 누른다.
                아이콘은 남긴다: 머리말의 왼쪽 끝을 잡아 주고, `title` 이 이름을 준다. */}
            <span
              className="flex items-center whitespace-nowrap text-gray-900"
              title="dbmon — MySQL Query Monitor"
            >
              <Database className="h-7 w-7 text-blue-600" />
            </span>
            {/* 탭 글자는 **본문보다 크다**(`text-xl` = 20px, 이전 14px). 화면을 고르는
                것이 이 도구에서 가장 자주 하는 동작이고, 표 머리글을 굵게 키운 뒤로는
                탭이 표보다 작아 보였다.

                탭 사이 24px 은 **아이콘–라벨 간격(6px)의 네 배**다. "무엇이 한 탭인가" 를
                간격만으로 읽을 수 있어야 한다 — 안쪽이 좁고 바깥쪽이 넓다.

                28px 까지 벌렸다가 24px 로 되돌렸다: 머리말에 리전 이름
                (`ap-northeast-2 (서울)`)이 들어오면서 마지막 탭과 계정 배지 사이가
                7px 밖에 남지 않았고, 그러면 탭과 배지가 한 덩이로 읽힌다(실측).
                1280px 가 최소 폭이므로 그 폭에서 성립해야 한다. */}
            <nav aria-label="주요 화면" className="flex gap-6">
              {NAV.map(({ to, label, Icon }) => (
                <NavLink
                  key={to}
                  to={to}
                  className={({ isActive }) =>
                    `inline-flex items-center gap-1.5 border-b-2 pt-1 pb-0.5 text-xl font-medium whitespace-nowrap ${
                      isActive
                        ? "border-blue-600 text-blue-700"
                        : "border-transparent text-gray-700 hover:text-blue-600"
                    }`
                  }
                >
                  <Icon className="h-5 w-5" />
                  {label}
                </NavLink>
              ))}
            </nav>
          </div>
          <div className="flex shrink-0 items-center gap-4 whitespace-nowrap">
            <AwsBadge />
            <StreamBadge />
            <OptionsLink />
          </div>
        </div>
        <StreamNotices />
      </header>

      <main className={`${PAGE} flex-1 py-6`}>
        <Outlet />
      </main>

      <footer className="mt-auto bg-white shadow-inner">
        {/* **공개 저장소로 전환할 것을 전제로 둔다.** 이름·출처·라이선스를 화면에서 바로
            읽을 수 있어야 한다 — 스크린샷만 돌아다닐 때 그게 유일한 단서다. */}
        <div
          className={`${PAGE} flex flex-col items-center gap-1.5 py-4 text-sm text-gray-600`}
        >
          <span className="font-medium text-gray-700">
            AWS Aurora &amp; RDS MySQL Slow Query Monitor
          </span>
          <span className="flex flex-wrap items-center justify-center gap-x-4 gap-y-1">
            <span>Rust + React · Created by TeiNam</span>
            <a
              href={REPO_URL}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1.5 hover:text-gray-900"
            >
              <GithubMark />
              {REPO_SLUG}
            </a>
            {/* **라이선스를 링크로 둔다.** 문구만 적으면 근거를 확인할 곳이 없다.
                `LICENSE` 가 들어오면 [`LICENSE_LABEL`] 한 줄과 이 경로만 바꾼다. */}
            <a
              href={`${REPO_URL}${LICENSE_HREF}`}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1.5 hover:text-gray-900"
            >
              <Scale className="h-4 w-4" />
              {LICENSE_LABEL}
            </a>
          </span>
        </div>
      </footer>
    </div>
  );
}

/**
 * 설정. **탭 줄이 아니라 상태 배지 옆이다.**
 *
 * 조사 화면들과 성격이 다르고 자주 가지 않는다. 탭 줄에 두면 (a) "무엇을 볼까" 목록에
 * 설정이 끼어 익숙한 탭 위치가 밀리고 (b) 탭이 늘어날 때마다 폭 다툼을 한다.
 *
 * 밑줄(탭의 활성 표시)을 쓰지 않는다 — 배지 옆에서는 어색하다. 대신 배경·색으로 표시한다.
 */
function OptionsLink() {
  return (
    <NavLink
      to="/options"
      title="Options"
      aria-label="Options"
      className={({ isActive }) =>
        `rounded-md p-1.5 ${
          isActive
            ? "bg-blue-50 text-blue-700"
            : "text-gray-500 hover:bg-gray-100 hover:text-gray-800"
        }`
      }
    >
      <Settings className="h-4 w-4" />
    </NavLink>
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
  // **라벨을 지우고 값만 둔다.** 탭이 8개가 되면서 `Account:`·`Region:` 글자가
  // 폭을 먹어 머리말이 겹쳤다(실측). 의미는 `title` 로 남긴다 — 계정 착각을 막는 것이
  // 이 배지의 목적이므로 값 자체는 절대 줄이지 않는다.
  //
  // **배포 환경 칩은 프로덕션에서만 띄운다.** `dev` 라고 적혀 있는 것은 계정·리전이
  // 이미 보이는 상황에서 소음이다. 반대로 `prd` 는 "이 화면은 프로덕션 대시보드다" 를
  // 말하는 유일한 표시이고, 두 탭을 열어 두고 일할 때 엉뚱한 쪽을 조작하는 것을 막는다.
  // `unknown` 도 띄운다 — `Env::treat_as_production()` 이 그걸 프로덕션으로 보기 때문이다.
  const loudEnv = deployment_env === "prd" || deployment_env === "unknown";
  return (
    <span className="text-sm text-gray-700">
      <span title="AWS 계정">{account_id}</span>
      <span className="mx-1 text-gray-300">|</span>
      {/* **리전 선택기**. 탐색 범위가 여러 리전일 때만 고를 수 있게 된다 —
          하나뿐이면 고를 것이 없고, 선택기가 있으면 "다른 리전이 있나" 를 헷갈리게
          한다. 리전 코드 옆에 이름을 붙이는 것은 `ap-northeast-1` 과 `-2` 를 눈으로
          구분하기 위해서다(계정 착각과 같은 부류의 실수다). */}
      <RegionPicker deploymentRegion={region} />
      {loudEnv ? (
        <>
          <span className="mx-1 text-gray-300">|</span>
          <span title="이 대시보드의 배포 환경">
            <EnvChip env={deployment_env} />
          </span>
        </>
      ) : null}
    </span>
  );
}

/**
 * 보고 있는 리전. 탐색 범위가 **여러 리전이면 고를 수 있다.**
 *
 * # 목록은 등록부에서 얻는다
 *
 * 설정(`/api/settings`)에도 리전 목록이 있지만 그건 "탐색하기로 한 곳" 이고, 이 선택기가
 * 필터하는 대상은 "실제로 등록된 인스턴스" 다. 설정에 리전을 추가하고 탐색이 아직 돌지
 * 않았으면 그 리전에는 아무것도 없으므로, **고를 수 있게 해 두면 빈 화면이 나온다.**
 *
 * 등록부가 비어 있으면(첫 기동) 배포 리전 하나를 보여준다 — 선택기가 사라지는 것보다
 * "지금 여기를 본다" 가 보이는 편이 낫다.
 */
function RegionPicker({ deploymentRegion }: { deploymentRegion: string }) {
  const { all } = useInstances();
  const known = useMemo(() => {
    const set = new Set<string>();
    for (const i of all ?? []) {
      const r = regionOfInstanceId(i.id);
      if (r !== null) set.add(r);
    }
    if (set.size === 0) set.add(deploymentRegion);
    return [...set].sort();
  }, [all, deploymentRegion]);
  const { region, setRegion } = useRegionPicker(known);

  if (known.length < 2) {
    // 리전이 하나면 고를 것이 없다. 이름은 그래도 보여준다.
    return <span title="리전">{regionLabel(known[0] ?? deploymentRegion)}</span>;
  }
  return (
    <label className="inline-flex items-center gap-1" title="보고 있는 리전">
      <Globe className="h-4 w-4 text-gray-500" aria-hidden="true" />
      <select
        aria-label="리전 선택"
        className="rounded border border-gray-300 bg-white px-1.5 py-0.5 text-sm text-gray-800"
        value={region}
        onChange={(e) => setRegion(e.target.value)}
      >
        {/* **"전체" 가 기본이다.** 여러 리전을 모니터링하는 사람의 첫 질문은
            "어디에 문제가 있나" 이고, 그건 전부를 봐야 답이 나온다. */}
        <option value={ALL_REGIONS}>전체 ({known.length}개 리전)</option>
        {known.map((r) => (
          <option key={r} value={r}>
            {regionLabel(r)}
          </option>
        ))}
      </select>
    </label>
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
