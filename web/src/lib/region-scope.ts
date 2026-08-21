/**
 * **지금 보고 있는 리전.** 머리말의 선택기가 정하고, 인스턴스를 다루는 화면 전부가 따른다.
 *
 * # 왜 URL 이 아닌가
 *
 * 리전은 "이 세션이 어느 범위를 본다" 는 **환경**이고, 화면마다 들고 다니는 필터가
 * 아니다. URL 에 두면 일곱 화면이 각자 파라미터를 읽고 링크마다 붙여야 하며, 하나를
 * 빠뜨리면 탭을 옮기는 순간 범위가 조용히 넓어진다.
 *
 * 대가: 링크를 공유하면 상대는 자기 범위로 본다. 계정·리전은 머리말에 항상 보이므로
 * 그 상태를 오해하기 어렵다.
 *
 * # 저장하는 값은 코드 하나뿐이다
 *
 * `ALL_REGIONS` 는 "전부" 다. 저장된 리전이 더 이상 탐색 범위에 없으면(설정에서
 * 지웠다) **자동으로 전부로 되돌린다** — 없는 리전으로 필터하면 목록이 영구히 비고,
 * 그건 "인스턴스가 사라졌다" 로 읽힌다.
 */

import { useCallback, useSyncExternalStore } from "react";

const KEY = "dbmon.region";
/** "전부" 를 뜻하는 값. 저장하지 않는다(키를 지운다). */
export const ALL_REGIONS = "*";

const listeners = new Set<() => void>();

function read(): string {
  try {
    return window.localStorage.getItem(KEY) ?? ALL_REGIONS;
  } catch {
    // 사생활 보호 모드. 이번 세션은 전부 본다.
    return ALL_REGIONS;
  }
}

function emit() {
  for (const fn of listeners) fn();
}

/** 범위를 바꾼다. 화면 전체가 즉시 따라온다. */
export function setRegionScope(region: string): void {
  try {
    if (region === ALL_REGIONS) window.localStorage.removeItem(KEY);
    else window.localStorage.setItem(KEY, region);
  } catch {
    // 저장은 못 해도 이번 세션은 바뀌어야 한다 — 아래 `emit()` 이 그 일을 한다.
  }
  emit();
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  // **다른 탭에서 바꾼 것도 반영한다.** 두 탭을 열어 두고 한쪽에서 리전을 바꿨는데
  // 다른 쪽이 옛 범위를 보여주면, 그 화면을 보고 조작하다 엉뚱한 리전을 만진다.
  const onStorage = (e: StorageEvent) => {
    if (e.key === KEY || e.key === null) fn();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(fn);
    window.removeEventListener("storage", onStorage);
  };
}

/** 지금 범위. `ALL_REGIONS` 면 전부다. */
export function useRegionScope(): string {
  return useSyncExternalStore(subscribe, read, () => ALL_REGIONS);
}

/**
 * 범위 + 바꾸는 함수. **알려진 리전 목록을 함께 받아 검증한다** — 목록에 없는 값이
 * 저장돼 있으면 전부로 접는다.
 */
export function useRegionPicker(known: readonly string[]): {
  region: string;
  setRegion: (r: string) => void;
} {
  const stored = useRegionScope();
  const region = stored !== ALL_REGIONS && !known.includes(stored) ? ALL_REGIONS : stored;
  const setRegion = useCallback((r: string) => setRegionScope(r), []);
  return { region, setRegion };
}

/**
 * 인스턴스 id(`계정/리전/이름`)에서 리전을 뽑는다.
 *
 * **문자열 위치로 자른다.** 이름에 `/` 가 들어갈 수 없으므로(RDS 식별자 규칙)
 * 두 번째 조각이 리전이다.
 */
export function regionOfInstanceId(id: string): string | null {
  const parts = id.split("/");
  return parts.length >= 3 && parts[1] !== "" ? (parts[1] as string) : null;
}

/** 이 범위에 드는가. 범위가 "전부" 면 항상 참이다. */
export function inRegionScope(id: string, scope: string): boolean {
  if (scope === ALL_REGIONS) return true;
  return regionOfInstanceId(id) === scope;
}
