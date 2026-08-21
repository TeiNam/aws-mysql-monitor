import { useQuery } from "@tanstack/react-query";
import {
  Calendar,
  Clock,
  Database,
  Download,
  FileJson,
  Filter,
  Hash,
  Share2,
} from "lucide-react";
import { useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { Card } from "../components/Card";
import { EmptyRow, ErrorNotice, Note, Pending } from "../components/Notices";
import { PlanGraph } from "../components/PlanGraph";
import { StateBadge } from "../components/StateBadge";
import { EnvChip } from "../components/Shell";
import {
  BTN_GHOST,
  CELL_ICON,
  COL_GROW,
  COL_TIGHT,
  COL_TIGHT_CAPPED,
  LABEL,
  MONO,
  PAGE_TITLE,
  SELECT,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { fetchInstances, fetchMarkdown, fetchPlan, fetchPlans, queryKeys } from "../lib/api";
import { EMPTY, fmtInt, fmtListTime, shortInstance, type Timezone } from "../lib/format";
import { downloadText, formatSql } from "../lib/sql";
import type { SlowQueryView } from "../lib/types";

/**
 * 실행계획 화면. 참조 대시보드의 `Plan Visualization` 과 같은 구성:
 * 최근 플랜 목록 → 선택 → 쿼리 정보 + 계획 그래프 + 원문.
 */
export function PlanVisualizationPage() {
  const [params, setParams] = useSearchParams();
  const instance = params.get("instance") ?? "";
  const tz: Timezone = params.get("tz") === "UTC" ? "UTC" : "KST";
  const selected = params.get("record") ?? "";

  const listParams = useMemo(
    () => ({ ...(instance === "" ? {} : { instance }) }),
    [instance],
  );
  const plans = useQuery({
    queryKey: queryKeys.plans(listParams),
    queryFn: ({ signal }) => fetchPlans(listParams, signal),
  });
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });

  function update(key: string, value: string) {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  }

  const items = plans.data?.items ?? [];
  // **없는 레코드를 다른 것으로 갈아치우지 않는다.** 오래된 북마크나 필터 변경으로
  // 목록에서 사라진 경우 **다른 쿼리의 계획**을 보여주게 되고, 그건 조사 도구에서
  // 가장 위험한 거짓말이다.
  const picked = selected === "" ? undefined : items.find((i) => i.record_id === selected);
  const missing = selected !== "" && picked === undefined && !plans.isPending;
  const current = picked ?? (selected === "" ? items[0] : undefined);
  const nowMs = Date.now();

  return (
    <div className="space-y-6">
      <h1 className={PAGE_TITLE}>Query Plan Visualization</h1>

      <Card
        title={
          <>
            <Share2 className="h-5 w-5 text-gray-500" /> Recent Explain Plans
          </>
        }
        actions={
          <label className={LABEL}>
            <Filter className="h-4 w-4 text-gray-500" />
            <select
              className={SELECT}
              value={instance}
              onChange={(e) => update("instance", e.target.value)}
              aria-label="인스턴스 선택"
            >
              <option value="">All Instances</option>
              {(instances.data ?? []).map((i) => (
                <option key={i.id} value={i.id}>
                  {i.name}
                </option>
              ))}
            </select>
          </label>
        }
        note={
          <>
            실행계획이 <strong>저장된 레코드만</strong> 나온다. 계획은 수집 시점에 마스킹돼
            저장되므로(FR-PLN-09) 여기에는 리터럴이 없다.
          </>
        }
      >
        {plans.error !== null ? (
          <ErrorNotice error={plans.error} onRetry={() => void plans.refetch()} />
        ) : plans.isPending ? (
          <Pending label="플랜 목록을 불러오는 중…" />
        ) : (
          <div className="overflow-x-auto">
            <table className={TABLE}>
              <thead>
                <tr>
                  {/* **짧은 열은 내용 폭만.** 남는 폭은 Query 가 받는다 — 표에서
                      정보량이 가장 큰 열이 가장 넓어야 한다. */}
                  <th className={`${TH} ${COL_TIGHT}`}>Created At</th>
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>Thread</th>
                  <th className={`${TH} ${COL_TIGHT_CAPPED}`}>Instance</th>
                  <th className={`${TH} ${COL_TIGHT_CAPPED}`}>Schema</th>
                  <th className={`${TH_NUM} ${COL_TIGHT}`}>Time</th>
                  <th className={`${TH} ${COL_GROW}`}>Query</th>
                </tr>
              </thead>
              <tbody className={TBODY}>
                {items.length === 0 ? (
                  <EmptyRow colSpan={6}>
                    저장된 실행계획이 없다. 계획은 슬로우 쿼리를 잡은 뒤 수집되므로, 임계값을
                    넘는 쿼리가 한 번은 돌아야 한다.
                  </EmptyRow>
                ) : (
                  items.map((q) => (
                    <tr
                      key={q.record_id}
                      className={`${TR} cursor-pointer ${
                        current?.record_id === q.record_id ? "bg-blue-50/60" : ""
                      }`}
                      onClick={() => update("record", q.record_id)}
                    >
                      <td className={`${TD} ${COL_TIGHT}`}>
                        <span className="flex items-center">
                          <Calendar className={CELL_ICON} />
                          {fmtListTime(q.started_at_ms, tz, nowMs)}
                        </span>
                      </td>
                      <td className={`${TD_NUM} ${COL_TIGHT}`}>
                        <span className="flex items-center justify-end">
                          <Hash className={CELL_ICON} />
                          {q.thread_id}
                        </span>
                      </td>
                      <td className={`${TD} ${COL_TIGHT_CAPPED}`} title={q.instance_id}>
                        <span className="flex items-center gap-1">
                          <Database className={CELL_ICON} />
                          {shortInstance(q.instance_id)}
                          <EnvChip env={q.env} />
                        </span>
                      </td>
                      <td className={`${TD} ${COL_TIGHT}`}>{q.schema_name ?? EMPTY}</td>
                      <td className={`${TD_NUM} ${COL_TIGHT}`}>
                        <span className="flex items-center justify-end">
                          <Clock className={CELL_ICON} />
                          {(q.duration_ms / 1000).toFixed(1)}s
                        </span>
                      </td>
                      {/* 남는 폭을 전부 받는다. `max-w-0` + `truncate` 조합이 있어야
                          긴 SQL 이 표를 밀어내지 않고 잘린다. */}
                      <td className={`${COL_GROW} max-w-0 px-3 py-2 text-sm text-gray-700`}>
                        <div className="truncate font-mono text-xs">
                          {q.sql_text ?? "(SQL 미저장)"}
                        </div>
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      {missing ? (
        <Card>
          <p className="text-sm text-gray-700">
            주소로 지정한 레코드가 이 목록에 없다 (
            <span className={MONO}>{selected}</span>). 인스턴스 필터나 조회 구간을 확인한다 —
            <strong> 다른 쿼리의 계획을 대신 보여주지 않는다.</strong>
          </p>
          <button type="button" className={`${BTN_GHOST} mt-3`} onClick={() => update("record", "")}>
            최근 것부터 보기
          </button>
        </Card>
      ) : null}
      {current === undefined ? null : <PlanDetail query={current} tz={tz} />}
      {plans.data?.has_more === true ? (
        <Note>
          플랜이 조회 상한을 넘었다 — 목록은 최근 것만 보여준다. 인스턴스로 좁히면 더
          거슬러 볼 수 있다.
        </Note>
      ) : null}
    </div>
  );
}

function PlanDetail({ query, tz }: { query: SlowQueryView; tz: Timezone }) {
  const [downloading, setDownloading] = useState(false);
  const plan = useQuery({
    queryKey: queryKeys.plan(query.record_id),
    queryFn: ({ signal }) => fetchPlan(query.record_id, signal),
  });

  async function download() {
    setDownloading(true);
    try {
      const md = await fetchMarkdown(query.record_id);
      downloadText(`slow-query-${query.thread_id}.md`, md);
    } catch {
      // 실패는 아래 오류 표시로 드러난다. 다운로드는 재시도가 값싸다.
    } finally {
      setDownloading(false);
    }
  }

  return (
    <>
      <Card
        title={
          <>
            <FileJson className="h-5 w-5 text-gray-500" /> Query Information
          </>
        }
        actions={
          <button type="button" className={BTN_GHOST} onClick={() => void download()} disabled={downloading}>
            <Download className="h-4 w-4" />
            {downloading ? "만드는 중…" : "Download Markdown"}
          </button>
        }
      >
        <dl className="grid grid-cols-2 gap-x-6 gap-y-3 md:grid-cols-4">
          <Item label="Instance">
            <span title={query.instance_id}>{shortInstance(query.instance_id)}</span>
          </Item>
          <Item label="Env">
            <EnvChip env={query.env} />
          </Item>
          <Item label="State">
            <StateBadge state={query.state} reason={query.abandoned_reason} />
          </Item>
          <Item label="Thread">{query.thread_id}</Item>
          <Item label="Execution Time">{(query.duration_ms / 1000).toFixed(2)}s</Item>
          <Item label="Started">{fmtListTime(query.started_at_ms, tz, Date.now())}</Item>
          <Item label="Schema">{query.schema_name ?? EMPTY}</Item>
          <Item label="DB User">{query.db_user ?? EMPTY}</Item>
          <Item label="Rows examined / sent">
            {fmtInt(query.rows_examined)} / {fmtInt(query.rows_sent)}
          </Item>
          <Item label="Capture">{query.capture_source}</Item>
          <Item label="Duration source">{query.duration_source}</Item>
          <Item label="Digest">
            <span className={MONO}>{query.app_digest}</span>
          </Item>
          <Item label="Plan source">{plan.data?.source ?? EMPTY}</Item>
        </dl>

        <h3 className="mt-5 mb-2 text-sm font-medium text-gray-700">SQL Query</h3>
        {query.sql_text === null ? (
          <p className="text-sm text-gray-600">
            {query.sql_redacted_reason === "insufficient_role"
              ? "원문이 저장돼 있지만 열람 권한이 없다."
              : "SQL 이 저장되지 않았다."}
          </p>
        ) : (
          <pre className="overflow-x-auto rounded-md bg-gray-50 p-3 font-mono text-xs whitespace-pre-wrap text-gray-800">
            {formatSql(query.sql_text)}
          </pre>
        )}
      </Card>

      <Card
        title={
          <>
            <FileJson className="h-5 w-5 text-gray-500" /> Query Execution Plan
          </>
        }
      >
        {plan.error !== null ? (
          <ErrorNotice error={plan.error} onRetry={() => void plan.refetch()} />
        ) : plan.isPending ? (
          <Pending label="계획을 불러오는 중…" />
        ) : (
          <>
            {plan.data.error === null ? null : (
              <p className="mb-3 rounded-md bg-amber-50 p-3 text-sm text-amber-800">
                계획 수집이 실패했다: <span className="font-mono">{plan.data.error}</span>
              </p>
            )}
            {plan.data.normalized_json === null ? (
              <p className="text-sm text-gray-600">
                {plan.data.s3_key === null
                  ? "저장된 계획이 없다."
                  : `300KB 를 넘어 S3 로 오프로드됐다 (${plan.data.s3_key}). 본문 조회 경로는 아직 없다.`}
              </p>
            ) : (
              <PlanGraph json={plan.data.normalized_json} />
            )}

            {plan.data.referenced_tables.length === 0 ? null : (
              <Note>
                참조 테이블: <span className={MONO}>{plan.data.referenced_tables.join(", ")}</span>
              </Note>
            )}

            {plan.data.tree_text === null ? null : (
              <>
                <h3 className="mt-5 mb-2 text-sm font-medium text-gray-700">EXPLAIN ANALYZE (tree)</h3>
                <pre className="overflow-x-auto rounded-md bg-gray-50 p-3 font-mono text-xs whitespace-pre text-gray-800">
                  {plan.data.tree_text}
                </pre>
              </>
            )}

            {plan.data.normalized_json === null ? null : (
              <details className="mt-4">
                <summary className="cursor-pointer text-sm text-gray-700">
                  계획 JSON 원문 ({plan.data.format_version ?? "형식 미기록"})
                </summary>
                <pre className="mt-2 max-h-96 overflow-auto rounded-md bg-gray-50 p-3 font-mono text-xs whitespace-pre-wrap text-gray-800">
                  {prettyJson(plan.data.normalized_json)}
                </pre>
              </details>
            )}
          </>
        )}
      </Card>
    </>
  );
}

/** 저장된 JSON 은 한 줄이다. 들여쓰기에 실패하면 원문을 그대로 보여준다. */
function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

function Item({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <dt className="text-xs font-medium tracking-wide text-gray-500 uppercase">{label}</dt>
      <dd className="mt-0.5 text-sm text-gray-800">{children}</dd>
    </div>
  );
}
