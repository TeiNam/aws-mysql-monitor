#!/usr/bin/env python3
"""문서 링크·구조 검사. `just docs-check` 가 호출한다.

CI 게이트로 쓰므로 **문제가 있으면 종료 코드 1**을 반환한다.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def markdown_files():
    return sorted(
        [*ROOT.glob("docs/*.md"), ROOT / "README.md", ROOT / "infra/README.md"]
    )


def check_links(files):
    """상대 링크가 실제 파일을 가리키는지."""
    broken = []
    for f in files:
        for m in re.finditer(r"\[[^\]]*\]\(([^)#\s]+)(#[^)]*)?\)", f.read_text()):
            target = m.group(1)
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            if not (f.parent / target).resolve().exists():
                line = f.read_text()[: m.start()].count("\n") + 1
                broken.append(f"{f.relative_to(ROOT)}:{line} → {target}")
    return broken


def check_suspicious_chars(files):
    """오타로 섞여 들어간 비한글·비라틴 문자를 찾는다.

    이전에 '다و계정'(아랍 문자 혼입)이 있었다. 눈으로는 거의 안 보인다.
    """
    import unicodedata

    allowed_scripts = ("HANGUL", "LATIN", "DIGIT", "CJK")
    # 의도적으로 쓰는 기술 표기. 화이트리스트로 두어 검사가 실제 오타만 잡게 한다.
    allowed_chars = {
        "\u03b1",  # α — 지수 이동 평균 계수
        "\u00b5",  # µ — 마이크로초
        "\u03bc",  # μ — 그리스 mu (µ 와 다른 코드포인트다)
        "\u2264",  # ≤
        "\u2265",  # ≥
        "\u2229",  # ∩ — 교집합
    }
    found = []
    for f in files:
        for i, line in enumerate(f.read_text().splitlines(), 1):
            for ch in line:
                if ch.isalpha() and ord(ch) > 0x7F and ch not in allowed_chars:
                    name = unicodedata.name(ch, "")
                    if not any(s in name for s in allowed_scripts):
                        found.append(
                            f"{f.relative_to(ROOT)}:{i} → {ch!r} ({name or 'UNKNOWN'})"
                        )
    return found


def main() -> int:
    files = markdown_files()
    total_lines = sum(len(f.read_text().splitlines()) for f in files)

    broken = check_links(files)
    suspicious = check_suspicious_chars(files)

    print(f"문서 {len(files)}개 / {total_lines:,}줄")
    print(f"  깨진 링크: {len(broken)}")
    for b in broken:
        print(f"    ✗ {b}")
    print(f"  의심 문자: {len(suspicious)}")
    for s in suspicious[:20]:
        print(f"    ✗ {s}")

    return 1 if (broken or suspicious) else 0


if __name__ == "__main__":
    sys.exit(main())
