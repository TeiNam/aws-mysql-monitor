import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Monitor, Moon, RotateCcw, Save, Settings, Sun } from "lucide-react";
import { useEffect, useState } from "react";
import { Card } from "../components/Card";
import { ErrorNotice, Note, Pending } from "../components/Notices";
import { PageHeader } from "../components/PageHeader";
import { AiSection } from "../components/settings/AiSection";
import { AuthSection } from "../components/settings/AuthSection";
import { DiscoverySection } from "../components/settings/DiscoverySection";
import { NotifySection } from "../components/settings/NotifySection";
import { BTN_GHOST, BTN_PRIMARY } from "../components/ui";
import { ApiError, SettingsInvalid, fetchSettings, queryKeys, saveSettings } from "../lib/api";
import { fmtListTime } from "../lib/format";
import { applyTheme, prefersDark, readChoice, writeChoice } from "../lib/theme";
import type { ThemeChoice } from "../lib/theme";
import type { AppSettings, SettingsProblem, SettingsView } from "../lib/types";

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
 * 옵션 화면 — **두 종류의 설정**이 한 화면에 있다.
 *
 * | 설정 | 저장 위치 | 적용 범위 |
 * |---|---|---|
 * | 화면 테마 | 브라우저(`localStorage`) | 이 브라우저만 |
 * | 알림·탐색·로그인·AI | 서버(DynamoDB `CFG/GLOBAL`) | 배포 전체 |
 *
 * 섞어 두는 이유는 사용자에게 "설정" 이 한 곳이어야 하기 때문이다. 대신 각 카드가
 * 자기 저장 위치를 말한다 — 테마를 바꿨는데 "저장" 을 눌러야 하는지 헷갈리면 안 된다.
 *
 * # 서버 설정은 초안(draft)으로 편집한다
 *
 * 필드를 칠 때마다 저장하면 (a) 검증이 매 글자마다 실패하고 (b) 리전 목록을 지우는 중간
 * 상태가 그대로 적용된다. 그래서 **저장 버튼**을 누를 때 한 번에 보낸다.
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
      <PageHeader title="Options" subtitle="= 화면 취향 + 배포 설정 =" />

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

      <ServerSettings />
    </div>
  );
}

