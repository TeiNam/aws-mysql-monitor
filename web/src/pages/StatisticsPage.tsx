import { useQuery } from "@tanstack/react-query";
import { BarChart3, Clock, Database, Filter, User } from "lucide-react";
import { useMemo } from "react";
import { useSearchParams } from "react-router";
import { Card } from "../components/Card";
import { EmptyRow, ErrorNotice, Note, Pending, TruncatedNote } from "../components/Notices";
import { EnvChip } from "../components/Shell";
import {
  CELL_ICON,
  LABEL,
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
import {
  fetchInstanceStatistics,
  fetchInstances,
  fetchUserStatistics,
  queryKeys,
} from "../lib/api";
import {
  EMPTY,
  fmtDateTime,
  fmtInt,
  fmtSeconds,
  monthKey,
  monthRangeLabel,
  recentMonths,
} from "../lib/format";

/** 참조 대시보드의 `Statistics` 화면 — 월간 SQL 통계 + 사용자별 통계. */
export function StatisticsPage() {
  const [params, setParams] = useSearchParams();
  const month = params.get("month") ?? monthKey(new Date());
  const instance = params.get("instance") ?? "";

  const months = useMemo(() => recentMonths(new Date(), 12), []);
  const statParams = useMemo(
    () => ({ month, ...(instance === "" ? {} : { instance }) }),
    [month, instance],
  );

  const stats = useQuery({
    queryKey: queryKeys.statistics(statParams),
    queryFn: ({ signal }) => fetchInstanceStatistics(statParams, signal),
  });
  const users = useQuery({
    queryKey: queryKeys.userStatistics(statParams),
    queryFn: ({ signal }) => fetchUserStatistics(statParams, signal),
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

  const filters = (
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
  );

  return (
    <div className="space-y-6">
      <h1 className={PAGE_TITLE}>SQL Statistics</h1>

      <Card
        title={
          <>
            <BarChart3 className="h-5 w-5 text-gray-500" /> 인스턴스별 (
            {monthRangeLabel(month, stats.data?.from_ms, stats.data?.to_ms)})
          </>
        }
        actions={filters}
        note={
          <>
            월 경계는 <strong>KST</strong> 기준이다. 참조 구현은 CloudWatch 다이제스트를 따로
            쌓았지만 여기서는 <strong>레코드 하나가 실행 하나</strong>라서 슬로우 쿼리 수와 실행
            수가 같다.
          </>
        }
      >
        {stats.error !== null ? (
          <ErrorNotice error={stats.error} onRetry={() => void stats.refetch()} />
        ) : stats.isPending ? (
          <Pending label="통계를 계산하는 중…" />
        ) : (
          <>
            <div className="overflow-x-auto">
              <table className={TABLE}>
                <thead>
                  <tr>
                    <th className={TH}>Instance</th>
                    <th className={TH_NUM}>Unique Digests</th>
                    <th className={TH_NUM}>Slow Queries</th>
                    <th className={TH_NUM}>Total Time (s)</th>
                    <th className={TH_NUM}>Avg Time (s)</th>
                    <th className={TH_NUM}>Max Time (s)</th>
                    <th className={TH_NUM}>Rows Examined</th>
                    <th className={TH}>유형 분포</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {stats.data.items.length === 0 ? (
                    <EmptyRow colSpan={8}>
                      {/* 좁혀진 구간을 "이 달" 로 말하지 않는다(6라운드 지적). */}
                      {monthRangeLabel(month, stats.data.from_ms, stats.data.to_ms) === month
                        ? "이 달에 기록된 슬로우 쿼리가 없다."
                        : `조회한 구간(${monthRangeLabel(month, stats.data.from_ms, stats.data.to_ms)})에는 기록이 없다 — 이 달 전체를 본 것이 아니다.`}
                    </EmptyRow>
                  ) : (
                    stats.data.items.map((s) => (
                      <tr key={s.instance_id} className={TR}>
                        <td className={TD} title={s.instance_id}>
                          <span className="flex items-center gap-1">
                            <Database className={CELL_ICON} />
                            {s.instance_id.split("/").pop()}
                            <EnvChip env={envOf.get(s.instance_id) ?? "unknown"} />
                          </span>
                        </td>
                        <td className={TD_NUM}>{fmtInt(s.unique_digest_count)}</td>
                        <td className={TD_NUM}>{fmtInt(s.total_slow_query_count)}</td>
                        <td className={TD_NUM}>{fmtSeconds(s.total_execution_time_ms)}</td>
                        <td className={TD_NUM}>{fmtSeconds(s.avg_execution_time_ms)}</td>
                        <td className={TD_NUM}>{fmtSeconds(s.max_execution_time_ms)}</td>
                        <td className={TD_NUM}>{fmtInt(s.total_rows_examined)}</td>
                        <td className={TD}>
                          <KindMix
                            read={s.read_query_count}
                            write={s.write_query_count}
                            ddl={s.ddl_query_count}
                            commit={s.commit_query_count}
                            other={s.other_query_count}
                          />
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>
            {stats.data.truncated ? <TruncatedNote scanned={stats.data.scanned} /> : null}
            <Note>
              실제로 읽은 구간: {fmtDateTime(stats.data.from_ms, "KST")} ~{" "}
              {fmtDateTime(stats.data.to_ms, "KST")} KST ·{" "}
              {stats.data.scanned.toLocaleString("ko-KR")}건 실행을 접었다.
            </Note>
          </>
        )}
      </Card>

      <Card
        title={
          <>
            <User className="h-5 w-5 text-gray-500" /> 사용자별 (
            {monthRangeLabel(month, users.data?.from_ms, users.data?.to_ms)})
          </>
        }
        note={
          <>
            DB 사용자를 <strong>알 수 없는 실행은 빠진다</strong> — `unknown` 같은 가짜 행을 만들면
            실제 계정처럼 보인다.
          </>
        }
      >
        {users.error !== null ? (
          <ErrorNotice error={users.error} onRetry={() => void users.refetch()} />
        ) : users.isPending ? (
          <Pending label="사용자별 통계를 계산하는 중…" />
        ) : (
          <>
            <div className="overflow-x-auto">
              <table className={TABLE}>
                <thead>
                  <tr>
                    <th className={TH}>Instance</th>
                    <th className={TH}>User</th>
                    <th className={TH_NUM}>Queries</th>
                    <th className={TH_NUM}>Unique Digests</th>
                    <th className={TH_NUM}>Total Time (s)</th>
                    <th className={TH_NUM}>Avg Time (s)</th>
                    <th className={TH_NUM}>Max Time (s)</th>
                    <th className={TH}>유형 분포</th>
                  </tr>
                </thead>
                <tbody className={TBODY}>
                  {users.data.items.length === 0 ? (
                    <EmptyRow colSpan={8}>
                      사용자 정보가 있는 실행이 없다. 실시간 캡처는 계정을 함께 기록하지만,
                      슬로우로그만으로 만든 레코드는 계정이 비어 있을 수 있다.
                      {monthRangeLabel(month, users.data.from_ms, users.data.to_ms) === month
                        ? ""
                        : ` (조회한 구간: ${monthRangeLabel(month, users.data.from_ms, users.data.to_ms)} — 이 달 전체가 아니다)`}
                    </EmptyRow>
                  ) : (
                    users.data.items.map((u) => (
                      <tr key={`${u.instance_id}:${u.user}`} className={TR}>
                        <td className={TD} title={u.instance_id}>
                          {u.instance_id.split("/").pop()}
                        </td>
                        <td className={TD}>
                          <span className="flex items-center">
                            <User className={CELL_ICON} />
                            {u.user}
                          </span>
                        </td>
                        <td className={TD_NUM}>{fmtInt(u.total_queries)}</td>
                        <td className={TD_NUM}>{fmtInt(u.unique_digest_count)}</td>
                        <td className={TD_NUM}>{fmtSeconds(u.total_exec_time_ms)}</td>
                        <td className={TD_NUM}>{fmtSeconds(u.avg_execution_time_ms)}</td>
                        <td className={TD_NUM}>{fmtSeconds(u.max_execution_time_ms)}</td>
                        <td className={TD}>
                          <KindMix
                            read={u.read_query_count}
                            write={u.write_query_count}
                            ddl={u.ddl_query_count}
                            commit={u.commit_query_count}
                            other={u.other_query_count}
                          />
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>
            {users.data.truncated ? <TruncatedNote scanned={users.data.scanned} /> : null}
            <Note>
              실제로 읽은 구간: {fmtDateTime(users.data.from_ms, "KST")} ~{" "}
              {fmtDateTime(users.data.to_ms, "KST")} KST ·{" "}
              {users.data.scanned.toLocaleString("ko-KR")}건 실행을 접었다.
            </Note>
            <Note>
              <span className="font-mono">{EMPTY}</span> 는 0 이 아니라 값이 없다는 뜻이다.
              `commit` 은 SQL 을 볼 수 있을 때만 구분한다 — 가려진 문장을 추측하지 않는다.
            </Note>
          </>
        )}
      </Card>
    </div>
  );
}

/** 읽기/쓰기/DDL/Commit 분포. 참조 대시보드도 한 칸에 넷을 적었다. */
function KindMix({
  read,
  write,
  ddl,
  commit,
  other,
}: {
  read: number;
  write: number;
  ddl: number;
  commit: number;
  other: number;
}) {
  const chips: Array<[string, number, string]> = [
    ["읽기", read, "bg-blue-50 text-blue-700 ring-blue-200"],
    ["쓰기", write, "bg-emerald-50 text-emerald-700 ring-emerald-200"],
    ["DDL", ddl, "bg-purple-50 text-purple-700 ring-purple-200"],
    ["Commit", commit, "bg-amber-50 text-amber-800 ring-amber-200"],
    ["기타", other, "bg-gray-100 text-gray-700 ring-gray-300"],
  ];
  return (
    <span className="flex flex-wrap gap-1">
      {chips
        .filter(([, n]) => n > 0)
        .map(([label, n, tone]) => (
          <span key={label} className={`rounded px-1.5 py-0.5 text-xs ring-1 ${tone}`}>
            {label} {n.toLocaleString("ko-KR")}
          </span>
        ))}
    </span>
  );
}
