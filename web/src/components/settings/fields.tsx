import type { ReactNode } from "react";
import { regionLabel } from "../../lib/regions";
import type { SettingsProblem } from "../../lib/types";

/**
 * 설정 화면의 입력 조각들.
 *
 * # 왜 따로 두는가
 *
 * 설정 화면은 필드가 스무 개를 넘는다. 각 절이 라벨·설명·오류 표시를 각자 만들면
 * **같은 종류의 필드가 절마다 다르게 보이고**, 오류 표시를 한 곳만 빠뜨려도
 * "저장이 안 되는데 이유를 모르는" 상태가 된다.
 */

/** 이 필드의 문제. 서버가 `notify.slack_secret` 처럼 경로로 알려준다. */
export function problemOf(problems: SettingsProblem[], field: string): string | null {
  return problems.find((p) => p.field === field)?.message ?? null;
}

const INPUT =
  "w-full rounded-md border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-900 disabled:bg-gray-100 disabled:text-gray-500";

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode | undefined;
  // `exactOptionalPropertyTypes` 라 **`undefined` 를 타입에 적어야** 한다 —
  // 상위 컴포넌트의 옵셔널 prop 을 그대로 넘기면 그 값이 `undefined` 일 수 있다.
  error?: string | null | undefined;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <span className="block text-sm font-medium text-gray-800">{label}</span>
      {hint === undefined ? null : <span className="mt-0.5 block text-xs text-gray-600">{hint}</span>}
      <span className="mt-1 block">{children}</span>
      {/* **오류는 필드 아래에 둔다.** 화면 위쪽에 모아 두면 어느 입력이 틀렸는지
          찾아야 하고, 필드가 스무 개면 그게 곧 포기다. */}
      {error === null || error === undefined ? null : (
        <span className="mt-1 block text-xs font-medium text-red-700">{error}</span>
      )}
    </label>
  );
}

export function TextField({
  label,
  hint,
  error,
  value,
  onChange,
  placeholder,
  disabled,
  mono,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  disabled?: boolean;
  mono?: boolean;
}) {
  return (
    <Field label={label} hint={hint} error={error}>
      <input
        type="text"
        className={`${INPUT} ${mono === true ? "font-mono" : ""}`}
        value={value}
        placeholder={placeholder}
        disabled={disabled === true}
        onChange={(e) => onChange(e.target.value)}
        spellCheck={false}
        autoComplete="off"
      />
    </Field>
  );
}

export function NumberField({
  label,
  hint,
  error,
  value,
  onChange,
  disabled,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  value: number;
  onChange: (v: number) => void;
  disabled?: boolean;
}) {
  return (
    <Field label={label} hint={hint} error={error}>
      <input
        type="number"
        className={INPUT}
        value={value}
        disabled={disabled === true}
        // **빈 입력을 0 으로 접지 않는다.** 지우는 중에 0 이 저장되면 검증이
        // 엉뚱한 사유로 막힌다.
        onChange={(e) => {
          const n = Number.parseInt(e.target.value, 10);
          if (!Number.isNaN(n)) onChange(n);
        }}
      />
    </Field>
  );
}

export function Toggle({
  label,
  hint,
  checked,
  onChange,
  disabled,
}: {
  label: string;
  hint?: ReactNode;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <label className="flex items-start gap-3">
      <input
        type="checkbox"
        className="mt-0.5 size-4"
        checked={checked}
        disabled={disabled === true}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span>
        <span className="block text-sm font-medium text-gray-800">{label}</span>
        {hint === undefined ? null : <span className="mt-0.5 block text-xs text-gray-600">{hint}</span>}
      </span>
    </label>
  );
}

/**
 * 문자열 목록 편집기 (리전 코드 등).
 *
 * **줄바꿈·쉼표로 나눈다.** 항목마다 입력칸을 만들면 리전 다섯 개를 넣는 데 클릭이
 * 열 번이고, 붙여넣기가 안 된다(운영자는 보통 목록을 어딘가에서 복사해 온다).
 */
export function StringList({
  label,
  hint,
  error,
  value,
  onChange,
  placeholder,
  disabled,
  renderItem,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  value: string[];
  onChange: (v: string[]) => void;
  placeholder?: string;
  disabled?: boolean;
  /** 미리보기 표기(리전이면 이름을 붙인다). */
  renderItem?: (item: string) => string;
}) {
  return (
    <Field label={label} hint={hint} error={error}>
      <textarea
        className={`${INPUT} min-h-[4.5rem] font-mono`}
        value={value.join("\n")}
        placeholder={placeholder}
        disabled={disabled === true}
        spellCheck={false}
        onChange={(e) => onChange(splitList(e.target.value))}
      />
      {value.length === 0 ? null : (
        <span className="mt-1 flex flex-wrap gap-1">
          {value.map((item) => (
            <span
              key={item}
              className="rounded bg-gray-100 px-1.5 py-0.5 text-xs text-gray-700 ring-1 ring-gray-300"
            >
              {renderItem === undefined ? item : renderItem(item)}
            </span>
          ))}
        </span>
      )}
    </Field>
  );
}

/** 리전 목록 전용 — 칩에 이름을 붙여 `ap-northeast-1` 과 `-2` 를 눈으로 구분하게 한다. */
export function RegionList(props: Omit<Parameters<typeof StringList>[0], "renderItem">) {
  return <StringList {...props} renderItem={regionLabel} />;
}

/**
 * 줄바꿈·쉼표·공백으로 나눈다. **빈 항목을 남기지 않는다** — 빈 문자열이 리전 코드로
 * 들어가면 검증이 "형식이 아니다" 로 막는데, 사용자는 빈 줄을 못 본다.
 */
export function splitList(raw: string): string[] {
  return raw
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter((s) => s !== "");
}
