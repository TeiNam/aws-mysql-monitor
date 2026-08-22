import { Navigate, Route, Routes } from "react-router";
import { Note } from "./components/Notices";
import { Shell } from "./components/Shell";
import { AuthCallbackPage } from "./pages/AuthCallbackPage";
import { CloudWatchPage } from "./pages/CloudWatchPage";
import { InstanceMetricsPage } from "./pages/InstanceMetricsPage";
import { MetricsFleetPage } from "./pages/MetricsFleetPage";
import { MySQLMonitorPage } from "./pages/MySQLMonitorPage";
import { OptionsPage } from "./pages/OptionsPage";
import { PlanVisualizationPage } from "./pages/PlanVisualizationPage";
import { RDSInstancePage } from "./pages/RDSInstancePage";
import { StatisticsPage } from "./pages/StatisticsPage";

/**
 * 라우트. 참조 대시보드(`my_slow_query_dashboard`)의 5화면 + 메트릭 2개 + 옵션이다.
 *
 * 경로까지 같게 뒀다(`/mysql`·`/plan`·`/cloudwatch`·`/statistics`·`/rds`) —
 * 기존 도구의 북마크와 손이 기억하는 위치를 그대로 쓴다. `/options` 는 참조에 없던
 * 화면이므로 **맨 끝**이다 — 앞에 끼우면 익숙한 탭 위치가 밀린다.
 *
 * 코드 스플리팅을 하지 않는 이유: 화면이 다섯이고 합쳐도 작다. `React.lazy` 는
 * 라우트 전환마다 로딩 상태를 하나 더 만든다 — 조사 도구에서 그건 손해다.
 */
export function App() {
  return (
    <Routes>
      {/*
       * **셸 밖이다.** 이 화면은 인증되기 전에 렌더되므로, 셸 안에 두면 셸의 데이터
       * 조회가 전부 401 을 받으면서 교환이 끝나기 전에 "토큰이 필요하다" 배너가 뜬다.
       */}
      <Route path="auth/callback" element={<AuthCallbackPage />} />
      <Route element={<Shell />}>
        {/* 첫 탭과 같은 곳으로 보낸다 — 3번째 탭에서 열리면 실수처럼 보인다. */}
        <Route index element={<Navigate to="/metrics" replace />} />
        <Route path="mysql" element={<MySQLMonitorPage />} />
        <Route path="plan" element={<PlanVisualizationPage />} />
        <Route path="cloudwatch" element={<CloudWatchPage />} />
        {/* 메트릭 두 화면. `/metrics` 는 플릿 표, `/instance` 는 그 한 대의 전체 메트릭. */}
        <Route path="metrics" element={<MetricsFleetPage />} />
        <Route path="instance" element={<InstanceMetricsPage />} />
        <Route path="statistics" element={<StatisticsPage />} />
        <Route path="rds" element={<RDSInstancePage />} />
        <Route path="options" element={<OptionsPage />} />
        <Route path="*" element={<Note>그런 화면이 없다.</Note>} />
      </Route>
    </Routes>
  );
}
