import { Monitor, Moon, Settings, Sun } from "lucide-react";
import { useState } from "react";
import { Card } from "../components/Card";
import { PageHeader } from "../components/PageHeader";
import { Note } from "../components/Notices";
import { applyTheme, prefersDark, readChoice, writeChoice } from "../lib/theme";
import type { ThemeChoice } from "../lib/theme";

interface Option {
  value: ThemeChoice;
  label: string;
  hint: string;
  Icon: typeof Sun;
}

const THEMES: readonly Option[] = [
  {
    value: "system",
    label: "시스템 설정",
    hint: "OS 가 바뀌면 함께 바뀐다",
    Icon: Monitor,
  },
  { value: "light", label: "라이트", hint: "항상 밝게", Icon: Sun },
  { value: "dark", label: "다크", hint: "항상 어둡게", Icon: Moon },
];

/**
 * 옵션 화면. 지금은 테마 하나뿐이다.
 *
 * # 왜 별 화면인가
 *
 * 머리말에 토글을 두면 **여섯 번째 자리 다툼**이 시작된다(계정·리전·환경·연결 상태가
 * 이미 거기 있다). 설정은 자주 바꾸는 것이 아니므로 탭 하나로 밀어 두고, 다음 설정
 * (기본 시간대·기본 조회 구간 같은 것)이 생기면 여기 쌓는다.
 *
 * # 서버에 저장하지 않는다
 *
 * 테마는 **그 브라우저의 취향**이다. 서버에 두면 사용자 레코드에 필드가 하나 늘고
 * 인증이 필요해지는데, 얻는 것은 "다른 기기에서도 어둡다" 뿐이다.
 */
export function OptionsPage() {
  const [choice, setChoice] = useState<ThemeChoice>(() => readChoice());

  const pick = (next: ThemeChoice) => {
    // **적용을 먼저 한다.** 저장이 실패해도(사생활 보호 모드) 이번 세션은 바뀐다.
    applyTheme(next);
    writeChoice(next);
    setChoice(next);
  };

  return (
    <div className="space-y-6">
      <PageHeader title="Options" />

      <Card
        title={
          <>
            <Settings className="h-5 w-5 text-gray-500" /> 화면 테마
          </>
        }
        note={
          <>
            이 브라우저에만 저장된다(<span className="font-mono">localStorage</span>). 시스템
            설정은 지금 <strong>{prefersDark() ? "다크" : "라이트"}</strong> 다.
          </>
        }
      >
        <fieldset className="flex flex-wrap gap-3">
          <legend className="sr-only">화면 테마 선택</legend>
          {THEMES.map(({ value, label, hint, Icon }) => {
            const active = choice === value;
            return (
              <label
                key={value}
                className={`flex cursor-pointer items-start gap-3 rounded-lg border px-4 py-3 ${
                  active
                    ? "border-blue-500 bg-blue-50 ring-2 ring-blue-500/40"
                    : "border-gray-300 hover:bg-gray-50"
                }`}
              >
                <input
                  type="radio"
                  name="theme"
                  value={value}
                  checked={active}
                  onChange={() => pick(value)}
                  className="mt-1"
                />
                <span>
                  <span className="flex items-center gap-2 text-sm font-medium text-gray-900">
                    <Icon className="h-4 w-4 text-gray-500" />
                    {label}
                  </span>
                  <span className="mt-0.5 block text-xs text-gray-600">{hint}</span>
                </span>
              </label>
            );
          })}
        </fieldset>
        <Note>
          다크 모드는 색 계단을 뒤집어 만든다(<span className="font-mono">src/index.css</span>).
          새 화면을 그릴 때 그 계단 안의 색만 쓰면 두 모드가 함께 맞는다.
        </Note>
      </Card>
    </div>
  );
}
