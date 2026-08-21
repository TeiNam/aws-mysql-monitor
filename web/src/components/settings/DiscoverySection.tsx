import { Globe, Plus, Trash2 } from "lucide-react";
import { Card } from "../Card";
import { Note } from "../Notices";
import { BTN_GHOST } from "../ui";
import { regionLabel } from "../../lib/regions";
import type { AccountTarget, DiscoverySettings, SettingsProblem } from "../../lib/types";
import { RegionList, TextField, Toggle, problemOf } from "./fields";

/** 계정마다 만드는 역할의 기본 이름. 서버 기본값과 같아야 한다. */
const DEFAULT_ROLE = "dbmon-discovery";

/**
 * 탐색 범위 — 리전과 계정.
 *
 * # 왜 "전체 리전" 이 없는가
 *
 * 리전마다 `DescribeDBInstances` 를 부르므로 30개 리전이면 매 탐색이 30배가 되고,
 * 대부분은 RDS 가 하나도 없는 리전이다. 목록으로 좁히는 것이 기본이고, 비워 두면
 * **이 워커가 사는 리전 하나**만 본다.
 *
 * # 계정 목록은 네트워크와 무관하다
 *
 * 크로스 계정 조회는 `sts:AssumeRole` + RDS API 다 — VPC 피어링·TGW 는 필요 없다.
 * 피어링이 필요한 것은 그 다음, **DB 에 붙어 수집할 때**다. 그래서 "목록에는 뜨는데
 * 수집이 `unreachable`" 인 상태가 정상적으로 존재한다.
 */
