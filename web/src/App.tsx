import { Navigate, Route, Routes } from "react-router";
import { Note } from "./components/Notices";
import { Shell } from "./components/Shell";
import { CloudWatchPage } from "./pages/CloudWatchPage";
import { MySQLMonitorPage } from "./pages/MySQLMonitorPage";
import { PlanVisualizationPage } from "./pages/PlanVisualizationPage";
import { RDSInstancePage } from "./pages/RDSInstancePage";
import { StatisticsPage } from "./pages/StatisticsPage";

/**
 * 라우트. **참조 대시보드(`my_slow_query_dashboard`)와 같은 5화면**이다.
 *
 * 경로까지 같게 뒀다(`/mysql`·`/plan`·`/cloudwatch`·`/statistics`·`/rds`) —
 * 기존 도구의 북마크와 손이 기억하는 위치를 그대로 쓴다.
 *
 * 코드 스플리팅을 하지 않는 이유: 화면이 다섯이고 합쳐도 작다. `React.lazy` 는
 * 라우트 전환마다 로딩 상태를 하나 더 만든다 — 조사 도구에서 그건 손해다.
 */
export function App() {
  return (
    <Routes>
      <Route element={<Shell />}>
        <Route index element={<Navigate to="/mysql" replace />} />
        <Route path="mysql" element={<MySQLMonitorPage />} />
        <Route path="plan" element={<PlanVisualizationPage />} />
        <Route path="cloudwatch" element={<CloudWatchPage />} />
        <Route path="statistics" element={<StatisticsPage />} />
        <Route path="rds" element={<RDSInstancePage />} />
        <Route path="*" element={<Note>그런 화면이 없다.</Note>} />
      </Route>
    </Routes>
  );
}
