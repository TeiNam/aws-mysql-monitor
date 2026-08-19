//! `dbmon` — 단일 바이너리의 내부 모듈.
//!
//! 라이브러리 타깃을 두는 이유는 **통합 테스트가 내부 모듈을 쓸 수 있어야** 하기 때문이다.
//! `main.rs` 는 조립(wiring)만 한다 ([02 §3](../../../docs/02-architecture.md)).
//!
//! 도메인 로직은 `dbmon-core` · `dbmon-normalize` · `dbmon-planparse` 에 있고,
//! 이 크레이트는 **어댑터와 루프**만 담는다.

pub mod collector;
pub mod config;
pub mod health;
pub mod mysql;
pub mod shutdown;
pub mod telemetry;

pub use config::{Config, Role};
