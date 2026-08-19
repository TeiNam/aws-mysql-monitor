//! 슬로우 로그 백필 (M2-7).
//!
//! # 왜 이것이 필수인가
//!
//! 실측 결과 실행 중 문장의 행 카운터는 **0** 이고, 완료 후에도 실행별로 남는 곳이
//! 없다([19 §G2](../../../../docs/19-m1-findings.md)). 즉 `rows_examined` 같은
//! 정확 지표의 **유일한 출처가 슬로우 로그**다.
//!
//! # 순수 파싱과 I/O 를 나눈다
//!
//! [`parse`] 는 텍스트만 본다 — 파일도 AWS 도 모른다. 그래서 실측한 로그 조각으로
//! 전수 검증할 수 있다. 형식 파싱은 이 프로젝트에서 반복해서 구멍이 났던 부류라
//! (마스킹 5회, 축약형 4회) 순수 함수로 두는 것이 특히 중요하다.

pub mod fetch;
pub mod parse;
pub mod source;

pub use fetch::{CloudWatchFetcher, FileFetcher, LogChunk, SlowLogFetcher, slowquery_log_group};
pub use parse::{ParseOutcome, SkipReason, SlowLogEntry, parse};
