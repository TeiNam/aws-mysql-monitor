import { Check, Clock, Pause, X } from "lucide-react";
import { isInstancePaused } from "../lib/pause";
import type { InstanceView, PausedScope } from "../lib/types";

/**
 * 인스턴스의 **실효 수집 상태**.
 *
 * # 왜 컴포넌트인가
 *
 * `InstanceView.state` 는 등록부의 생애 주기(`pending`/`collecting`/…)이고, 정지는 그 위에
 * 겹치는 **별개의 사실**이다(`PauseSet`). 그래서 `state` 만 찍으면 사람이 멈춘 인스턴스가
 * 계속 `collecting` 으로 보인다 — RDS 화면에서 정지를 눌렀는데 Instance Metrics 화면이
 * "collecting" 이라고 말하던 결함이 그것이었다.
 *
 * 판정을 두 화면이 각자 쓰면 또 갈라진다. 한 곳에 둔다.
 *
 * 구분해야 하는 네 가지 — 대응이 전부 다르다:
 *
 * | 상태 | 뜻 | 사람이 할 일 |
 * |---|---|---|
 * | 대기 | 등록만 됐다. 아직 시작하지 않았다 | 시작을 누른다 |
 * | 제외 | 탐색 필터에서 빠졌다(`collectible=false`) | 태그·필터를 고친다 |
 * | 정지 | 사람이 멈췄다 | 재개를 누른다 |
 * | 수집 | 돌고 있다 | 없다 |
 *
 * 색만으로 구분하지 않는다 — 아이콘과 글자를 함께 둔다.
 */
interface InstanceCollectionStatusProps {
  instance: InstanceView;
  pausedScopes: readonly PausedScope[];
}

export function InstanceCollectionStatus({
  instance,
  pausedScopes,
}: InstanceCollectionStatusProps) {
  if (instance.state === "pending") {
    // **"제외" 와 다르다.** 등록만 됐고 사람이 아직 시작하지 않았다.
    return (
      <span className="inline-flex items-center gap-1 text-blue-700">
        <Clock className="h-4 w-4" /> 대기
      </span>
    );
  }
  if (!instance.collectible) {
    return (
      <span className="inline-flex items-center gap-1 text-gray-500">
        <X className="h-4 w-4" /> 제외
      </span>
    );
  }
  if (isInstancePaused(pausedScopes, instance)) {
    return (
      <span className="inline-flex items-center gap-1 text-amber-700">
        <Pause className="h-4 w-4" /> 정지
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 text-green-700">
      <Check className="h-4 w-4" /> 수집
    </span>
  );
}
