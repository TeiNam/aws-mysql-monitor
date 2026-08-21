/**
 * 조회 필터 — 환경(env)과 인스턴스.
 *
 * # 왜 컴포넌트로 뽑았는가
 *
 * 목록·플랜·다이제스트·통계 네 화면이 같은 필터를 쓴다. 화면마다 `<select>` 를 복사하면
 * **한 화면만 규칙이 어긋난다** — 실제로 인스턴스 선택은 네 곳에 복사돼 있었고, env 를
 * 추가하면서 그 수가 여덟 곳이 될 상황이었다.
 *
 * # 규칙 세 개 (여기 없으면 조용히 빈 표가 나온다)
 *
 * 1. **옵션은 보이는 인스턴스에서 만든다.** env 목록을 코드에 박으면 dev 스코프인
 *    사용자에게 `prd` 를 제시하고, 그걸 고르면 서버가 `env_not_allowed`(403)로 거부한다 —
 *    **항상 실패하는 선택지를 보여주는 셈**이다. 등록부 응답은 이미 스코프로 걸러져 있다.
 * 2. **인스턴스 목록은 고른 env 로 좁힌다.** 안 좁히면 `env=dev` 를 고른 뒤 prd
 *    인스턴스를 고를 수 있고, 결과는 0건인데 이유가 화면에 없다.
 * 3. **env 를 바꿀 때 그 env 에 없는 인스턴스 선택은 지운다.** 남겨 두면 같은 이유로
 *    빈 표가 된다.
 */

import { Filter, Layers, Search, X } from "lucide-react";
import { useEffect, useState } from "react";
import { LABEL, SELECT } from "./ui";
import type { InstanceView } from "../lib/types";

/** 표시 순서. 운영이 먼저 보여야 한다. */
const ENV_ORDER = ["prd", "stg", "dev"] as const;

/** 이 계정에 보이는 환경들. 등록부 응답에서 만든다(규칙 1). */
export function visibleEnvs(instances: readonly InstanceView[]): string[] {
  const found = new Set(instances.map((i) => i.env));
  const ordered = ENV_ORDER.filter((e) => found.has(e)) as string[];
  // 규정에 없는 값이 오면 뒤에 붙인다 — 숨기면 그 인스턴스를 고를 방법이 없다.
  const extra = [...found].filter((e) => !ENV_ORDER.includes(e as (typeof ENV_ORDER)[number]));
  return [...ordered, ...extra.sort()];
}

/** 이름 조각에 걸리는 인스턴스 (대소문자 무시). **서버와 같은 규칙**(`name_matches`). */
export function instancesMatching(
  instances: readonly InstanceView[],
  like: string,
): readonly InstanceView[] {
  const pattern = like.trim().toLowerCase();
  if (pattern === "") return instances;
  return instances.filter((i) => i.name.toLowerCase().includes(pattern));
}

/** 고른 env 에 속한 인스턴스만 (규칙 2). `env` 가 비면 전부. */
export function instancesInEnv(
  instances: readonly InstanceView[],
  env: string,
): readonly InstanceView[] {
  return env === "" ? instances : instances.filter((i) => i.env === env);
}

/**
 * **두 필터를 함께** 적용한 인스턴스 집합 (규칙 4).
 *
 * 따로 쓰면 화면이 어긋난다 — 실측으로 두 번 드러났다:
 *
 * - 인스턴스 드롭다운이 env 만 좁혀서, `orders` 를 치고도 `billing-prd-01` 이 남았다.
 * - "몇 대 걸렸나" 가 env 를 무시해서, `env=prd` + `orders` 에 **3대**라고 적었다
 *   (`orders-stg-01` 까지 세어서 — 그 인스턴스는 이 조회에 들어오지 않는다).
 *
 * 서버도 같은 순서로 적용한다(`self_instances`: 정확 id → 이름 조각, 그리고 레코드마다
 * env 교집합).
 */
export function visibleInstances(
  instances: readonly InstanceView[],
  env: string,
  like: string,
): readonly InstanceView[] {
  return instancesMatching(instancesInEnv(instances, env), like);
}

interface FilterProps {
  instances: readonly InstanceView[];
  env: string;
  instance: string;
  /** 이름 조각. 인스턴스 목록·개수 표시가 **이것까지 반영해야** 화면이 어긋나지 않는다. */
  instanceLike: string;
  /** `(키, 값)` 쌍을 URL 에 반영한다. 두 값을 한 번에 바꿔야 할 때는 두 번 부른다. */
  onChange: (patch: { env?: string; instance?: string }) => void;
}

