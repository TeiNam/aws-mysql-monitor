import { BTN_GHOST } from "./ui";

interface PaginationProps {
  page: number;
  pageSize: number;
  total: number;
  onChange: (page: number) => void;
  /** 서버가 상한에 걸렸다. **마지막 페이지가 끝이 아니라는 뜻이다.** */
  truncated?: boolean;
}

/** 참조 대시보드와 같은 `<< < 1 2 3 … > >>`. 최대 10개 번호를 보여준다. */
const MAX_NUMBERS = 10;

function pageNumbers(page: number, totalPages: number): number[] {
  const count = Math.min(totalPages, MAX_NUMBERS);
  let start = Math.max(1, page - Math.floor(count / 2));
  const end = Math.min(start + count - 1, totalPages);
  if (end - start + 1 < count) start = Math.max(1, end - count + 1);
  const out: number[] = [];
  for (let i = start; i <= end; i += 1) out.push(i);
  return out;
}

export function Pagination({ page, pageSize, total, onChange, truncated }: PaginationProps) {
  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  if (total === 0) return null;

  const from = (page - 1) * pageSize + 1;
  const to = Math.min(page * pageSize, total);

  return (
    <nav
      className="mt-4 flex flex-wrap items-center justify-between gap-3 border-t border-gray-200 pt-3"
      aria-label="페이지네이션"
    >
      <p className="text-xs text-gray-600">
        {from.toLocaleString("ko-KR")}–{to.toLocaleString("ko-KR")} / {total.toLocaleString("ko-KR")}
        건
        {truncated === true ? (
          <span className="ml-2 text-amber-700">
            (조회 상한에 걸렸다 — 이보다 더 있을 수 있다)
          </span>
        ) : null}
      </p>
      <div className="flex items-center gap-1">
        <button
          type="button"
          className={BTN_GHOST}
          onClick={() => onChange(1)}
          disabled={page === 1}
          aria-label="첫 페이지"
        >
          {"<<"}
        </button>
        <button
          type="button"
          className={BTN_GHOST}
          onClick={() => onChange(page - 1)}
          disabled={page === 1}
          aria-label="이전 페이지"
        >
          {"<"}
        </button>
        {pageNumbers(page, totalPages).map((n) => (
          <button
            key={n}
            type="button"
            onClick={() => onChange(n)}
            aria-label={`페이지 ${n}`}
            aria-current={n === page ? "page" : undefined}
            className={`min-w-9 rounded-md px-2 py-1.5 text-sm font-medium ${
              n === page
                ? "bg-blue-50 text-blue-700 ring-1 ring-blue-500"
                : "text-gray-600 hover:bg-gray-100"
            }`}
          >
            {n}
          </button>
        ))}
        <button
          type="button"
          className={BTN_GHOST}
          onClick={() => onChange(page + 1)}
          disabled={page >= totalPages}
          aria-label="다음 페이지"
        >
          {">"}
        </button>
        <button
          type="button"
          className={BTN_GHOST}
          onClick={() => onChange(totalPages)}
          disabled={page >= totalPages}
          aria-label="마지막 페이지"
        >
          {">>"}
        </button>
      </div>
    </nav>
  );
}
