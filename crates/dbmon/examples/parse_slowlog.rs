//! 실제 슬로우 로그 파일을 파싱해 통계를 낸다. **픽스처가 아닌 실물 검증용.**
//!
//! `cargo run --example parse_slowlog -- <파일> [최소_ms]`

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("사용법: parse_slowlog <파일> [최소_ms]");
    let min_ms: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1_000);

    let text = std::fs::read_to_string(&path).expect("파일 읽기");
    let out = dbmon::slowlog::parse(&text, min_ms);

    println!(
        "엔트리 {}건, 건너뜀 {}건",
        out.entries.len(),
        out.skipped.len()
    );
    for (reason, count) in out.skip_counts() {
        println!("  건너뜀 {reason}: {count}");
    }
    for e in out.entries.iter().take(5) {
        println!(
            "  thread={} dur={}ms rows_examined={:?} start={} sql={:.60}",
            e.thread_id, e.duration_ms, e.rows_examined, e.started_at_ms, e.sql_text
        );
    }
}
