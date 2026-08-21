import { useQuery } from "@tanstack/react-query";
import { Check, Clock, Database, Pause, Play, RefreshCw, Server, X } from "lucide-react";
import { PageHeader } from "../components/PageHeader";
import { Card } from "../components/Card";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  CollectorControls,
  useCollectorStatus,
  usePauseControls,
} from "../components/CollectorControls";
import { EmptyRow, ErrorNotice, Note, Pending } from "../components/Notices";
import { EnvChip } from "../components/Shell";
import {
  CELL_X,
  BTN,
  BTN_GHOST,
  CELL_ICON,
  MONO,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { fetchInstances, queryKeys, startInstance } from "../lib/api";
import { EMPTY, fmtDateTime } from "../lib/format";
import {
  canControlScope,
  envScope,
  instanceScope,
  isInstancePaused,
  supportsScopedPause,
} from "../lib/pause";
import type { CollectorStatus, InstanceView } from "../lib/types";

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
  // 표의 "수집" 열과 행 버튼이 정지를 반영해야 한다 — 같은 `queryKey` 라 요청은 늘지 않는다.
  const status = useCollectorStatus();
  const pausedScopes = status.data?.paused_scopes ?? [];

  return (
    <div className="space-y-6">
      <PageHeader title="RDS Instance Management" />

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
                    <th className={TH_NUM}>수집 제어</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {instances.data.length === 0 ? (
                    <EmptyRow colSpan={11}>
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
                          {/* 색만으로 구분하지 않는다 — 아이콘과 글자를 함께 둔다.
                              **정지를 따로 말한다.** 멈춰 있는데 "수집" 이라고 적으면
                              표가 거짓말을 하고, "제외" 로 접으면 필터에서 빠진 것과
                              사람이 멈춘 것을 구분할 수 없다(대응이 다르다). */}
                          {i.state === "pending" ? (
                            // **"제외" 와 다르다.** 등록만 됐고 사람이 아직 시작하지 않았다.
                            <span className="inline-flex items-center gap-1 text-blue-700">
                              <Clock className="h-4 w-4" /> 대기
                            </span>
                          ) : !i.collectible ? (
                            <span className="inline-flex items-center gap-1 text-gray-500">
                              <X className="h-4 w-4" /> 제외
                            </span>
                          ) : isInstancePaused(pausedScopes, i) ? (
                            <span className="inline-flex items-center gap-1 text-amber-700">
                              <Pause className="h-4 w-4" /> 정지
                            </span>
                          ) : (
                            <span className="inline-flex items-center gap-1 text-green-700">
                              <Check className="h-4 w-4" /> 수집
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
                        <td className={`${CELL_X} max-w-[320px] py-2 text-sm`}>
                          <Tags tags={i.tags} />
                        </td>
                        <td className={`${TD_NUM} text-gray-600`}>
                          <span className="flex items-center justify-end">
                            <Clock className={CELL_ICON} />
                            {fmtDateTime(i.last_seen_ms, "KST")}
                          </span>
                        </td>
                        <td className={TD_NUM}>
                          <RowPauseButton instance={i} status={status.data} />
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

/**
 * 행 하나의 수집 켜기/끄기 버튼.
 *
 * # 왜 버튼이 두 개가 아니라 하나인가
 *
 * 인스턴스가 수집되지 않는 이유는 내부적으로 둘이다 — **한 번도 시작하지 않았다**
 * (`state=pending`)와 **사람이 멈췄다**(정지 스코프). 하지만 운영자에게 필요한 개념은
 * "이 인스턴스를 수집하나 / 안 하나" 하나뿐이고, 그 차이는 옆의 상태 열이
 * (`대기` / `정지`) 이미 말해 준다. 버튼을 셋으로 나누면(`시작`·`정지`·`재개`) 같은
 * 뜻의 라벨이 둘 생긴다.
 *
 * 그래서 `수집 시작` 을 누르면 필요한 것을 **둘 다** 한다: `pending` 이면 승격하고,
 * 자기 스코프가 멈춰 있으면 함께 재개한다.
 *
 * # 상위 스코프는 이 버튼으로 풀 수 없다
 *
 * `전체` 나 `env:*` 정지에 걸려 있으면 이 인스턴스만 켤 수 없다 — 그 정지는 다른
 * 인스턴스에도 걸려 있으므로 여기서 풀면 의도를 넘어선다. 그 사실을 호박색 표식과
 * `title` 로 말하고, 버튼은 자기 스코프에만 작용한다.
 */
function RowPauseButton({
  instance,
  status,
}: {
  instance: InstanceView;
  status: CollectorStatus | undefined;
}) {
  const { pause, resume, busy } = usePauseControls();
  const queryClient = useQueryClient();
  // 시작은 **대상 DB 에 매초 쿼리를 날리기 시작하는 일**이다. 그래서 등록은 자동이지만
  // 시작은 사람이 누른다(`should_collect` 가 pending 을 뺀다).
  const start = useMutation({
    mutationFn: startInstance,
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: queryKeys.instances }),
  });

  const scope = instanceScope(instance.id);
  const scopes = status?.paused_scopes ?? [];
  const ownPaused = scopes.some((p) => p.scope === scope);
  const neverStarted = instance.state === "pending";
  const byAll = scopes.some((p) => p.scope === "*");
  const byEnv = scopes.some((p) => p.scope === envScope(instance.env));
  const broader = byAll ? "전체" : byEnv ? `환경(${instance.env})` : null;
  // 정지·재개와 **같은 권한 규칙**을 쓴다 — 그 인스턴스의 환경 권한이 필요하다.
  const can = canControlScope(scope, status, [instance]);
  const working = busy || start.isPending;

  // 태그·버전·필터가 근거인 상태는 화면 버튼으로 덮지 않는다 — 눌려도 다음 탐색이
  // 되돌리므로 고장으로 읽힌다. 서버도 409 로 거부한다.
  const notStartable = ["disabled", "unsupported", "excluded", "deleted"].includes(instance.state);

  const off = neverStarted || ownPaused;

  const title = !can
    ? status === undefined
      ? "상태를 아직 읽지 못했다"
      : !supportsScopedPause(status)
        ? `이 서버는 스코프 정지를 모른다 (scope=${status.scope})`
        : `이 환경(${instance.env}) 의 조작 권한이 없다 (역할: ${status.role})`
    : notStartable
      ? `상태가 ${instance.state} 다 — 태그·버전·필터를 고쳐야 한다`
      : off
        ? broader === null
          ? neverStarted
            ? "이 인스턴스에 접속을 시작한다 (매초 탐지 쿼리가 나간다)"
            : "이 인스턴스만 재개한다"
          : `${broader} 정지가 함께 걸려 있다 — 이것만 켜도 그 정지가 남아 있으면 수집되지 않는다`
        : broader === null
          ? "이 인스턴스만 정지한다"
          : `이미 ${broader} 정지에 걸려 있다 — 이 버튼은 이 인스턴스만 따로 고정한다`;

  async function toggle() {
    if (off) {
      if (neverStarted) await start.mutateAsync(instance.id);
      if (ownPaused) await resume.mutateAsync(scope);
      return;
    }
    await pause.mutateAsync(scope);
  }

  return (
    <span className="inline-flex items-center justify-end gap-1">
      {broader === null ? null : (
        // 상위 스코프 때문에 멈춰 있다는 표시. 버튼 라벨과 실제 상태가 어긋나 보이는
        // 것을 막는 유일한 단서다.
        <span className="text-xs text-amber-700" title={`${broader} 정지 적용 중`}>
          {broader}
        </span>
      )}
      <button
        type="button"
        className={`${BTN} ${
          off
            ? "bg-green-500 text-oncolor hover:bg-green-600"
            : "bg-gray-100 text-gray-700 hover:bg-gray-200"
        } px-2 py-1 text-xs`}
        disabled={working || !can || notStartable}
        title={title}
        onClick={() => void toggle()}
      >
        {off ? (
          <>
            <Play className="h-3.5 w-3.5" /> 수집 시작
          </>
        ) : (
          <>
            <Pause className="h-3.5 w-3.5" /> 정지
          </>
        )}
      </button>
    </span>
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
