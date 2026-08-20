import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * 마지막 방어선.
 *
 * # 왜 필요한가
 *
 * 렌더 중 예외가 하나 나면 React 는 트리를 **전부 언마운트**한다. 그러면 화면이
 * 하얗게 비고, 모니터링 도구에서 그건 "서버가 죽었나?" 로 오해된다. 실제로는
 * 프레임 하나가 예상과 달랐을 뿐일 수 있다.
 *
 * 방어적 파싱([`../lib/live-reduce`])이 1차 방어이고, 이건 그걸 빠져나간 것을
 * **말이 되는 화면으로** 바꾼다. 오류를 숨기지 않는다 — 메시지를 보여 준다.
 *
 * 클래스 컴포넌트인 이유: React 19 에도 `componentDidCatch` 를 대신할 훅이 없다.
 */
interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  override state: State = { error: null };

  static getDerivedStateFromError(error: unknown): State {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  override componentDidCatch(error: Error, info: ErrorInfo): void {
    // 콘솔이 유일한 로그 경로다. 프론트 오류를 서버로 보내는 경로는 없다
    // (있으면 그게 인증 없는 쓰기 엔드포인트가 된다).
    console.error("화면 렌더 실패", error, info.componentStack);
  }

  override render(): ReactNode {
    const { error } = this.state;
    if (error === null) return this.props.children;

    return (
      <div className="min-h-screen bg-zinc-950 p-6 text-zinc-100">
        <div className="mx-auto max-w-2xl rounded-lg border border-rose-500/40 bg-rose-500/10 p-4">
          <h1 className="text-base font-semibold text-rose-100">화면을 그리지 못했다</h1>
          <p className="mt-2 text-sm text-rose-200/90">
            서버가 죽은 것이 아니라 <strong>프론트 렌더가 실패</strong>했다. 백엔드 응답
            형식이 바뀌었을 수 있다. 콘솔에 스택이 남아 있다.
          </p>
          <pre className="mt-3 overflow-x-auto rounded bg-zinc-950/60 p-3 font-mono text-xs text-rose-100">
            {error.message}
          </pre>
          <button
            type="button"
            onClick={() => window.location.reload()}
            className="mt-3 rounded border border-rose-400/40 px-3 py-1.5 text-sm text-rose-100 hover:bg-rose-500/20"
          >
            새로 고침
          </button>
        </div>
      </div>
    );
  }
}
