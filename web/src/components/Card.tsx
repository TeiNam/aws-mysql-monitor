import type { ReactNode } from "react";
import { CARD, CARD_BODY, CARD_TITLE } from "./ui";

interface CardProps {
  title?: ReactNode;
  /** 제목 오른쪽 (필터·버튼). */
  actions?: ReactNode;
  /** 제목 아래 한 줄 설명. 제약·천장을 말하는 자리다. */
  note?: ReactNode;
  children: ReactNode;
  className?: string;
}

/** 참조 대시보드의 흰 카드. 모든 화면이 이 안에 들어간다. */
export function Card({ title, actions, note, children, className = "" }: CardProps) {
  return (
    <section className={`${CARD} ${className}`}>
      <div className={CARD_BODY}>
        {title === undefined && actions === undefined ? null : (
          <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
            <h2 className={CARD_TITLE}>{title}</h2>
            {actions === undefined ? null : (
              <div className="flex flex-wrap items-center gap-3">{actions}</div>
            )}
          </div>
        )}
        {note === undefined ? null : <p className="mb-3 text-xs text-gray-600">{note}</p>}
        {children}
      </div>
    </section>
  );
}
