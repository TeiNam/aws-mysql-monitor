import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router";
import { App } from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import "./index.css";
import { ApiError } from "./lib/api";
import { takeToken } from "./lib/auth";
import { startTheme } from "./lib/theme";

// **아무것도 렌더하기 전에** 토큰을 URL 에서 걷어낸다. 라우터가 먼저 돌면
// `?token=` 이 히스토리 항목으로 남는다.
takeToken();

// 테마도 렌더 전에 붙인다 — 나중에 붙이면 첫 프레임이 라이트로 깜빡인다.
// ponytail: `index.html` 인라인 스크립트면 깜빡임이 완전히 사라지지만 키 이름이
// 두 곳에 생긴다. 번들이 defer 로 실행되는 그 한 프레임을 감수한다.
startTheme();

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // 5초 주기 지표는 WS 로 온다. HTTP 목록은 사람이 새로 고칠 때 다시 읽는다.
      staleTime: 10_000,
      refetchOnWindowFocus: true,
      retry: (failureCount, error) => {
        // 4xx 는 재시도로 풀리지 않는다. 401 은 토큰이 이미 버려졌고,
        // 400 은 같은 요청을 다시 보내도 같은 답이 온다.
        if (error instanceof ApiError && error.status < 500) return false;
        return failureCount < 2;
      },
    },
  },
});

const root = document.getElementById("root");
if (root === null) {
  // 조용히 실패하면 빈 화면만 남는다.
  throw new Error("#root 를 찾을 수 없다 — index.html 이 바뀌었다");
}

createRoot(root).render(
  <StrictMode>
    <ErrorBoundary>
      <QueryClientProvider client={queryClient}>
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </QueryClientProvider>
    </ErrorBoundary>
  </StrictMode>,
);
