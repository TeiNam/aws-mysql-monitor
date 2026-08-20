// `vitest/config` 의 `defineConfig` 를 쓰는 이유: `vite` 쪽 것은 `test` 필드를
// 모르므로 타입 검사에서 걸린다.
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

/**
 * 개발 서버는 `/api` 와 `/api/ws` 를 로컬 dbmon 으로 넘긴다.
 *
 * 프록시를 쓰는 이유: 같은 오리진으로 만들어야 **CORS 설정을 만들지 않아도 된다.**
 * 백엔드에 CORS 를 열면 그 설정이 프로덕션에도 따라가고, 그건 필요 없는 표면이다.
 */
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8080",
        changeOrigin: true,
        // WebSocket 업그레이드도 넘긴다 — 없으면 `/api/ws` 가 404 다.
        ws: true,
      },
    },
  },
  build: {
    // Rust 바이너리가 이 디렉터리를 정적 서빙한다. 런타임 이미지에 노드를
    // 넣지 않기 위해 빌드 산출물만 넘긴다.
    outDir: "dist",
    // 라우트별 코드 스플리팅은 Vite 기본값(동적 import)에 맡긴다.
    sourcemap: true,
  },
  test: {
    // DOM 없는 순수 함수 테스트도 있지만, 컴포넌트 하나를 렌더하는 테스트가
    // 있으므로 jsdom 으로 통일한다 — 파일마다 환경을 지정하면 빠뜨린다.
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
  },
});
