import { Sparkles } from "lucide-react";
import { Card } from "../Card";
import { Note } from "../Notices";
import { regionLabel } from "../../lib/regions";
import type { AiSettings, SettingsProblem } from "../../lib/types";
import { NumberField, TextField, Toggle, problemOf } from "./fields";

/**
 * 알려진 Bedrock 모델 ID. **하드코딩된 목록이 아니라 힌트다** — 계정마다 쓸 수 있는
 * 프로파일이 다르고, 없는 것을 고르면 호출 시점에 400 이 온다.
 *
 * 그래서 입력은 자유 텍스트이고, 이 목록은 자동완성으로만 쓴다.
 */
const KNOWN_MODELS = [
  "global.anthropic.claude-sonnet-5",
  "global.anthropic.claude-opus-5",
  "global.anthropic.claude-haiku-4-5-20251001-v1:0",
] as const;

/**
 * AI 튜닝 설정 (Bedrock).
 *
 * # 왜 모델 ID 를 코드에 두지 않는가
 *
 * 모델 교체가 배포 없이 되어야 하고([11 §7](../../../../.claude/docs/11-ai-advisor.md)),
 * 리전마다 쓸 수 있는 추론 프로파일이 다르다. 결과에는 실제로 쓴 모델 ID 를 함께
 * 저장하므로, 나중에 "이 권고는 어느 모델이 낸 것인가" 를 답할 수 있다.
 */
export function AiSection({
  value,
  onChange,
  problems,
  disabled,
  ownRegion,
}: {
  value: AiSettings;
  onChange: (next: AiSettings) => void;
  problems: SettingsProblem[];
  disabled: boolean;
  ownRegion: string;
}) {
  const set = <K extends keyof AiSettings>(key: K, v: AiSettings[K]) =>
    onChange({ ...value, [key]: v });

  return (
    <Card
      title={
        <>
          <Sparkles className="h-5 w-5 text-gray-500" /> AI 튜닝 (Bedrock)
        </>
      }
    >
      <div className="space-y-4">
        <Toggle
          label="튜닝 분석을 켠다"
          hint="계획 화면의 Tuning 버튼이 이 설정으로 동작한다. 끄면 버튼이 사유를 표시한다."
          checked={value.enabled}
          onChange={(v) => set("enabled", v)}
          disabled={disabled}
        />

        <div className="grid gap-4 sm:grid-cols-2">
          <div className="sm:col-span-2">
            <TextField
              label="모델 ID"
              hint="추론 프로파일 ID 를 권한다 — 교차 리전으로 처리돼 조절(throttle)에 강하다."
              value={value.model_id}
              onChange={(v) => set("model_id", v)}
              error={problemOf(problems, "ai.model_id")}
              placeholder={KNOWN_MODELS[0]}
              disabled={disabled}
              mono
            />
            {/* 자동완성 힌트. 오타 하나가 호출 시점 400 이 되므로 눈에 보이게 둔다. */}
            <span className="mt-1 flex flex-wrap gap-1">
              {KNOWN_MODELS.map((m) => (
                <button
                  key={m}
                  type="button"
                  disabled={disabled}
                  onClick={() => set("model_id", m)}
                  className="rounded bg-gray-100 px-1.5 py-0.5 font-mono text-xs text-gray-700 ring-1 ring-gray-300 hover:bg-gray-200 disabled:opacity-60"
                >
                  {m}
                </button>
              ))}
            </span>
          </div>

          <TextField
            label="Bedrock 리전"
            hint={`비우면 ${regionLabel(ownRegion)}`}
            value={value.region}
            onChange={(v) => set("region", v)}
            error={problemOf(problems, "ai.region")}
            placeholder={ownRegion}
            disabled={disabled}
            mono
          />
          <NumberField
            label="출력 토큰 상한"
            hint="작으면 문서가 중간에서 끊긴다 (권장 4000 이상)"
            value={value.max_output_tokens}
            onChange={(v) => set("max_output_tokens", v)}
            error={problemOf(problems, "ai.max_output_tokens")}
            disabled={disabled}
          />
        </div>

        <Note>
          모델에 보내는 것은 <strong>정규화된 SQL·마스킹된 실행계획·스키마 명세</strong>다 —
          리터럴은 보내지 않는다(FR-AI-04, 08 §6 의 egress 인벤토리). DDL 을 자동으로
          적용하지도 않는다(FR-AI-09): 복사할 수 있는 문장까지가 이 기능의 경계다.
        </Note>
      </div>
    </Card>
  );
}
