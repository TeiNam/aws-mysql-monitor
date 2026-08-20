import { Route, Routes } from "react-router";
import { Layout } from "./components/Layout";
import { Note } from "./components/Notices";
import { Fleet } from "./routes/Fleet";
import { Live } from "./routes/Live";
import { SlowQueries } from "./routes/SlowQueries";
import { SlowQueryDetail } from "./routes/SlowQueryDetail";

/**
 * 라우트.
 *
 * **백엔드에 없는 것은 화면도 만들지 않는다** — 다이제스트·알림·리포트·부트스트랩·
 * CloudWatch 지표·Athena 콜드 티어는 조회 경로가 없다. 빈 화면을 만들면
 * "데이터가 없다" 로 오해되므로 라우트 자체를 두지 않는다([21 §3.2]).
 *
 * 코드 스플리팅을 하지 않는 이유: 화면이 넷이고 합쳐도 작다. `React.lazy` 는
 * 라우트 전환마다 로딩 상태를 하나 더 만든다 — 실시간 화면에서 그건 손해다.
 */
export function App() {
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route index element={<Fleet />} />
        <Route path="slow-queries" element={<SlowQueries />} />
        <Route path="slow-queries/:id" element={<SlowQueryDetail />} />
        <Route path="live" element={<Live />} />
        <Route path="*" element={<Note>그런 화면이 없다.</Note>} />
      </Route>
    </Routes>
  );
}