export function DiscoverySection({
  value,
  onChange,
  problems,
  disabled,
  ownRegion,
}: {
  value: DiscoverySettings;
  onChange: (next: DiscoverySettings) => void;
  problems: SettingsProblem[];
  disabled: boolean;
  ownRegion: string;
}) {
  const setAccount = (index: number, next: AccountTarget) =>
    onChange({
      ...value,
      // **새 배열을 만든다.** 제자리 수정은 리렌더를 놓치고, 되돌리기(취소)도 깨진다.
      accounts: value.accounts.map((a, i) => (i === index ? next : a)),
    });

  return (
    <Card
      title={
        <>
          <Globe className="h-5 w-5 text-gray-500" /> 탐색 범위
        </>
      }
    >
      <div className="space-y-4">
        <RegionList
          label="리전"
          hint={
            <>
              비워 두면 이 워커의 리전(<span className="font-mono">{regionLabel(ownRegion)}</span>)
              하나만 본다. 줄바꿈이나 쉼표로 여러 개를 넣는다.
            </>
          }
          value={value.regions}
          onChange={(regions) => onChange({ ...value, regions })}
          error={problemOf(problems, "discovery.regions")}
          placeholder={"ap-northeast-2\nus-east-1"}
          disabled={disabled}
        />
        {/* 배열 항목별 오류(`discovery.regions[2]`)도 보여준다 — 어느 줄이 틀렸는지
            모르면 다섯 줄 중 하나를 찾아 헤맨다. */}
        <ItemProblems problems={problems} prefix="discovery.regions" />

        <div className="border-t border-gray-200 pt-4">
          <Toggle
            label="다른 계정도 탐색한다"
            hint="끄면 아래 목록을 지우지 않고 무시한다 — 다시 켤 때 재입력하지 않아도 된다."
            checked={value.multi_account_enabled}
            onChange={(v) => onChange({ ...value, multi_account_enabled: v })}
            disabled={disabled}
          />
        </div>

        {value.accounts.map((account, i) => (
          <div
            // 계정 번호는 편집 중 비어 있을 수 있어 키로 쓸 수 없다(중복 키가 된다).
            key={`account-${i}`}
            className="rounded-lg border border-gray-200 bg-gray-50 p-3"
          >
            <div className="mb-3 flex items-center justify-between gap-2">
              <span className="text-sm font-medium text-gray-800">
                계정 {i + 1}
                {account.label === "" ? "" : ` · ${account.label}`}
              </span>
              <button
                type="button"
                className={BTN_GHOST}
                disabled={disabled}
                onClick={() =>
                  onChange({
                    ...value,
                    accounts: value.accounts.filter((_, j) => j !== i),
                  })
                }
              >
                <Trash2 className="h-4 w-4" /> 제거
              </button>
            </div>
            <div className="grid gap-3 sm:grid-cols-2">
              <TextField
                label="계정 번호"
                value={account.account_id}
                onChange={(account_id) => setAccount(i, { ...account, account_id })}
                error={problemOf(problems, `discovery.accounts[${i}].account_id`)}
                placeholder="111122223333"
                disabled={disabled}
                mono
              />
              <TextField
                label="별칭"
                hint="계정 번호만으로는 어느 팀인지 모른다"
                value={account.label}
                onChange={(label) => setAccount(i, { ...account, label })}
                placeholder="prod-payments"
                disabled={disabled}
              />
              <TextField
                label="맡을 역할 이름"
                hint="ARN 이 아니라 이름만. 계정 번호와 합쳐 서버가 ARN 을 만든다."
                value={account.role_name}
                onChange={(role_name) => setAccount(i, { ...account, role_name })}
                error={problemOf(problems, `discovery.accounts[${i}].role_name`)}
                placeholder={DEFAULT_ROLE}
                disabled={disabled}
                mono
              />
              <div className="sm:col-span-2">
                <RegionList
                  label="이 계정의 리전"
                  hint="비우면 위의 리전 목록을 쓴다"
                  value={account.regions}
                  onChange={(regions) => setAccount(i, { ...account, regions })}
                  disabled={disabled}
                />
                <ItemProblems problems={problems} prefix={`discovery.accounts[${i}].regions`} />
              </div>
            </div>
            <div className="mt-3">
              <Toggle
                label="이 계정을 탐색한다"
                checked={account.enabled}
                onChange={(enabled) => setAccount(i, { ...account, enabled })}
                disabled={disabled}
              />
            </div>
          </div>
        ))}

        <div>
          <button
            type="button"
            className={BTN_GHOST}
            disabled={disabled}
            onClick={() =>
              onChange({
                ...value,
                accounts: [
                  ...value.accounts,
                  {
                    account_id: "",
                    role_name: DEFAULT_ROLE,
                    regions: [],
                    enabled: true,
                    label: "",
                  },
                ],
              })
            }
          >
            <Plus className="h-4 w-4" /> 계정 추가
          </button>
          {/* 목록 전체에 걸린 오류(중복 계정 등). */}
          <ItemProblems problems={problems} prefix="discovery.accounts" onlyExact />
        </div>

        <Note>
          대상 계정에 역할(<span className="font-mono">{DEFAULT_ROLE}</span>)이 있어야 한다 —
          신뢰 정책은 이 배포의 태스크 롤만, 권한은{" "}
          <span className="font-mono">rds:Describe*</span>·
          <span className="font-mono">rds:ListTagsForResource</span>·
          <span className="font-mono">cloudwatch:GetMetricData</span> 다.
        </Note>
      </div>
    </Card>
  );
}

/** 배열 항목의 오류 목록. `onlyExact` 면 접두어와 정확히 같은 것만. */
function ItemProblems({
  problems,
  prefix,
  onlyExact,
}: {
  problems: SettingsProblem[];
  prefix: string;
  onlyExact?: boolean;
}) {
  const list = problems.filter((p) =>
    onlyExact === true ? p.field === prefix : p.field.startsWith(`${prefix}[`),
  );
  if (list.length === 0) return null;
  return (
    <ul className="mt-1 space-y-0.5">
      {list.map((p) => (
        <li key={`${p.field}:${p.message}`} className="text-xs font-medium text-red-700">
          <span className="font-mono">{p.field}</span> — {p.message}
        </li>
      ))}
    </ul>
  );
}
