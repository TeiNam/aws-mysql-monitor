/**
 * 화면 머리말. **다섯 화면이 같은 높이로 시작해야 한다.**
 *
 * # 왜 컴포넌트로 뽑았는가
 *
 * 화면마다 `<h1>` 을 직접 쓰다 보니 `/mysql` 만 부제 한 줄이 더 있었고, 그래서 탭을
 * 바꿀 때마다 **첫 카드가 28px 위아래로 튀었다**(실측: `/mysql` 176px vs 나머지 148px).
 * 눌러 보는 사람에게는 "화면이 시작하는 위치가 다르다" 로 보인다.
 *
 * 부제를 제목 **옆에** 두어 머리말을 항상 한 줄로 만든다 — 빈 줄을 예약해 높이를
 * 맞추는 방법도 있지만 그러면 부제 없는 네 화면이 28px 을 낭비한다.
 *
 * 좁은 화면에서는 부제를 숨긴다(`hidden sm:inline`). 줄바꿈을 허용하면 머리말이 두 줄이
 * 되어 **같은 문제가 좁은 화면에서 다시 생긴다.**
 */

import { PAGE_TITLE } from "./ui";

export function PageHeader({ title, subtitle }: { title: string; subtitle?: string }) {
  return (
    <div className="flex flex-wrap items-baseline gap-x-3">
      <h1 className={PAGE_TITLE}>{title}</h1>
      {subtitle === undefined ? null : (
        <p className="hidden text-sm text-gray-600 italic sm:inline">{subtitle}</p>
      )}
    </div>
  );
}
