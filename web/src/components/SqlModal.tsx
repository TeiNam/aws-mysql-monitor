import { Check, Copy, X } from "lucide-react";
import { useEffect, useState } from "react";
import { formatSql } from "../lib/sql";
import type { SqlRedactedReason } from "../lib/types";
import { BTN_GHOST } from "./ui";

interface SqlModalProps {
  title: string;
  sql: string | null;
  /** SQL 이 없는 이유. **`not_stored` 와 `insufficient_role` 을 구분해 말한다.** */
  reason?: SqlRedactedReason | null;
  truncated?: boolean;
  onClose: () => void;
}

/**
 * 저장된 SQL 팝업. 참조 대시보드의 `SQL Query` 모달과 같은 역할.
 *
 * **"전체" 라고 말하지 않는다** — 원문은 마스킹되거나(리터럴 정책) 잘렸거나
 * (`sql_text_truncated`) 권한이 없어 비어 있을 수 있고, 모달은 그 사실을 함께 띄운다.
 *
 * 표의 SQL 열은 한 줄로 잘려 있어서 **전문을 볼 방법이 반드시 있어야 한다.**
 * 참조 구현은 행을 클릭하면 화면 오른쪽에 고정 패널을 띄웠다. 여기서는 가운데
 * 모달로 두고 `Esc`·바깥 클릭으로 닫는다 — 키보드만으로도 닫을 수 있어야 한다.
 */
export function SqlModal({ title, sql, reason, truncated, onClose }: SqlModalProps) {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  async function copy() {
    if (sql === null) return;
    try {
      await navigator.clipboard.writeText(sql);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      // 클립보드 권한이 없을 수 있다(비-HTTPS 오리진 등). 조용히 실패하지만
      // 사용자는 텍스트를 직접 선택할 수 있다.
    }
  }

  return (
    <div
      // 배경 막. `gray-900` 은 다크에서 밝은 글자색으로 뒤집히므로 여기서는 검정을 쓴다.
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/40 p-6 dark:bg-black/60"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      onClick={onClose}
    >
      <div
        className="mt-10 flex max-h-[80vh] w-full max-w-4xl flex-col rounded-lg bg-white shadow-xl"
        // 안쪽 클릭은 닫지 않는다 — 텍스트를 선택하려면 필요하다.
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-gray-200 px-4 py-3">
          <h3 className="text-sm font-medium text-gray-800">{title}</h3>
          <div className="flex items-center gap-1">
            {sql === null ? null : (
              <button type="button" className={BTN_GHOST} onClick={() => void copy()}>
                {copied ? (
                  <>
                    <Check className="h-4 w-4 text-green-600" /> 복사됨
                  </>
                ) : (
                  <>
                    <Copy className="h-4 w-4" /> 복사
                  </>
                )}
              </button>
            )}
            <button type="button" className={BTN_GHOST} onClick={onClose} aria-label="닫기">
              <X className="h-4 w-4" />
            </button>
          </div>
        </div>

        <div className="overflow-auto p-4">
          {sql === null ? (
            <p className="text-sm text-gray-700">
              {reason === "insufficient_role" ? (
                <>
                  원문이 저장돼 있지만 <strong>열람 권한이 없다.</strong> 리터럴 열람 역할이
                  필요하다.
                </>
              ) : (
                <>
                  SQL 이 <strong>저장되지 않았다.</strong> 수집 정책이{" "}
                  <span className="font-mono">literal_policy=off</span> 이거나 마스킹에 실패한
                  레코드다.
                </>
              )}
            </p>
          ) : (
            <pre className="rounded-md bg-gray-50 p-3 font-mono text-xs whitespace-pre-wrap text-gray-800">
              {formatSql(sql)}
            </pre>
          )}
          {truncated === true ? (
            <p className="mt-2 text-xs text-amber-700">저장 한계로 잘렸다 — 뒷부분이 없다.</p>
          ) : null}
        </div>
      </div>
    </div>
  );
}
