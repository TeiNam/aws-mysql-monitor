import { useQuery } from "@tanstack/react-query";
import { Clock, CloudCog, Database, Search, User } from "lucide-react";
import { useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { useInstances } from "../hooks/useInstances";
import { PageHeader } from "../components/PageHeader";
import { Card } from "../components/Card";
import { BackfillButton } from "../components/CollectorControls";
import { EnvFilter, InstanceFilter, InstanceSearch } from "../components/Filters";
import { EmptyRow, ErrorNotice, Note, Pending, TruncatedNote } from "../components/Notices";
import { EnvChip } from "../components/Shell";
import { SqlModal } from "../components/SqlModal";
import {
  CELL_ICON,
  CELL_X,
  COL_GROW,
  COL_TIGHT,
  COL_TIGHT_CAPPED,
  LABEL,
  MONO,
  SELECT,
  TABLE,
  TBODY,
  TD,
  TD_NUM,
  TH,
  TH_NUM,
  TR,
} from "../components/ui";
import { fetchDigests, queryKeys } from "../lib/api";
import {
  EMPTY,
  fmtDateTime,
  fmtInt,
  fmtSeconds,
  monthKey,
  monthRangeLabel,
  recentMonths,
} from "../lib/format";
import type { DigestRow } from "../lib/types";

/**
 * 다이제스트 화면. 참조 대시보드의 `CloudWatch Slow Query Monitor` 자리다.
 *
 * # 참조 구현과 다른 점
 *
 * 참조는 "전월 데이터 수집" 버튼으로 CloudWatch 로그를 한 번에 긁어 MongoDB 에
 * 월간 다이제스트를 만들어 뒀다. 이 백엔드는 **슬로우로그 백필이 상시 자동**이고
 * (수집기가 주기적으로 CloudWatch Logs 를 읽어 레코드를 병합한다), 다이제스트는
 * 조회 시점에 접는다. 그래서 수집 버튼이 없다 — 대신 그 사실을 카드에 적는다.
 *
 * 수동 트리거·진행률 표시가 필요하면 백엔드에 작업 큐를 먼저 만들어야 한다.
 */
export function CloudWatchPage() {
  const [params, setParams] = useSearchParams();
  const month = params.get("month") ?? monthKey(new Date());
  const instance = params.get("instance") ?? "";
  const env = params.get("env") ?? "";
  const instanceLike = params.get("instance_like") ?? "";
  const [openSql, setOpenSql] = useState<DigestRow | null>(null);

  const months = useMemo(() => recentMonths(new Date(), 12), []);
  // **필터는 조회 키에 들어간다.** 안 넣으면 필터를 바꿔도 캐시된 앞 결과가 그려진다.
  const digestParams = useMemo(
    () => ({
      month,
      ...(instance === "" ? {} : { instance }),
      ...(env === "" ? {} : { env }),
      ...(instanceLike === "" ? {} : { instance_like: instanceLike }),
    }),
    [month, instance, env, instanceLike],
  );

  const digests = useQuery({
    queryKey: queryKeys.digests(digestParams),
    queryFn: ({ signal }) => fetchDigests(digestParams, signal),
  });
  // **머리말의 리전 범위가 적용된 목록**이다(`useInstances`). 화면마다 직접
  // 조회하면 리전 필터를 한 곳만 빠뜨려도 그 화면에서 범위 밖이 보인다.
  const instances = useInstances();
  const envOf = useMemo(() => {
    const map = new Map<string, string>();
    for (const i of instances.data ?? []) map.set(i.id, i.env);
    return map;
  }, [instances.data]);

  /** 여러 값을 **한 번에** 바꾼다 — env 를 바꾸며 인스턴스를 지울 때 두 번 쓰면
   *  중간 상태(새 env + 옛 인스턴스)로 한 번 조회가 나간다. */
  function updateMany(patch: Record<string, string>) {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(patch)) {
      if (value === "") next.delete(key);
      else next.set(key, value);
    }
    setParams(next, { replace: true });
  }

  function update(key: string, value: string) {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  }

  return (
    <div className="space-y-6">
      <PageHeader title="CloudWatch Slow Query Monitor" />

      <Card
        title={
          <>
            <CloudCog className="h-5 w-5 text-gray-500" /> 수집 방식
          </>
        }
        actions={<BackfillButton />}
      >
        <p className="text-sm text-gray-700">
          슬로우로그 수집은 <strong>상시 자동</strong>이다. 수집기가 CloudWatch Logs 를 주기적으로
          읽어 실시간 캡처 레코드와 병합한다 — 참조 구현처럼 "전월 데이터 수집" 을 사람이 눌러야
          하지 않는다. 아래 표는 그렇게 쌓인 실행 레코드를 <strong>조회 시점에</strong> 다이제스트로
          접은 것이다.
        </p>
        <Note>
          "지금 수집" 은 <strong>다음 주기를 기다리지 않고 한 번 돌린다.</strong> 참조 구현처럼
          임의의 지난달을 다시 훑으려면 별도 작업(잡 큐)이 필요해서 아직 없다 — 백필은
          체크포인트부터 현재까지를 읽는 구조다.
        </Note>
      </Card>

      <Card
        title={
          <>
            <Search className="h-5 w-5 text-gray-500" /> Slow Query Digest (
            {monthRangeLabel(month, digests.data?.from_ms, digests.data?.to_ms)})
          </>
        }
        actions={
          <>
            <label className={LABEL}>
              <Clock className="h-4 w-4 text-gray-500" />
              <select
                className={SELECT}
                value={month}
                onChange={(e) => update("month", e.target.value)}
                aria-label="조회 월"
              >
                {months.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </select>
            </label>
            <EnvFilter
              instances={instances.data ?? []}
              env={env}
              instance={instance}
              instanceLike={instanceLike}
              onChange={(patch) =>
                updateMany({
                  ...(patch.env === undefined ? {} : { env: patch.env }),
                  ...(patch.instance === undefined ? {} : { instance: patch.instance }),
                })
              }
            />
            <InstanceFilter
              instances={instances.data ?? []}
              env={env}
              instance={instance}
              instanceLike={instanceLike}
              onChange={(patch) => update("instance", patch.instance ?? "")}
            />
            <InstanceSearch
              instances={instances.data ?? []}
              env={env}
              value={instanceLike}
              onChange={(like) => update("instance_like", like)}
            />
          </>
        }
        note={<>월 경계는 <strong>KST</strong> 기준이다. UTC 로 자르면 매월 초 9시간이 이전 달로 간다.</>}
      >
        {digests.error !== null ? (
          <ErrorNotice error={digests.error} onRetry={() => void digests.refetch()} />
        ) : digests.isPending ? (
          <Pending label="다이제스트를 접는 중…" />
        ) : (
          <>
            <div className="overflow-x-auto">
              <table className={TABLE}>
                <thead>
                  <tr>
                    {/* 짧은 열은 내용 폭만, 남는 폭은 SQL 이 받는다. */}
                    <th className={`${TH} ${COL_TIGHT_CAPPED}`}>Instance</th>
                    <th className={`${TH} ${COL_TIGHT_CAPPED}`}>User</th>
                    <th className={`${TH} ${COL_TIGHT}`}>Type</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Exec Count</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Total Time (s)</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Avg Time (s)</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Max Time (s)</th>
                    <th className={`${TH_NUM} ${COL_TIGHT}`}>Avg Rows Examined</th>
                    <th className={`${TH} ${COL_GROW}`}>SQL Digest</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {digests.data.items.length === 0 ? (
                    <EmptyRow colSpan={9}>
                      {/* **"이 달에 없다" 라고 말하지 않는다.** 서버가 구간을 좁혀 읽었으면
                          달의 일부만 본 것이고, 그걸 "없다" 로 읽으면 조사가 끝나 버린다. */}
                      {/* 상한(`truncated`)에 걸렸을 때도 "없다" 로 말하면 안 된다 —
                          구간은 그대로여도 **일부만 읽은 것**이다(7라운드 지적). */}
                      {monthRangeLabel(month, digests.data.from_ms, digests.data.to_ms) === month &&
                      !digests.data.truncated
                        ? "이 달에 기록된 슬로우 쿼리가 없다. 월을 바꿔 보거나, 수집이 도는지 MySQL Monitor 화면에서 확인한다."
                        : `조회한 범위(${monthRangeLabel(month, digests.data.from_ms, digests.data.to_ms)}${digests.data.truncated ? ", 조회 상한에 걸림" : ""})에서는 기록을 찾지 못했다 — 이 달 전체를 본 것이 아니다. 인스턴스를 지정하면 더 좁혀 볼 수 있다.`}
                    </EmptyRow>
                  ) : (
                    digests.data.items.map((row) => (
                      <tr
                        key={`${row.instance_id}:${row.app_digest}`}
                        className={`${TR} cursor-pointer`}
                        onClick={() => setOpenSql(row)}
                      >
                        <td className={`${TD} ${COL_TIGHT_CAPPED}`} title={row.instance_id}>
                          <span className="flex items-center gap-1">
                            <Database className={CELL_ICON} />
                            {row.instance_id.split("/").pop()}
                            <EnvChip env={envOf.get(row.instance_id) ?? "unknown"} />
                          </span>
                        </td>
                        <td className={TD}>
                          <span className="flex items-center">
                            <User className={CELL_ICON} />
                            {row.users.length === 0 ? EMPTY : row.users.join(", ")}
                          </span>
                        </td>
                        <td className={`${TD} ${COL_TIGHT}`}>{row.statement_type}</td>
                        <td className={`${TD_NUM} ${COL_TIGHT}`}>{fmtInt(row.exec_count)}</td>
                        <td className={`${TD_NUM} ${COL_TIGHT}`}>{fmtSeconds(row.total_time_ms)}</td>
                        <td className={`${TD_NUM} ${COL_TIGHT}`}>{fmtSeconds(row.avg_time_ms)}</td>
                        <td className={`${TD_NUM} ${COL_TIGHT}`}>{fmtSeconds(row.max_time_ms)}</td>
                        {/* 행 정보가 없는 실행만 있으면 `null` 이다 — 0 이 아니다. */}
                        <td className={TD_NUM}>
                          {row.avg_rows_examined === null
                            ? EMPTY
                            : fmtInt(Math.round(row.avg_rows_examined))}
                        </td>
                        <td className={`${COL_GROW} ${CELL_X} max-w-0 py-2 text-sm text-gray-700`}>
                          <div className={`truncate ${MONO}`}>
                            {/* **"미저장" 으로 뭉개지 않는다.** 권한이 없어 가려진 것과
                                애초에 저장하지 않은 것은 운영자가 할 일이 다르다. */}
                            {row.digest_query ??
                              (row.digest_query_redacted_reason === "insufficient_role"
                                ? "(권한 없음 — SQL 은 저장돼 있다)"
                                : "(SQL 미저장 — 리터럴 정책)")}
                          </div>
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>

            {digests.data.truncated ? <TruncatedNote scanned={digests.data.scanned} /> : null}
            <Note>
              {digests.data.items.length.toLocaleString("ko-KR")}개 다이제스트 ·{" "}
              {digests.data.scanned.toLocaleString("ko-KR")}건 실행을 접었다 (
              {fmtDateTime(digests.data.from_ms, "KST")} ~ {fmtDateTime(digests.data.to_ms, "KST")}{" "}
              KST). 행을 누르면 그 다이제스트의 <strong>대표</strong> SQL 을 본다 — 리터럴은 환경의 저장 정책에 따라 남아 있을 수도, 마스킹돼 있을 수도 있다.
            </Note>
          </>
        )}
      </Card>

      {openSql === null ? null : (
        <SqlModal
          title={`SQL Digest — ${openSql.app_digest}`}
          sql={openSql.digest_query}
          onClose={() => setOpenSql(null)}
        />
      )}
    </div>
  );
}
