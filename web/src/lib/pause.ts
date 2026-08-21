/**
 * 정지 스코프 — 키 만들기와 판정.
 *
 * # 왜 여기 모으는가
 *
 * 스코프 키(`*` · `env:prd` · `id:<account>/<region>/<identifier>`)는 서버
 * (`crates/core/src/pause.rs`)와 **같은 문자열**이어야 한다. 화면에서 손으로
 * `` `env:${e}` `` 를 조립하면 한쪽만 바뀔 때 정지가 조용히 아무 인스턴스에도
 * 걸리지 않는다 — 에러가 아니라 "눌렀는데 계속 수집된다" 로 나타난다.
 *
 * # 판정을 서버와 두 번 적는 이유
 *
 * 서버가 진실이지만, 화면은 **누를 수 있는 것만 제시해야** 한다. 못 누를 옵션을
 * 보여주고 403 을 받게 하는 것은 항상 실패하는 선택지를 제시하는 것이다
 * (`Filters.tsx` 규칙 1과 같은 이유).
 */

import type { CollectorStatus, Env, InstanceView, PausedScope } from "./types";

/** 전체 정지 키. */
export const ALL_SCOPE = "*";

export const envScope = (env: string): string => `env:${env}`;
export const instanceScope = (instanceId: string): string => `id:${instanceId}`;

/**
 * 이 인스턴스의 수집이 멈춰 있는가. 서버 `PauseSet::is_paused` 와 같은 판정이다 —
 * 전체 · 그 인스턴스의 **적용 환경** · 그 인스턴스 자신 중 하나라도 멈췄으면 멈춘 것이다.
 */
export function isInstancePaused(
  paused: readonly PausedScope[],
  instance: InstanceView,
): boolean {
  const keys = new Set(paused.map((p) => p.scope));
  return (
    keys.has(ALL_SCOPE) ||
    keys.has(envScope(instance.env)) ||
    keys.has(instanceScope(instance.id))
  );
}

/** 스코프 키에서 인스턴스 id 를 뽑는다. 인스턴스 스코프가 아니면 `null`. */
export function instanceIdOf(scope: string): string | null {
  return scope.startsWith("id:") ? scope.slice(3) : null;
}

/** 스코프 키에서 환경을 뽑는다. 환경 스코프가 아니면 `null`. */
export function envOf(scope: string): string | null {
  return scope.startsWith("env:") ? scope.slice(4) : null;
}

/**
 * 사람이 읽는 이름. **인스턴스는 등록부의 이름으로 바꾼다** — 화면에
 * `id:123456789012/ap-northeast-2/orders-dev-01` 을 그대로 띄우면 칩이 줄을 넘긴다.
 * 등록부에 없으면(삭제됐거나 스코프 밖) 식별자만 남긴다.
 */
export function scopeLabel(scope: string, instances: readonly InstanceView[]): string {
  if (scope === ALL_SCOPE) return "전체";
  const env = envOf(scope);
  if (env !== null) return env;
  const id = instanceIdOf(scope);
  if (id === null) return scope;
  const found = instances.find((i) => i.id === id);
  return found?.name ?? id.split("/").at(-1) ?? id;
}

/**
 * 서버가 **스코프 정지를 아는가.**
 *
 * # 왜 확인해야 하는가 (실측)
 *
 * 스코프를 모르는 구 버전 서버는 `POST /api/collector/pause` 의 본문을 **그냥 무시하고
 * 전체를 멈춘다.** 그러면 "이 인스턴스만 정지" 를 누른 사람이 전 환경 관측을 멈추게
 * 된다 — 200 이 오고 화면도 "정지" 로 보이므로 알아챌 단서가 없다.
 *
 * 롤링 배포 중에는 새 화면이 구 워커에 붙는 구간이 반드시 생긴다. 그래서 판정을
 * 서버가 스스로 밝힌 사실에 걸어 둔다: 정지 상태를 저장소에 두는 서버만
 * `scope: "deployment"` 라고 말한다(구 버전은 `"process"` 였다).
 */
export function supportsScopedPause(status: CollectorStatus | undefined): boolean {
  return status?.scope === "deployment";
}

/**
 * 이 스코프를 멈추거나 재개할 권한이 있는가. 서버 `require_scope_control` 과 같은 규칙:
 * 전체는 전 환경 스코프, 환경·인스턴스는 그 환경 권한이다.
 *
 * 등록부에 없는 인스턴스는 **판단할 수 없으므로 false** 다 — 서버는 404 를 준다.
 * 스코프를 모르는 서버에서는 **전부 false** 다 (위 설명).
 */
export function canControlScope(
  scope: string,
  status: CollectorStatus | undefined,
  instances: readonly InstanceView[],
): boolean {
  if (status === undefined || !supportsScopedPause(status)) return false;
  if (scope === ALL_SCOPE) return status.can_control;
  const allowed = (env: Env) => status.controllable_envs.includes(env);
  const env = envOf(scope);
  if (env !== null) return allowed(env as Env);
  const id = instanceIdOf(scope);
  if (id === null) return false;
  const found = instances.find((i) => i.id === id);
  return found !== undefined && allowed(found.env);
}

/** 셀렉터에 넣을 선택지 하나. */
export interface ScopeOption {
  key: string;
  label: string;
  /** `<optgroup>` 이름. 전체는 그룹이 없다. */
  group: "all" | "env";
  /** 권한이 없어 누를 수 없다. **숨기지 않고 비활성화한다** — 왜 없는지 알려야 한다. */
  disabled: boolean;
}

/**
 * 전체 → 환경 순서의 선택지. `envs` 는 **보이는 환경**이다(`visibleEnvs`).
 *
 * # 왜 인스턴스가 없는가
 *
 * 개별 정지는 **표의 행 버튼**이 한다. 등록부가 500대면 셀렉터가 500줄이 되고,
 * 그 안에서 하나를 찾는 것보다 표에서 그 행을 보는 것이 빠르다 — 게다가 표는 이미
 * 환경·상태·마지막 관측을 함께 보여주므로 "이걸 멈춰도 되나" 를 판단할 근거가 그 자리에 있다.
 */
export function scopeOptions(
  envs: readonly string[],
  status: CollectorStatus | undefined,
): ScopeOption[] {
  const option = (key: string, label: string, group: ScopeOption["group"]): ScopeOption => ({
    key,
    label,
    group,
    // 환경·전체 스코프는 등록부를 보지 않는다 — 권한이 `controllable_envs` 로 정해진다.
    disabled: !canControlScope(key, status, []),
  });
  return [
    option(ALL_SCOPE, "전체", "all"),
    ...envs.map((e) => option(envScope(e), e, "env")),
  ];
}