export function EnvFilter({ instances, env, instance, instanceLike, onChange }: FilterProps) {
  const envs = visibleEnvs(instances);
  // **등록부가 비었을 때만 숨긴다.** 환경이 하나뿐이어도 위젯은 남긴다 — 없으면
  // "환경 필터가 없는 화면" 으로 읽히고, 인스턴스가 늘어난 뒤에야 나타나는 컨트롤은
  // 찾는 사람을 헤매게 한다.
  if (envs.length === 0) return null;

  return (
    <label className={LABEL}>
      <Layers className="h-4 w-4 text-gray-500" />
      <select
        className={SELECT}
        value={env}
        onChange={(e) => {
          const next = e.target.value;
          // 규칙 3 — 고른 인스턴스가 새 조건에 없으면 함께 지운다(이름 조각까지 본다).
          const keep = visibleInstances(instances, next, instanceLike).some(
            (i) => i.id === instance,
          );
          onChange({ env: next, instance: keep ? instance : "" });
        }}
        aria-label="환경 선택"
      >
        <option value="">All Envs</option>
        {envs.map((e) => (
          <option key={e} value={e}>
            {e}
          </option>
        ))}
      </select>
    </label>
  );
}

export function InstanceFilter({
  instances,
  env,
  instance,
  instanceLike,
  onChange,
}: FilterProps) {
  // **두 필터를 함께 반영한다.** env 만 좁히면 `orders` 를 치고도 `billing-*` 이 남는다.
  const options = visibleInstances(instances, env, instanceLike);

  return (
    <label className={LABEL}>
      <Filter className="h-4 w-4 text-gray-500" />
      <select
        className={SELECT}
        value={instance}
        onChange={(e) => onChange({ instance: e.target.value })}
        aria-label="인스턴스 선택"
      >
        <option value="">All Instances</option>
        {options.map((i) => (
          <option key={i.id} value={i.id}>
            {/* 환경이 여럿이면 이름만으로는 어느 환경인지 알 수 없다. */}
            {env === "" && visibleEnvs(instances).length > 1 ? `${i.name} (${i.env})` : i.name}
          </option>
        ))}
      </select>
    </label>
  );
}

/**
 * 이름 조각으로 **여러 인스턴스를 묶어** 보는 입력.
 *
 * # 왜 서버 필터인가
 *
 * 클라이언트에서 걸러내면 서버는 등록부 전체에 읽기 상한을 뿌린다 — `orders-*` 5대를
 * 보려는데 상한이 500대에 나뉘어 정작 그 5대의 최근 데이터가 빠진다. 그래서 조각을
 * `instance_like` 로 보내고, 서버가 **그 집합에만** 몫을 나눈다.
 *
 * # 왜 디바운스하는가
 *
 * 한 글자마다 조회하면 `orders` 를 치는 동안 6번 요청이 나가고, 그중 5개는 버려질
 * 결과다. 타이핑이 멈춘 뒤에만 URL 을 바꾼다(URL 이 바뀌면 조회 키가 바뀐다).
 *
 * # 몇 대에 걸렸는지 말한다
 *
 * 조각이 아무 인스턴스에도 안 걸리면 결과는 0건인데 화면에는 이유가 없다 — "0대에
 * 걸렸다" 를 그 자리에서 보여줘야 오타를 알아챈다.
 */
export function InstanceSearch({
  instances,
  env,
  value,
  onChange,
}: {
  instances: readonly InstanceView[];
  /** 고른 환경. **개수 표시가 이걸 반영해야** 조회에 들어오지 않는 인스턴스를 세지 않는다. */
  env: string;
  value: string;
  onChange: (like: string) => void;
}) {
  const [text, setText] = useState(value);

  // 바깥(URL)에서 값이 바뀌면 따라간다 — 뒤로 가기·링크 공유가 동작해야 한다.
  useEffect(() => setText(value), [value]);

  // 타이핑이 멈춘 뒤에만 URL 에 반영한다.
  useEffect(() => {
    if (text === value) return;
    const id = window.setTimeout(() => onChange(text), DEBOUNCE_MS);
    return () => window.clearTimeout(id);
  }, [text, value, onChange]);

  const matched = visibleInstances(instances, env, text);
  const active = text.trim() !== "";

  return (
    <label className={`${LABEL} relative`}>
      <Search className="h-4 w-4 text-gray-500" />
      <input
        type="search"
        className={`${SELECT} w-44 pr-6`}
        placeholder="이름 조각 (orders)"
        value={text}
        onChange={(e) => setText(e.target.value)}
        aria-label="인스턴스 이름으로 묶어 보기"
      />
      {active ? (
        <>
          <button
            type="button"
            className="absolute right-1.5 text-gray-400 hover:text-gray-700"
            onClick={() => {
              setText("");
              onChange("");
            }}
            aria-label="이름 필터 지우기"
          >
            <X className="h-3.5 w-3.5" />
          </button>
          <span
            className={`ml-1 text-xs ${matched.length === 0 ? "text-amber-700" : "text-gray-500"}`}
          >
            {matched.length}대
          </span>
        </>
      ) : null}
    </label>
  );
}

/** 타이핑이 멈춘 뒤 이만큼 기다린다. 사람이 한 낱말을 치는 간격보다 길다. */
const DEBOUNCE_MS = 350;
