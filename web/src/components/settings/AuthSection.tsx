import { ShieldAlert, ShieldCheck } from "lucide-react";
import { Card } from "../Card";
import { Note } from "../Notices";
import type { AuthModeSetting, AuthSettings, SettingsProblem, SettingsView } from "../../lib/types";
import { TextField, problemOf } from "./fields";

/**
 * 로그인 설정 — 방식 토글 + Cognito 연결.
 *
 * # 인증 끄기는 두 곳이 허용해야 한다
 *
 * 이 화면은 인증 뒤에 있다. 끄는 순간 이 화면도 누구에게나 열리고, 되돌리려면 다시 켤
 * 권한이 필요한데 그걸 판정할 근거가 사라진 상태다. 그래서 배포 설정
 * (`http.allow_auth_disable`)이 먼저 허용해야 하고, 이 화면이 두 번째 허용이다.
 *
 * # 저장은 되지만 적용되지 않는 상태를 숨기지 않는다
 *
 * Cognito 검증기가 아직 없다. 고르면 저장은 되고 **적용은 토큰 방식으로 떨어진다** —
 * 검증 없이 통과시키는 것보다 낫다. 그 사실을 화면이 말해야 "저장했는데 왜 그대로냐" 가
 * 되지 않는다.
 */
export function AuthSection({
  value,
  onChange,
  problems,
  disabled,
  view,
}: {
  value: AuthSettings;
  onChange: (next: AuthSettings) => void;
  problems: SettingsProblem[];
  disabled: boolean;
  view: SettingsView;
}) {
  const setCognito = <K extends keyof AuthSettings["cognito"]>(
    key: K,
    v: AuthSettings["cognito"][K],
  ) => onChange({ ...value, cognito: { ...value.cognito, [key]: v } });

  const chosen = value.mode;
  const effective = view.effective_auth_mode;
  // **고른 것과 적용되는 것이 다를 수 있다.** 그 간극이 이 화면의 핵심 정보다.
  const ignored = chosen !== effective;

  return (
    <Card
      title={
        <>
          {effective === "off" ? (
            <ShieldAlert className="h-5 w-5 text-red-600" />
          ) : (
            <ShieldCheck className="h-5 w-5 text-gray-500" />
          )}
          로그인
        </>
      }
    >
      <div className="space-y-4">
        <fieldset className="space-y-2">
          <legend className="text-sm font-medium text-gray-800">인증 방식</legend>
          <ModeChoice
            mode="token"
            label="공유 토큰"
            hint="배포 설정의 토큰으로 접속한다. 로컬 개발은 루프백에서 토큰 없이 통과한다."
            chosen={chosen}
            onChange={(mode) => onChange({ ...value, mode })}
            disabled={disabled}
          />
          {/*
           * **검증기가 없으면 고를 수 없다.**
           *
           * 한때 "저장은 되지만 토큰 방식으로 적용된다" 고 안내했는데 그게 틀렸다 —
           * 서버는 이제 Cognito 를 유지하고 거부한다(토큰으로 내려가면 권한 상승이다).
           * 그 상태로 저장하면 **되돌릴 설정 화면까지 닫힌다.**
           *
           * 그래서 서버가 저장을 거부하고(`put_settings`), 화면도 선택을 막는다.
           */}
          <ModeChoice
            mode="cognito"
            label="Cognito"
            hint={
              view.cognito_ready
                ? "사용자 풀의 JWT 를 검증한다."
                : "⚠ 이 워커에 검증기가 배선되지 않았다 — 고르면 아무도 못 들어온다. 저장이 거부된다."
            }
            chosen={chosen}
            onChange={(mode) => onChange({ ...value, mode })}
            disabled={disabled || !view.cognito_ready}
          />
          <ModeChoice
            mode="off"
            label="인증 없음"
            hint={
              view.allow_auth_disable
                ? "⚠ 접근할 수 있는 누구나 admin 이 된다. VPN·사설 ALB 뒤에서만 쓴다."
                : "배포 설정이 허용하지 않는다 — `http.allow_auth_disable = true` 가 먼저 필요하다."
            }
            chosen={chosen}
            onChange={(mode) => onChange({ ...value, mode })}
            // **파일이 허용하지 않으면 고를 수 없다.** 고르게 해 두고 무시하면
            // "저장했는데 안 꺼진다" 로 보인다.
            disabled={disabled || !view.allow_auth_disable}
          />
        </fieldset>

        {ignored ? (
          <p className="rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-800">
            고른 방식은 <span className="font-mono">{chosen}</span> 인데 지금 적용되는 것은{" "}
            <span className="font-mono">{effective}</span> 다.{" "}
            {chosen === "cognito"
              ? "Cognito 설정이 불완전하거나 검증기가 없다."
              : "배포 설정이 이 방식을 허용하지 않는다."}
          </p>
        ) : null}

        {effective === "off" ? (
          <p className="rounded-md bg-red-50 px-3 py-2 text-xs font-medium text-red-800" role="alert">
            지금 이 배포는 <strong>인증이 없다.</strong> 접근할 수 있는 누구나 모든 화면과
            조작 권한을 갖는다.
          </p>
        ) : null}

        <div className="grid gap-4 border-t border-gray-200 pt-4 sm:grid-cols-2">
          <TextField
            label="사용자 풀 ID"
            hint="`ap-northeast-2_AbCdEf123` — 접두어에서 리전을 읽는다"
            value={value.cognito.user_pool_id}
            onChange={(v) => setCognito("user_pool_id", v)}
            error={problemOf(problems, "auth.cognito.user_pool_id")}
            placeholder="ap-northeast-2_AbCdEf123"
            disabled={disabled}
            mono
          />
          <TextField
            label="앱 클라이언트 ID"
            value={value.cognito.client_id}
            onChange={(v) => setCognito("client_id", v)}
            error={problemOf(problems, "auth.cognito.client_id")}
            disabled={disabled}
            mono
          />
          <TextField
            label="리전 (선택)"
            hint="풀 ID 접두어와 다를 때만 적는다"
            value={value.cognito.region}
            onChange={(v) => setCognito("region", v)}
            error={problemOf(problems, "auth.cognito.region")}
            placeholder={view.own_region}
            disabled={disabled}
            mono
          />
          <TextField
            label="호스팅 UI 도메인"
            hint="로그인 화면으로 보낼 주소"
            value={value.cognito.domain}
            onChange={(v) => setCognito("domain", v)}
            error={problemOf(problems, "auth.cognito.domain")}
            placeholder="https://dbmon.auth.ap-northeast-2.amazoncognito.com"
            disabled={disabled}
          />
        </div>

        <Note>
          Cognito 설정은 <strong>비밀이 아니다</strong> — 풀 ID·클라이언트 ID·도메인은 로그인
          전에 브라우저가 알아야 하는 공개 값이다. 클라이언트 시크릿을 쓰는 앱 클라이언트는
          이 화면에 넣지 않는다(SPA 는 시크릿을 쓰지 않는다).
        </Note>
      </div>
    </Card>
  );
}

function ModeChoice({
  mode,
  label,
  hint,
  chosen,
  onChange,
  disabled,
}: {
  mode: AuthModeSetting;
  label: string;
  hint: string;
  chosen: AuthModeSetting;
  onChange: (m: AuthModeSetting) => void;
  disabled: boolean;
}) {
  const active = chosen === mode;
  return (
    <label
      className={`flex cursor-pointer items-start gap-3 rounded-lg border px-3 py-2 ${
        active ? "border-blue-500 bg-blue-50" : "border-gray-300"
      } ${disabled ? "cursor-not-allowed opacity-60" : "hover:bg-gray-50"}`}
    >
      <input
        type="radio"
        name="auth-mode"
        className="mt-1"
        checked={active}
        disabled={disabled}
        onChange={() => onChange(mode)}
      />
      <span>
        <span className="block text-sm font-medium text-gray-900">{label}</span>
        <span className="mt-0.5 block text-xs text-gray-600">{hint}</span>
      </span>
    </label>
  );
}
