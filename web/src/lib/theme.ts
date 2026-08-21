/**
 * 테마 선택 — 시스템 설정 / 라이트 / 다크.
 *
 * # 왜 세 가지인가
 *
 * 두 가지(라이트·다크)만 두면 **OS 를 따라가는 상태로 돌아올 방법이 없다.** 낮에는
 * 밝게 밤에는 어둡게 쓰는 사람이 하루에 두 번 이 화면에 들어와 손으로 바꿔야 한다.
 *
 * # 왜 `.dark` 클래스인가
 *
 * `prefers-color-scheme` 만으로는 화면에서 고를 수 없다(OS 설정을 바꾸러 나가야
 * 한다). 그래서 기본값은 OS 를 따르고, 고르면 `<html class="dark">` 로 덮는다.
 */

export type ThemeChoice = "system" | "light" | "dark";

/** localStorage 키. 사용자 설정이지 비밀이 아니다. */
const KEY = "dbmon.theme";

const CHOICES: readonly ThemeChoice[] = ["system", "light", "dark"];

/**
 * 저장된 선택. **모르는 값은 `system`** 이다 — 다른 버전이 남긴 값이나 사람이 손으로
 * 넣은 값에 화면이 걸려 넘어지지 않아야 한다.
 */
export function readChoice(): ThemeChoice {
  try {
    const raw = window.localStorage.getItem(KEY);
    return CHOICES.includes(raw as ThemeChoice) ? (raw as ThemeChoice) : "system";
  } catch {
    // 사생활 보호 모드에서 `localStorage` 접근이 던진다. 기본값으로 계속 간다.
    return "system";
  }
}

export function writeChoice(choice: ThemeChoice): void {
  try {
    if (choice === "system") {
      // **`"system"` 을 저장하지 않고 지운다.** 그러면 "고른 적 없음" 과 같은 상태가
      // 되어, 나중에 기본값 정책이 바뀌어도 그 사람은 새 기본값을 따른다.
      window.localStorage.removeItem(KEY);
    } else {
      window.localStorage.setItem(KEY, choice);
    }
  } catch {
    // 저장에 실패해도 이번 세션은 적용된다. 조용히 넘긴다 — 알릴 만한 실패가 아니다.
  }
}

/** OS 가 어두운 화면을 원하나. */
export function prefersDark(): boolean {
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches === true;
}

/** 실제로 적용될 테마. `system` 을 OS 설정으로 푼다. */
export function resolveTheme(choice: ThemeChoice): "light" | "dark" {
  if (choice === "system") return prefersDark() ? "dark" : "light";
  return choice;
}

/** `<html>` 에 반영한다. **여기가 유일한 DOM 접점이다.** */
export function applyTheme(choice: ThemeChoice): void {
  document.documentElement.classList.toggle("dark", resolveTheme(choice) === "dark");
}

/**
 * 기동 시 한 번 적용하고, **`system` 인 동안 OS 변경을 따라간다.**
 *
 * 구독하지 않으면 화면을 켜 둔 채로 OS 테마가 바뀔 때(자동 야간 모드) 어긋난 상태로
 * 남는다 — 새로고침해야 맞아지는 화면은 고장으로 읽힌다.
 *
 * 반환값은 구독 해제 함수다(테스트와 HMR 을 위해).
 */
export function startTheme(): () => void {
  applyTheme(readChoice());
  const media = window.matchMedia?.("(prefers-color-scheme: dark)");
  if (media === undefined) return () => {};
  const onChange = () => {
    // 사용자가 명시적으로 고른 상태에서는 OS 변경을 무시한다.
    if (readChoice() === "system") applyTheme("system");
  };
  media.addEventListener("change", onChange);
  return () => media.removeEventListener("change", onChange);
}
