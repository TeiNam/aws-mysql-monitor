import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { Link, useParams } from "react-router";
import { StateBadge } from "../components/Badges";
import { ErrorNotice, Note, Pending } from "../components/Notices";
import { SqlBlock } from "../components/SqlBlock";
import { CARD, LABEL, LINK, MONO } from "../components/styles";
import { ApiError, fetchSlowQuery, queryKeys } from "../lib/api";
import { EMPTY, fmtDateTime, fmtDuration, fmtInt, shortInstance } from "../lib/format";

/** 슬로우 쿼리 상세. `GET /api/queries/{id}`. */
export function SlowQueryDetail() {
  const { id } = useParams();
  const recordId = id ?? "";
  const detail = useQuery({
    queryKey: queryKeys.slowQuery(recordId),
    queryFn: ({ signal }) => fetchSlowQuery(recordId, signal),
    enabled: recordId !== "",
  });

  // 라우트 형태상 오지 않아야 하지만, 오면 조용히 빈 화면을 그리지 않는다.
  if (recordId === "") return <ErrorNotice error={new ApiError(400, "invalid_record_id")} />;
  if (detail.isPending) return <Pending label="상세를 불러오는 중…" />;
  if (detail.error !== null) {
    return <ErrorNotice error={detail.error} onRetry={() => void detail.refetch()} />;
  }

  const q = detail.data;

  return (
    <article>
      <nav className="mb-4 text-sm">
        <Link className={LINK} to="/slow-queries">
          ← 목록으로
        </Link>
      </nav>

      <header className="mb-4 flex flex-wrap items-center gap-3">
        <h1 className="text-lg font-semibold tracking-tight">
          {fmtDuration(q.duration_ms)} · {q.statement_type}
        </h1>
        <StateBadge kind="query" value={q.state} />
        <span className="text-sm text-zinc-400" title={q.instance_id}>
          {shortInstance(q.instance_id)} ({q.env})
        </span>
      </header>

      {q.abandoned_reason === null ? null : (
        <p className={`${CARD} mb-4 border-violet-500/40 bg-violet-500/10 p-3 text-sm text-violet-200`}>
          추적이 끊긴 레코드다: <span className="font-mono">{q.abandoned_reason}</span> — 쿼리가
          실패한 것이 아니라 <strong>관측이 중단된 사실</strong>이다.
        </p>
      )}

      <div className={`${CARD} p-4`}>
        <dl className="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-3 lg:grid-cols-4">
          <Item label="시작">{fmtDateTime(q.started_at_ms)}</Item>
          <Item label="실행시간">
            {fmtDuration(q.duration_ms)}
            <span className="ml-1 text-xs text-zinc-500">({q.duration_source})</span>
          </Item>
          <Item label="캡처 소스">{q.capture_source}</Item>
          {/* 스레드 id 는 수량이 아니라 식별자다 — 자리수 구분 기호를 넣지 않는다. */}
          <Item label="스레드">
            <span className={MONO}>{q.thread_id}</span>
          </Item>
          <Item label="스키마">{q.schema_name ?? EMPTY}</Item>
          <Item label="DB 사용자">{q.db_user ?? EMPTY}</Item>
          <Item label="조사 행">{fmtInt(q.rows_examined)}</Item>
          <Item label="반환 행">{fmtInt(q.rows_sent)}</Item>
          <Item label="락 대기">{fmtDuration(q.lock_time_ms)}</Item>
          <Item label="실행계획">{q.has_plan ? "있음" : "없음"}</Item>
          <Item label="다이제스트">
            <span className={MONO}>{q.app_digest}</span>
          </Item>
          <Item label="레코드 id">
            <span className={`${MONO} break-all`}>{q.record_id}</span>
          </Item>
        </dl>
      </div>

      <h2 className="mt-6 mb-2 text-sm font-medium text-zinc-300">SQL</h2>
      <SqlBlock
        sql={q.sql_text}
        reason={q.sql_redacted_reason}
        truncated={q.sql_text_truncated}
      />

      {q.has_plan ? (
        <Note>
          실행계획이 저장돼 있지만 조회 API 가 아직 없다 — 계획 본문을 내려주는 엔드포인트는
          만들어지지 않았다.
        </Note>
      ) : null}
    </article>
  );
}

function Item({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt className={LABEL}>{label}</dt>
      <dd className="mt-0.5 text-sm text-zinc-200">{children}</dd>
    </div>
  );
}
