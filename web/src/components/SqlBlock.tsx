import type { SqlRedactedReason } from "../lib/types";

interface SqlBlockProps {
  sql: string | null;
  reason: SqlRedactedReason | null;
  truncated: boolean;
}

/**
 * SQL 본문.
 *
 * # 가려진 이유를 반드시 구분한다
 *
 * `not_stored` 는 "정책상 저장하지 않았다", `insufficient_role` 은 "저장돼 있지만
 * 당신은 볼 수 없다" 다. 둘을 같은 문장으로 뭉치면 사용자는 권한을 요청해야 할지
 * 설정을 바꿔야 할지 알 수 없다.
 */
export function SqlBlock({ sql, reason, truncated }: SqlBlockProps) {
  if (sql === null) {
    return (
      <div className="rounded border border-zinc-800 bg-zinc-950/60 p-3 text-sm">
        {reason === "insufficient_role" ? (
          <p className="text-amber-300">
            원문이 저장돼 있지만 <strong>열람 권한이 없다</strong>. 리터럴 열람 역할이
            필요하다.
          </p>
        ) : (
          <p className="text-zinc-400">
            SQL 이 <strong>저장되지 않았다</strong>. 수집 정책이{" "}
            <span className="font-mono">literal_policy=off</span> 이거나 마스킹에 실패한
            레코드다.
          </p>
        )}
      </div>
    );
  }

  return (
    <div>
      {/* `pre` 를 쓰는 이유: SQL 은 공백과 줄바꿈이 의미를 가진다. */}
      <pre className="overflow-x-auto rounded border border-zinc-800 bg-zinc-950/60 p-3 font-mono text-xs whitespace-pre-wrap text-zinc-100">
        {sql}
      </pre>
      {truncated ? (
        <p className="mt-1 text-xs text-amber-300/80">
          저장 한계로 잘렸다 — 뒷부분이 없다.
        </p>
      ) : null}
    </div>
  );
}
