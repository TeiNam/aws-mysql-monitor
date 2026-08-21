import { AlertTriangle, Sparkles } from "lucide-react";
import { MONO } from "./ui";
import { fmtListTime } from "../lib/format";
import { formatSql } from "../lib/sql";
import type { TuningAdvice } from "../lib/types";

/**
 * AI 튜닝 권고 표시.
 *
 * # 왜 마크다운을 렌더하지 않는가
 *
 * 모델에게 **구조**(JSON)를 받는다. 마크다운을 받아 렌더하면 (a) 마크다운 파서를 넣어야
 * 하고 (b) 모델이 형식을 흔들 때마다 화면이 흔들리고 (c) "없는 테이블에 인덱스를 걸라"
 * 같은 내용 검증을 할 수 없다. 구조를 받으면 서버가 검증하고 화면은 그리기만 한다.
 *
 * 다운로드용 마크다운은 **서버가** 같은 구조에서 만든다(`core::tuning::render_markdown`) —
 * 화면과 문서가 갈라지지 않는다.
 *
 * # 제안과 사실을 섞지 않는다
 *
 * 머리에 모델 이름과 신뢰도를 박고, "제안이지 사실이 아니다" 를 적는다. DDL 은 복사용이고
 * 이 도구는 실행하지 않는다(FR-AI-09).
 */
export function TuningPanel({ advice }: { advice: TuningAdvice }) {
  return (
    <section className="mt-5 rounded-lg border border-violet-200 bg-violet-50/40 p-4">
      <header className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <h3 className="flex items-center gap-2 text-sm font-bold text-gray-900">
          <Sparkles className="h-4 w-4 text-violet-600" />
          AI 튜닝 권장
        </h3>
        <span className="text-xs text-gray-600">
          <ConfidenceChip confidence={advice.confidence} />
          <span className="mx-1.5 text-gray-300">|</span>
          <span className={MONO} title="이 권고를 만든 모델">
            {advice.model_id}
          </span>
          <span className="mx-1.5 text-gray-300">|</span>
          테이블 {advice.tables_analyzed}개
          <span className="mx-1.5 text-gray-300">|</span>
          {fmtListTime(advice.created_at_ms, "KST", Date.now())}
        </span>
      </header>

      <p className="text-sm leading-relaxed text-gray-800">{advice.summary}</p>

      {advice.findings.length === 0 ? null : (
        <div className="mt-4">
          <h4 className="mb-1 text-xs font-bold tracking-wide text-gray-700 uppercase">관찰</h4>
          <ul className="space-y-1.5">
            {/* **인덱스를 키에 넣는다.** 제목만 쓰면 모델이 같은 제목을 두 번 낼 때
                하나가 사라진다 — 모델 출력은 유일성을 보장하지 않는다. */}
            {advice.findings.map((f, n) => (
              <li key={`${n}:${f.title}`} className="text-sm text-gray-800">
                <strong>{f.title}</strong>
                {f.evidence === "" ? null : <> — {f.evidence}</>}
                {f.impact === "" ? null : (
                  <span className="block text-xs text-gray-600">{f.impact}</span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}

      {advice.indexes.length === 0 ? null : (
        <div className="mt-4">
          <h4 className="mb-1 text-xs font-bold tracking-wide text-gray-700 uppercase">
            인덱스 제안
          </h4>
          {advice.indexes.map((i, n) => (
            <div key={`${n}:${i.table}`} className="mb-3">
              <div className="text-sm text-gray-800">
                <span className={MONO}>{i.table}</span> ({i.columns.join(", ")})
                {i.covering ? (
                  <span className="ml-1.5 rounded bg-violet-100 px-1.5 py-0.5 text-xs text-violet-800">
                    커버링
                  </span>
                ) : null}
              </div>
              {i.rationale === "" ? null : (
                <p className="mt-0.5 text-xs text-gray-600">{i.rationale}</p>
              )}
              {i.ddl === "" ? null : (
                // **복사해서 검토하는 것이 목적이다.** 실행 버튼을 두지 않는다.
                <pre className="mt-1 overflow-x-auto rounded-md bg-white p-2 font-mono text-xs whitespace-pre text-gray-800 ring-1 ring-gray-200">
                  {formatSql(i.ddl)}
                </pre>
              )}
            </div>
          ))}
        </div>
      )}

      {advice.rewrite === null ? null : (
        <div className="mt-4">
          <h4 className="mb-1 text-xs font-bold tracking-wide text-gray-700 uppercase">
            쿼리 재작성
          </h4>
          {advice.rewrite.rationale === "" ? null : (
            <p className="text-xs text-gray-600">{advice.rewrite.rationale}</p>
          )}
          <pre className="mt-1 overflow-x-auto rounded-md bg-white p-2 font-mono text-xs whitespace-pre text-gray-800 ring-1 ring-gray-200">
            {formatSql(advice.rewrite.sql)}
          </pre>
        </div>
      )}

      {advice.verification.length === 0 ? null : (
        <div className="mt-4">
          <h4 className="mb-1 text-xs font-bold tracking-wide text-gray-700 uppercase">
            검증 방법
          </h4>
          <ol className="list-inside list-decimal space-y-0.5 text-sm text-gray-800">
            {advice.verification.map((v, n) => (
              <li key={`${n}:${v}`}>{v}</li>
            ))}
          </ol>
        </div>
      )}

      {advice.caveats.length === 0 ? null : (
        <div className="mt-4 rounded-md bg-amber-50 p-3">
          <h4 className="mb-1 flex items-center gap-1.5 text-xs font-bold text-amber-900">
            <AlertTriangle className="h-3.5 w-3.5" /> 주의
          </h4>
          <ul className="list-inside list-disc space-y-0.5 text-xs text-amber-900">
            {advice.caveats.map((c, n) => (
              <li key={`${n}:${c}`}>{c}</li>
            ))}
          </ul>
        </div>
      )}

      <p className="mt-3 text-xs text-gray-600">
        <strong>제안이지 사실이 아니다.</strong> DDL 은 검토 후 직접 적용한다 — 이 도구는
        스키마를 바꾸지 않는다. 마크다운을 내려받으면 이 절이 문서 끝에 함께 들어간다.
      </p>
    </section>
  );
}

/** 신뢰도. **색만으로 구분하지 않는다** — 글자를 함께 둔다. */
function ConfidenceChip({ confidence }: { confidence: TuningAdvice["confidence"] }) {
  const tone =
    confidence === "high"
      ? "bg-green-100 text-green-800"
      : confidence === "medium"
        ? "bg-amber-100 text-amber-800"
        : "bg-gray-200 text-gray-700";
  return (
    <span className={`rounded px-1.5 py-0.5 text-xs font-medium ${tone}`}>
      신뢰도 {confidence}
    </span>
  );
}
