import { useQuery } from "@tanstack/react-query";
import { Clock, CloudCog, Database, Filter, Search, User } from "lucide-react";
import { useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { Card } from "../components/Card";
import { BackfillButton } from "../components/CollectorControls";
import { EmptyRow, ErrorNotice, Note, Pending, TruncatedNote } from "../components/Notices";
import { EnvChip } from "../components/Shell";
import { SqlModal } from "../components/SqlModal";
import {
  CELL_ICON,
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
import { fetchDigests, fetchInstances, queryKeys } from "../lib/api";
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
  const [openSql, setOpenSql] = useState<DigestRow | null>(null);

  const months = useMemo(() => recentMonths(new Date(), 12), []);
  const digestParams = useMemo(
    () => ({ month, ...(instance === "" ? {} : { instance }) }),
    [month, instance],
  );

  const digests = useQuery({
    queryKey: queryKeys.digests(digestParams),
    queryFn: ({ signal }) => fetchDigests(digestParams, signal),
  });
  const instances = useQuery({
    queryKey: queryKeys.instances,
    queryFn: ({ signal }) => fetchInstances(signal),
  });
  const envOf = useMemo(() => {
    const map = new Map<string, string>();
    for (const i of instances.data ?? []) map.set(i.id, i.env);
    return map;
  }, [instances.data]);

  function update(key: string, value: string) {
    const next = new URLSearchParams(params);
    if (value === "") next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  }

  return (
    <div className="space-y-6">
      <h1 className={PAGE_TITLE}>CloudWatch Slow Query Monitor</h1>

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
                    <th className={TH}>Instance</th>
                    <th className={TH}>User</th>
                    <th className={TH}>Type</th>
                    <th className={TH_NUM}>Exec Count</th>
                    <th className={TH_NUM}>Total Time (s)</th>
                    <th className={TH_NUM}>Avg Time (s)</th>
                    <th className={TH_NUM}>Max Time (s)</th>
                    <th className={TH_NUM}>Avg Rows Examined</th>
                    <th className={TH}>SQL Digest</th>
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
                        <td className={TD} title={row.instance_id}>
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
                        <td className={TD}>{row.statement_type}</td>
                        <td className={TD_NUM}>{fmtInt(row.exec_count)}</td>
                        <td className={TD_NUM}>{fmtSeconds(row.total_time_ms)}</td>
                        <td className={TD_NUM}>{fmtSeconds(row.avg_time_ms)}</td>
                        <td className={TD_NUM}>{fmtSeconds(row.max_time_ms)}</td>
                        {/* 행 정보가 없는 실행만 있으면 `null` 이다 — 0 이 아니다. */}
                        <td className={TD_NUM}>
                          {row.avg_rows_examined === null
                            ? EMPTY
                            : fmtInt(Math.round(row.avg_rows_examined))}
                        </td>
                        <td className="max-w-[460px] px-3 py-2 text-sm text-gray-700">
                          <div className={`truncate ${MONO}`}>
                            {row.digest_query ?? "(SQL 미저장)"}
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
              KST). 행을 누르면 전체 SQL 을 본다.
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
