import { Bell } from "lucide-react";
import { Card } from "../Card";
import { Note } from "../Notices";
import { PLACEHOLDERS, renderTemplate } from "../../lib/template";
import type { NotifySettings, SettingsProblem, SlackMode } from "../../lib/types";
import { Field, TextField, Toggle, problemOf } from "./fields";

/**
 * 알림 설정 — Slack 연결 + 문구.
 *
 * # 비밀은 여기 넣지 않는다
 *
 * 웹훅 URL·봇 토큰은 **Secrets Manager 에** 두고 그 이름/ARN 만 적는다
 * ([10 §3.4](../../../../docs/10-alerting.md)). 이 설정 항목은 viewer 도 읽을 수 있고
 * DynamoDB 백업·CloudTrail 로도 흐르므로, 값을 두면 통제할 수 없는 곳으로 퍼진다.
 *
 * 서버는 저장된 참조를 가려서 보낸다(`••••abcd`). 그 문자열을 그대로 되돌려 보내면
 * 서버가 기존 값을 지킨다 — 새로 입력할 때만 실제 값이 간다.
 */
export function NotifySection({
  value,
  onChange,
  problems,
  disabled,
}: {
  value: NotifySettings;
  onChange: (next: NotifySettings) => void;
  problems: SettingsProblem[];
  disabled: boolean;
}) {
  const set = <K extends keyof NotifySettings>(key: K, v: NotifySettings[K]) =>
    onChange({ ...value, [key]: v });

  return (
    <Card
      title={
        <>
          <Bell className="h-5 w-5 text-gray-500" /> 알림
        </>
      }
    >
      <div className="space-y-4">
        <Toggle
          label="Slack 으로 알린다"
          hint="끄면 규칙이 평가돼도 발송하지 않는다. 설정은 남는다."
          checked={value.slack_enabled}
          onChange={(v) => set("slack_enabled", v)}
          disabled={disabled}
        />

        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            label="연결 방식"
            hint={
              value.slack_mode === "webhook"
                ? "Incoming Webhook — URL 하나. 채널이 URL 에 고정된다."
                : "Bot Token — 채널을 고를 수 있고 스레드·수정이 된다(앱 설치 필요)."
            }
          >
            <select
              className="w-full rounded-md border border-gray-300 bg-white px-3 py-1.5 text-sm disabled:bg-gray-100"
              value={value.slack_mode}
              disabled={disabled}
              onChange={(e) => set("slack_mode", e.target.value as SlackMode)}
            >
              <option value="webhook">Incoming Webhook</option>
              <option value="bot_token">Bot Token</option>
            </select>
          </Field>

          <TextField
            label={value.slack_mode === "bot_token" ? "채널 (필수)" : "채널 (표기용)"}
            hint="`C01234ABCDE` 또는 `#dba-alerts`"
            value={value.slack_channel}
            onChange={(v) => set("slack_channel", v)}
            error={problemOf(problems, "notify.slack_channel")}
            placeholder="#dba-alerts"
            disabled={disabled}
            mono
          />
        </div>

        <TextField
          label="Secrets Manager 참조"
          hint={
            <>
              웹훅 URL·봇 토큰이 담긴 <strong>시크릿의 이름 또는 ARN</strong>. 값 자체를 여기
              적지 않는다 — 이 설정은 뷰어도 읽고 백업으로도 나간다.
            </>
          }
          value={value.slack_secret}
          onChange={(v) => set("slack_secret", v)}
          error={problemOf(problems, "notify.slack_secret")}
          placeholder="dbmon/channel/slack"
          disabled={disabled}
          mono
        />

        <Field
          label="알림 문구"
          hint={
            <>
              자리표시자: <span className="font-mono">{PLACEHOLDERS.map((p) => `{${p}}`).join(" ")}</span>
            </>
          }
          error={problemOf(problems, "notify.message_template")}
        >
          <textarea
            className="min-h-[5rem] w-full rounded-md border border-gray-300 bg-white px-3 py-1.5 font-mono text-sm text-gray-900 disabled:bg-gray-100"
            value={value.message_template}
            disabled={disabled}
            spellCheck={false}
            onChange={(e) => set("message_template", e.target.value)}
          />
        </Field>

        {/* **미리보기가 있어야 자리표시자 오타를 눈으로 잡는다.** 서버 검증도
            모르는 이름을 거부하지만, 문장이 어떻게 읽히는지는 보여야 안다. */}
        <div className="rounded-md border border-gray-200 bg-gray-50 p-3">
          <span className="text-xs font-medium text-gray-600">미리보기</span>
          <pre className="mt-1 overflow-x-auto text-sm whitespace-pre-wrap text-gray-900">
            {renderTemplate(value.message_template)}
          </pre>
        </div>

        <Note>
          알림 본문에는 <strong>정규화된 SQL 만</strong> 담긴다(T-23). 리터럴은 Slack 같은
          제3자 SaaS 로 나가지 않고, 딥링크 뒤의 인증·권한·감사가 걸린 화면에만 있다.
        </Note>
      </div>
    </Card>
  );
}
