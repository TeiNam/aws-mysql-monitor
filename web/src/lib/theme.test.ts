import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { applyTheme, readChoice, resolveTheme, startTheme, writeChoice } from "./theme";

/**
 * 테마 저장·해석만 테스트한다.
 *
 * 여기가 조용히 틀리면 **화면이 첫 프레임에 깜빡이거나**, 고른 테마가 새로고침에
 * 사라진다. 둘 다 "고장" 으로 읽히는데 에러는 안 난다.
 */

/**
 * `localStorage` 를 흉내낸다.
 *
 * 이 환경에서는 Node 의 실험적 `localStorage`(플래그 없이는 비활성)가 jsdom 것을
 * 가려서 `window.localStorage` 가 `undefined` 다. 실제 브라우저에는 있고, 없을 때는
 * `theme.ts` 가 try/catch 로 기본값(`system`)으로 간다 — 그 경로도 아래에서 본다.
 */
function stubStorage(): void {
  const map = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
    clear: () => map.clear(),
  });
}

/** `matchMedia` 는 jsdom 에 없다. OS 설정을 주입할 수 있게 흉내낸다. */
function stubMatchMedia(dark: boolean) {
  const listeners = new Set<() => void>();
  const media = {
    matches: dark,
    addEventListener: (_: string, fn: () => void) => listeners.add(fn),
    removeEventListener: (_: string, fn: () => void) => listeners.delete(fn),
  };
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => media),
  );
  return { media, fire: () => listeners.forEach((fn) => fn()) };
}

beforeEach(() => {
  stubStorage();
  document.documentElement.classList.remove("dark");
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("저장된 선택", () => {
  it("고른 적 없으면 시스템 설정이다", () => {
    expect(readChoice()).toBe("system");
  });

  /** 다른 버전이 남긴 값이나 손으로 넣은 값에 화면이 걸려 넘어지지 않아야 한다. */
  it("모르는 값은 시스템 설정으로 접는다", () => {
    localStorage.setItem("dbmon.theme", "midnight");
    expect(readChoice()).toBe("system");
  });

  /** 사생활 보호 모드는 접근 자체가 던진다. **화면이 죽지 않아야 한다.** */
  it("저장소를 쓸 수 없으면 시스템 설정으로 간다", () => {
    vi.stubGlobal("localStorage", undefined);
    expect(readChoice()).toBe("system");
    expect(() => writeChoice("dark")).not.toThrow();
  });

  /**
   * **`system` 은 저장하지 않고 지운다.** "고른 적 없음" 과 같은 상태로 되돌려야
   * 나중에 기본값 정책이 바뀔 때 그 사람도 새 기본값을 따른다.
   */
  it("시스템 설정을 고르면 키를 지운다", () => {
    writeChoice("dark");
    expect(localStorage.getItem("dbmon.theme")).toBe("dark");
    writeChoice("system");
    expect(localStorage.getItem("dbmon.theme")).toBeNull();
    expect(readChoice()).toBe("system");
  });
});

describe("적용", () => {
  it("시스템 설정은 OS 를 따르고, 명시적 선택은 OS 를 덮는다", () => {
    stubMatchMedia(true);
    expect(resolveTheme("system")).toBe("dark");
    expect(resolveTheme("light")).toBe("light");

    stubMatchMedia(false);
    expect(resolveTheme("system")).toBe("light");
    expect(resolveTheme("dark")).toBe("dark");
  });

  it("`.dark` 클래스를 켜고 끈다", () => {
    stubMatchMedia(false);
    applyTheme("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    applyTheme("light");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  /**
   * 화면을 켜 둔 채로 OS 테마가 바뀌면(자동 야간 모드) 따라가야 한다 — 새로고침해야
   * 맞아지는 화면은 고장으로 읽힌다.
   */
  it("system 인 동안 OS 변경을 따라간다", () => {
    const { media, fire } = stubMatchMedia(false);
    const stop = startTheme();
    expect(document.documentElement.classList.contains("dark")).toBe(false);

    media.matches = true;
    fire();
    expect(document.documentElement.classList.contains("dark")).toBe(true);

    // **명시적으로 고른 뒤에는 OS 변경을 무시한다.**
    writeChoice("light");
    applyTheme("light");
    media.matches = true;
    fire();
    expect(document.documentElement.classList.contains("dark")).toBe(false);

    stop();
    media.matches = true;
    fire();
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  /** `matchMedia` 가 없는 환경(구형 브라우저·테스트)에서도 죽지 않는다. */
  it("matchMedia 가 없으면 라이트로 간다", () => {
    vi.stubGlobal("matchMedia", undefined);
    expect(resolveTheme("system")).toBe("light");
    const stop = startTheme();
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    stop();
  });
});
