import { useQuery } from "@tanstack/react-query";
import { Check, Clock, Database, RefreshCw, Server, X } from "lucide-react";
import { Card } from "../components/Card";
import { CollectorControls } from "../components/CollectorControls";
import { EmptyRow, ErrorNotice, Note, Pending } from "../components/Notices";
import { EnvChip } from "../components/Shell";
import {
  BTN_GHOST,
  CELL_ICON,
  MONO,
  PAGE_TITLE,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { fetchInstances, queryKeys } from "../lib/api";
import { EMPTY, fmtDateTime } from "../lib/format";

/**
 * 인스턴스 관리 화면. 참조 대시보드의 `RDS Instance Management` 와 같은 표다.
 *
 * # 참조 구현과 다른 점
 *
 * 참조는 "인스턴스 수집" 버튼으로 RDS `DescribeDBInstances` 를 즉시 호출했다.
 * 이 백엔드는 **탐색이 주기적으로 돈다**(기본 5분) — 태그로 수집 대상을 가리고,
 * 사라진 인스턴스는 2회 연속 미발견에서만 삭제 처리한다(1회 API 실패로 지우지 않기
 * 위해). 그래서 버튼 대신 마지막 관측 시각을 보여준다.
 */
export function RDSInstancePage() {
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });

  return (
    <div className="space-y-6">
      <h1 className={PAGE_TITLE}>RDS Instance Management</h1>

      <Card
        title={
          <>
            <Server className="h-5 w-5 text-gray-500" /> RDS Instances
          </>
        }
        actions={
          <>
            <CollectorControls />
            <button
              type="button"
              className={BTN_GHOST}
              onClick={() => void instances.refetch()}
              disabled={instances.isFetching}
            >
              <RefreshCw className={`h-4 w-4 ${instances.isFetching ? "animate-spin" : ""}`} />
              표 새로 고침
            </button>
          </>
        }
        note={
          <>
            탐색은 <strong>주기적으로 자동</strong>이다(기본 5분). "인스턴스 수집" 을 누르면
            다음 주기를 기다리지 않고 지금 한 번 훑는다. 수집 대상은 태그와 필터로 정해진다 —
            <span className={MONO}> REAL-TIME</span> 열이 그 결과다.
          </>
        }
      >
        {instances.error !== null ? (
          <ErrorNotice error={instances.error} onRetry={() => void instances.refetch()} />
        ) : instances.isPending ? (
          <Pending label="인스턴스를 불러오는 중…" />
        ) : (
          <>
            <div className="overflow-x-auto">
              <table className={TABLE}>
                <thead>
                  <tr>
                    <th className={TH}>Instance ID</th>
                    <th className={TH}>Real-time</th>
                    <th className={TH}>Env</th>
                    <th className={TH}>State</th>
                    <th className={TH}>Engine</th>
                    <th className={TH}>Version</th>
                    <th className={TH}>Endpoint</th>
                    <th className={TH}>Class</th>
                    <th className={TH}>Tags</th>
                    <th className={TH_NUM}>Last seen</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {instances.data.length === 0 ? (
                    <EmptyRow colSpan={10}>
                      등록된 인스턴스가 없다. 탐색이 아직 돌지 않았거나, 필터(VPC·이름 규칙)가
                      전부 제외했다.
                    </EmptyRow>
                  ) : (
                    instances.data.map((i) => (
                      <tr key={i.id} className={TR}>
                        <td className={TD} title={i.id}>
                          <span className="flex items-center">
                            <Database className={CELL_ICON} />
                            {i.name}
                            {i.deleted_at_ms === null ? null : (
                              <span className="ml-2 rounded bg-gray-100 px-1.5 py-0.5 text-xs text-gray-700">
                                삭제됨
                              </span>
                            )}
                          </span>
                        </td>
                        <td className={TD}>
                          {/* 색만으로 구분하지 않는다 — 아이콘과 글자를 함께 둔다. */}
                          {i.collectible ? (
                            <span className="inline-flex items-center gap-1 text-green-700">
                              <Check className="h-4 w-4" /> 수집
                            </span>
                          ) : (
                            <span className="inline-flex items-center gap-1 text-gray-500">
                              <X className="h-4 w-4" /> 제외
                            </span>
                          )}
                        </td>
                        <td className={TD}>
                          <span className="flex items-center gap-1">
                            <EnvChip env={i.env} />
                            {i.env_override === null ? null : (
                              <span
                                className="text-xs text-gray-500"
                                title={`태그는 ${i.env_from_tags} 인데 ${i.env_override} 로 지정됨`}
                              >
                                (지정)
                              </span>
                            )}
                          </span>
                        </td>
                        <td className={TD}>{i.state}</td>
                        <td className={TD}>{i.engine}</td>
                        <td className={`${TD} ${MONO}`}>{i.engine_version}</td>
                        <td className={`${TD} ${MONO} max-w-[280px] truncate`} title={i.endpoint ?? ""}>
                          {i.endpoint === null ? EMPTY : `${i.endpoint}:${i.port}`}
                        </td>
                        <td className={TD}>{i.instance_class ?? EMPTY}</td>
                        <td className="max-w-[320px] px-3 py-2 text-sm">
                          <Tags tags={i.tags} />
                        </td>
                        <td className={`${TD_NUM} text-gray-600`}>
                          <span className="flex items-center justify-end">
                            <Clock className={CELL_ICON} />
                            {fmtDateTime(i.last_seen_ms, "KST")}
                          </span>
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>
            <Note>
              환경은 태그에서 유도한다. <span className="font-mono">(지정)</span> 이 붙은 행은
              사용자가 태그와 다르게 정한 것이고, 마우스를 올리면 원래 태그 값을 보여준다.
            </Note>
          </>
        )}
      </Card>
    </div>
  );
}

/** 태그. **`env` 를 먼저 보여준다** — 수집 대상 분류의 근거다. */
function Tags({ tags }: { tags: Record<string, string> }) {
  const entries = Object.entries(tags);
  if (entries.length === 0) return <span className="text-gray-500">{EMPTY}</span>;
  entries.sort(([a], [b]) => (a === "env" ? -1 : b === "env" ? 1 : a.localeCompare(b)));
  return (
    <span className="flex flex-wrap gap-1">
      {entries.map(([k, v]) => (
        <span
          key={k}
          className={`rounded px-1.5 py-0.5 text-xs ring-1 ${
            k === "env"
              ? "bg-blue-50 text-blue-700 ring-blue-200"
              : "bg-gray-100 text-gray-700 ring-gray-300"
          }`}
        >
          {k}={v}
        </span>
      ))}
    </span>
  );
}