/** 서버 설정 묶음. 초안을 들고 편집하다가 한 번에 저장한다. */
function ServerSettings() {
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: queryKeys.settings,
    queryFn: ({ signal }) => fetchSettings(signal),
    // **자동 재조회를 끈다.** 편집 중에 서버 응답이 오면 초안이 덮이거나
    // "저장하지 않은 변경" 표시가 흔들린다.
    refetchOnWindowFocus: false,
  });

  const [draft, setDraft] = useState<AppSettings | null>(null);
  /** 저장 시 서버가 돌려준 필드별 사유. 저장 전에는 서버가 준 현재 값의 문제를 쓴다. */
  const [problems, setProblems] = useState<SettingsProblem[] | null>(null);
  const [saved, setSaved] = useState(false);

  // 서버 값이 처음 오면 초안을 채운다. **이미 편집 중이면 덮지 않는다.**
  useEffect(() => {
    if (query.data !== undefined && draft === null) setDraft(query.data.settings);
  }, [query.data, draft]);

  const mutation = useMutation({
    mutationFn: (next: AppSettings) => saveSettings(next.version, next),
    onSuccess: (view: SettingsView) => {
      // 서버가 돌려준 값이 진실이다(버전이 올라가 있고 비밀은 가려져 있다).
      setDraft(view.settings);
      setProblems(view.problems);
      setSaved(true);
      queryClient.setQueryData(queryKeys.settings, view);
    },
    onError: (e: unknown) => {
      setSaved(false);
      setProblems(e instanceof SettingsInvalid ? e.problems : null);
    },
  });

  if (query.error !== null) {
    return <ErrorNotice error={query.error} onRetry={() => void query.refetch()} />;
  }
  if (query.isPending || query.data === undefined || draft === null) {
    return <Pending label="설정을 불러오는 중…" />;
  }

  const view = query.data;
  const readOnly = !view.can_edit || mutation.isPending;
  const dirty = JSON.stringify(draft) !== JSON.stringify(view.settings);
  // 저장 전에는 **서버가 판정한 현재 값의 문제**를 보여준다. 저장 시도 후에는
  // 그 응답의 문제를 보여준다 — 둘을 섞으면 이미 고친 오류가 계속 남는다.
  const shown = problems ?? view.problems;

  return (
    <>
      <NotifySection
        value={draft.notify}
        onChange={(notify) => setDraft({ ...draft, notify })}
        problems={shown}
        disabled={readOnly}
      />
      <DiscoverySection
        value={draft.discovery}
        onChange={(discovery) => setDraft({ ...draft, discovery })}
        problems={shown}
        disabled={readOnly}
        ownRegion={view.own_region}
      />
      <AuthSection
        value={draft.auth}
        onChange={(auth) => setDraft({ ...draft, auth })}
        problems={shown}
        disabled={readOnly}
        view={view}
      />
      <AiSection
        value={draft.ai}
        onChange={(ai) => setDraft({ ...draft, ai })}
        problems={shown}
        disabled={readOnly}
        ownRegion={view.own_region}
      />

      {/* **저장 줄은 맨 아래에 둔다.** 화면이 길어 위쪽 고정 바가 카드를 덮는다. */}
      <Card>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <span className="text-xs text-gray-600">
            {view.can_edit ? null : (
              <span className="font-medium text-amber-700">
                저장 권한이 없다(admin 전용) — 값은 읽을 수 있다.{" "}
              </span>
            )}
            버전 <span className="font-mono">{draft.version}</span>
            {draft.updated_by === "" ? (
              " · 아직 저장된 적이 없다"
            ) : (
              <>
                {" · "}
                {fmtListTime(draft.updated_at_ms, "KST", Date.now())} · {draft.updated_by}
              </>
            )}
          </span>
          <span className="flex items-center gap-2">
            {dirty ? (
              <span className="text-xs font-medium text-amber-700">저장하지 않은 변경이 있다</span>
            ) : saved ? (
              <span className="text-xs font-medium text-green-700">저장했다</span>
            ) : null}
            <button
              type="button"
              className={BTN_GHOST}
              disabled={!dirty || mutation.isPending}
              onClick={() => {
                setDraft(view.settings);
                setProblems(view.problems);
              }}
            >
              <RotateCcw className="h-4 w-4" /> 되돌리기
            </button>
            <button
              type="button"
              className={BTN_PRIMARY}
              disabled={!dirty || readOnly}
              onClick={() => {
                setSaved(false);
                mutation.mutate(draft);
              }}
            >
              <Save className="h-4 w-4" /> {mutation.isPending ? "저장 중…" : "저장"}
            </button>
          </span>
        </div>
        {mutation.error === null ? null : <SaveError error={mutation.error} />}
      </Card>
    </>
  );
}

/**
 * 저장 실패 안내. **버전 충돌을 일반 오류와 구분한다** — 그건 "다시 읽어야 한다" 는
 * 뜻이고, 사용자가 할 일이 완전히 다르다.
 */
function SaveError({ error }: { error: unknown }) {
  if (error instanceof SettingsInvalid) {
    return (
      <p className="mt-3 rounded-md bg-red-50 px-3 py-2 text-xs text-red-800" role="alert">
        값이 올바르지 않아 저장하지 않았다 — 문제가 있는 필드 아래에 사유가 표시된다.
      </p>
    );
  }
  const conflict = error instanceof ApiError && error.status === 409;
  return (
    <p className="mt-3 rounded-md bg-red-50 px-3 py-2 text-xs text-red-800" role="alert">
      {conflict ? (
        <>
          그 사이 다른 관리자가 설정을 저장했다. <strong>덮어쓰지 않았다</strong> — 화면을
          새로 고쳐 최신 값을 받은 뒤 다시 편집한다.
        </>
      ) : (
        <>
          저장하지 못했다:{" "}
          <span className="font-mono">
            {error instanceof ApiError ? error.code : "unknown_error"}
          </span>
        </>
      )}
    </p>
  );
}
