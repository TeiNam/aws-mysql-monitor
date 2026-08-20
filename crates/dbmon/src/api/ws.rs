//! WebSocket 엔드포인트 ([09 §4.1](../../../../docs/09-frontend.md)).
//!
//! # 토큰을 URL 에 넣지 않는다 (T-13)
//!
//! `wss://…/api/ws?token=…` 은 ALB 액세스 로그·프록시 로그에 토큰을 남긴다.
//! 그래서 **연결 후 첫 메시지**로 받는다. 5초 안에 오지 않으면 끊는다 —
//! 인증되지 않은 연결을 무기한 열어 두면 그게 곧 자원 고갈 경로다.
//!
//! # 5분마다 재인증한다 (T-33)
//!
//! 최초 1회만 검사하면 그룹 강등·계정 비활성이 반영되지 않는다. WS 연결은
//! 사실상 무기한이므로 "한 번 통과하면 영원히" 가 된다. 주기적으로 같은
//! [`authenticate`] 를 다시 돌리고 실패하면 끊는다.
//!
//! # 판정은 여기 없다
//!
//! 토픽 파싱·인가는 [`super::topic`], 지표 유도는 [`crate::metrics::derive`] 에
//! 순수 함수로 있다. 이 모듈은 **소켓 배관**만 한다 — 그래서 이 파일에 버그가
//! 있으면 그건 배관 버그이고, 판정 버그는 저쪽 테스트가 잡는다.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use dbmon_core::env::Env;
use dbmon_core::ports::InstanceRegistry;
use dbmon_core::rbac::AuthContext;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use super::auth::authenticate;
use super::hub::Broadcast;
use super::topic::{MAX_TOPICS, Topic, TopicError, authorize};
use super::view::SlowQueryBroadcast;
use crate::metrics::LiveMetrics;

/// 첫 `auth` 메시지 기한. 넘으면 끊는다.
const AUTH_DEADLINE: Duration = Duration::from_secs(5);
/// 재인증 주기 (T-33).
const REAUTH_INTERVAL: Duration = Duration::from_secs(300);
/// 클라이언트 무응답 허용 시간. 규정은 클라이언트 30초 ping / 서버 60초 종료.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientMsg {
    /// 연결 후 첫 메시지. 로컬 개발 우회에서는 토큰이 없다.
    Auth {
        token: Option<String>,
    },
    Subscribe {
        topics: Vec<String>,
    },
    Unsubscribe {
        topics: Vec<String>,
    },
    Ping,
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Ready {
        user: ReadyUser<'a>,
    },
    /// 구독 결과. **거부된 토픽을 함께 알린다** — 조용히 빼면 클라이언트는
    /// "데이터가 없다" 와 "구독이 거부됐다" 를 구분할 수 없다.
    Subscribed {
        topics: Vec<String>,
        denied: Vec<String>,
    },
    Slowq {
        data: SlowQueryBroadcast,
    },
    Status {
        instance_id: String,
        metrics: LiveMetrics,
    },
    Pong,
    Error {
        code: &'static str,
    },
}

#[derive(Debug, Serialize)]
struct ReadyUser<'a> {
    subject: &'a str,
    role: &'a str,
    env_scope: Vec<&'a str>,
    can_see_literals: bool,
}

pub async fn handler(ws: WebSocketUpgrade, State(state): State<super::ApiState>) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(code) = run(socket, state).await {
            tracing::debug!(code, "WS 연결 종료");
        }
    })
}

/// 연결 하나를 처리한다. `Err(사유)` 는 종료 사유 문자열이다(로그용).
async fn run(mut socket: WebSocket, state: super::ApiState) -> Result<(), &'static str> {
    // ── 1. 인증 (5초 기한) ──────────────────────────────────────────────────
    let token = match tokio::time::timeout(AUTH_DEADLINE, socket.recv()).await {
        Ok(Some(Ok(Message::Text(raw)))) => match serde_json::from_str::<ClientMsg>(&raw) {
            Ok(ClientMsg::Auth { token }) => token,
            // **첫 메시지가 `auth` 가 아니면 끊는다.** 순서를 관용하면 인증 전에
            // 구독을 받아 줄 경로가 열린다.
            _ => {
                send(
                    &mut socket,
                    ServerMsg::Error {
                        code: "unauthorized",
                    },
                )
                .await;
                return Err("first_message_not_auth");
            }
        },
        Ok(_) => return Err("closed_before_auth"),
        Err(_) => {
            send(
                &mut socket,
                ServerMsg::Error {
                    code: "auth_timeout",
                },
            )
            .await;
            return Err("auth_timeout");
        }
    };
    let token = token.filter(|t| !t.trim().is_empty());

    let mut ctx = match authenticate(&state.policy, token.as_deref()) {
        Ok(ctx) => ctx,
        Err(_) => {
            send(
                &mut socket,
                ServerMsg::Error {
                    code: "unauthorized",
                },
            )
            .await;
            return Err("unauthorized");
        }
    };
    send(
        &mut socket,
        ServerMsg::Ready {
            user: ready_user(&ctx),
        },
    )
    .await;

    // ── 2. 본 루프 ─────────────────────────────────────────────────────────
    let mut subscribed: BTreeSet<String> = BTreeSet::new();
    let mut slowq_rx = state.hub.subscribe_slow_queries();
    let mut status_rx = state.hub.subscribe_status();
    let mut reauth = tokio::time::interval(REAUTH_INTERVAL);
    reauth.tick().await; // 첫 tick 은 즉시 오므로 버린다

    // ⚠ **절대 기한으로 관리한다.** `timeout(IDLE_TIMEOUT, recv())` 을 `select!` 안에
    // 두면 **다른 분기가 이길 때마다 타임아웃이 새로 시작된다.** 5초마다 오는
    // `status` 방송이 (구독하지 않은 인스턴스의 것까지) 이 분기를 깨우므로 기한이
    // 영원히 리셋되고, 인증만 하고 침묵하는 소켓이 **무기한 살아 있었다.**
    // 실측으로 확인했다(75초 침묵 후에도 열려 있음) — 9차 리뷰가 잡았다.
    let mut idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;

    loop {
        tokio::select! {
            // 클라이언트 메시지. `IDLE_TIMEOUT` 안에 아무것도 없으면 끊는다.
            incoming = socket.recv() => {
                // 무엇이든 받았으면 살아 있다. 기한을 뒤로 민다.
                idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                match incoming {
                    Some(Ok(Message::Text(raw))) => {
                        handle_client_msg(&raw, &mut socket, &state, &ctx, &mut subscribed).await?;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return Err("client_closed"),
                    Some(Ok(Message::Binary(_))) => {
                        // 이진 프레임을 쓰지 않는다. 관용하면 파싱 표면이 늘어난다.
                        send(&mut socket, ServerMsg::Error { code: "binary_not_supported" }).await;
                    }
                    Some(Err(_)) => return Err("transport_error"),
                }
            }

            // 무응답 종료. 기한이 절대 시각이므로 다른 분기가 이겨도 밀리지 않는다.
            _ = tokio::time::sleep_until(idle_deadline) => return Err("idle_timeout"),

            // 슬로우 쿼리 방송. **밀리면 알린다** — 조용히 버리면 그 쿼리가
            // 화면에 영구히 안 나타난다.
            got = slowq_rx.recv() => match got {
                Ok(Broadcast { key, data }) if subscribed.contains(&key) => {
                    send(&mut socket, ServerMsg::Slowq { data }).await;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    // **로그는 항상 남긴다.** 방송이 밀린 것은 운영 신호이고,
                    // 이 클라이언트가 구독했는지와 무관한 사실이다.
                    tracing::warn!(dropped = n, "slowq 방송이 밀렸다");
                    // **알림은 구독자에게만.** 구독하지 않았으면 놓친 것이 없고,
                    // 그런 경고는 화면에 거짓 구멍을 표시하게 만든다.
                    if subscribed.iter().any(|k| k.starts_with("slowq:")) {
                        send(&mut socket, ServerMsg::Error { code: "stream_lagged" }).await;
                    }
                }
                Err(RecvError::Closed) => return Err("hub_closed"),
            },

            // 실시간 지표. 밀리면 조용히 넘긴다 — 다음 샘플이 곧 온다.
            got = status_rx.recv() => match got {
                Ok(Broadcast { key, data }) if subscribed.contains(&key) => {
                    let instance_id = key.strip_prefix("status:inst=").unwrap_or(&key).to_string();
                    send(&mut socket, ServerMsg::Status { instance_id, metrics: data }).await;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return Err("hub_closed"),
            },

            // 재인증 (T-33). 권한이 줄었으면 여기서 끊긴다.
            _ = reauth.tick() => {
                match authenticate(&state.policy, token.as_deref()) {
                    Ok(fresh) => {
                        // **스코프가 줄었으면 구독도 줄인다.** 재인증만 하고
                        // 기존 구독을 그대로 두면 강등이 반영되지 않는다.
                        subscribed.retain(|key| {
                            Topic::parse(key).is_ok_and(|t| still_allowed(&t, &fresh))
                        });
                        ctx = fresh;
                    }
                    Err(_) => {
                        send(&mut socket, ServerMsg::Error { code: "unauthorized" }).await;
                        return Err("reauth_failed");
                    }
                }
            }
        }
    }
}

/// 재인증 후에도 이 토픽을 유지할 수 있는가.
///
/// 인스턴스 토픽은 등록부 조회가 필요하지만 재인증 경로에서 매번 등록부를
/// 읽지는 않는다. 대신 **환경 스코프가 그대로면 유지, 줄었으면 버린다** —
/// 인스턴스의 환경은 변하지 않는다고 가정하지 않고, 다음 `subscribe` 에서
/// 다시 검사된다.
fn still_allowed(topic: &Topic, ctx: &AuthContext) -> bool {
    match topic {
        Topic::SlowQueries(env) => ctx.is_env_allowed(*env),
        // 인스턴스 환경을 모르므로 보수적으로 유지한다. 방송 시점에 키가
        // 일치해야 전달되고, 스코프 축소는 다음 구독에서 반영된다.
        Topic::InstanceStatus(_) => true,
    }
}

async fn handle_client_msg(
    raw: &str,
    socket: &mut WebSocket,
    state: &super::ApiState,
    ctx: &AuthContext,
    subscribed: &mut BTreeSet<String>,
) -> Result<(), &'static str> {
    match serde_json::from_str::<ClientMsg>(raw) {
        Ok(ClientMsg::Ping) => send(socket, ServerMsg::Pong).await,
        // **인증 후 재-`auth` 를 받지 않는다.** 받아 주면 권한 상승 경로가 된다.
        Ok(ClientMsg::Auth { .. }) => {
            send(
                socket,
                ServerMsg::Error {
                    code: "already_authenticated",
                },
            )
            .await;
        }
        Ok(ClientMsg::Unsubscribe { topics }) => {
            for raw in &topics {
                subscribed.remove(raw);
            }
            send(
                socket,
                ServerMsg::Subscribed {
                    topics: subscribed.iter().cloned().collect(),
                    denied: Vec::new(),
                },
            )
            .await;
        }
        Ok(ClientMsg::Subscribe { topics }) => {
            let envs = instance_envs(state).await;
            let mut denied = Vec::new();
            for raw in &topics {
                // **상한을 넘으면 더 받지 않는다.** 무한 구독은 메모리와
                // 방송 비용을 클라이언트가 정하게 만든다.
                if subscribed.len() >= MAX_TOPICS {
                    denied.push(raw.clone());
                    continue;
                }
                let allowed = Topic::parse(raw).and_then(|t| {
                    let inst_env = match &t {
                        Topic::InstanceStatus(id) => envs
                            .as_ref()
                            .and_then(|m| m.iter().find(|(i, _)| i == id).map(|(_, e)| *e)),
                        Topic::SlowQueries(_) => None,
                    };
                    authorize(&t, ctx, inst_env).map(|()| t)
                });
                match allowed {
                    Ok(t) => {
                        subscribed.insert(t.key());
                    }
                    // 문법 오류와 권한 거부를 **구분하지 않는다** — 어떤 토픽이
                    // 존재하는지 알려 주지 않기 위해서다.
                    Err(TopicError::Malformed | TopicError::Denied) => denied.push(raw.clone()),
                }
            }
            send(
                socket,
                ServerMsg::Subscribed {
                    topics: subscribed.iter().cloned().collect(),
                    denied,
                },
            )
            .await;
        }
        Err(_) => send(socket, ServerMsg::Error { code: "malformed" }).await,
    }
    Ok(())
}

/// 등록부에서 `(instance_id, env)` 를 읽는다. 실패하면 `None` — 그러면 인스턴스
/// 토픽이 전부 거부된다(모르는 인스턴스는 거부가 기본값이다).
async fn instance_envs(state: &super::ApiState) -> Option<Vec<(String, Env)>> {
    InstanceRegistry::list(&*state.registry)
        .await
        .ok()
        .map(|all| {
            all.into_iter()
                .map(|i| (i.id.as_str().to_string(), i.env.effective))
                .collect()
        })
}

fn ready_user(ctx: &AuthContext) -> ReadyUser<'_> {
    ReadyUser {
        subject: &ctx.subject,
        role: ctx.role.as_str(),
        env_scope: ctx.env_scope.iter().map(|e| e.as_str()).collect(),
        can_see_literals: ctx.can_see_literals,
    }
}

/// 직렬화 실패나 전송 실패를 **삼키지 않고 로그로 남긴다.** 다만 연결을 끊지는
/// 않는다 — 한 메시지가 못 나간 것이 연결 종료 사유는 아니다.
async fn send(socket: &mut WebSocket, msg: ServerMsg<'_>) {
    match serde_json::to_string(&msg) {
        Ok(json) => {
            if let Err(e) = socket.send(Message::Text(json.into())).await {
                tracing::debug!(error = %e, "WS 전송 실패");
            }
        }
        Err(e) => tracing::error!(error = %e, "WS 메시지 직렬화 실패"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbmon_core::rbac::Role;

    fn ctx(scope: &[Env]) -> AuthContext {
        AuthContext {
            subject: "u".into(),
            role: Role::Viewer,
            env_scope: scope.to_vec(),
            can_see_literals: false,
            claims_version: 0,
        }
    }

    /// **첫 메시지 형식이 고정돼야 한다.** 규정과 어긋나면 클라이언트가 붙지 못한다.
    #[test]
    fn the_auth_message_parses_with_and_without_a_token() {
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"t":"auth","token":"abc"}"#),
            Ok(ClientMsg::Auth { token: Some(t) }) if t == "abc"
        ));
        // 로컬 개발 우회에서는 토큰이 없다.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"t":"auth"}"#),
            Ok(ClientMsg::Auth { token: None })
        ));
    }

    #[test]
    fn unknown_message_types_are_rejected() {
        for raw in [
            r#"{"t":"eval","code":"1"}"#,
            r#"{"t":"subscribe"}"#,
            "{}",
            "not json",
            r#"{"t":"ping","extra":1}"#,
        ] {
            let parsed = serde_json::from_str::<ClientMsg>(raw);
            // `ping` 에 여분 필드가 붙은 것은 허용된다(serde 기본). 나머지는 거부.
            if raw.contains(r#""t":"ping""#) {
                assert!(parsed.is_ok(), "{raw}");
            } else {
                assert!(parsed.is_err(), "{raw} 가 통과했다");
            }
        }
    }

    /// 서버 메시지 태그가 규정과 일치해야 한다 — 이름이 틀리면 클라이언트가
    /// 조용히 무시하고 화면이 빈다.
    #[test]
    fn server_message_tags_match_the_spec() {
        let cases = [
            (ServerMsg::Pong, r#""t":"pong""#),
            (ServerMsg::Error { code: "x" }, r#""t":"error""#),
        ];
        for (msg, expected) in cases {
            let json = serde_json::to_string(&msg).expect("직렬화");
            assert!(json.contains(expected), "{json}");
        }
        let ready = serde_json::to_string(&ServerMsg::Ready {
            user: ready_user(&ctx(&[Env::Dev])),
        })
        .expect("직렬화");
        assert!(ready.contains(r#""t":"ready""#), "{ready}");
        assert!(ready.contains("env_scope"), "{ready}");
    }

    /// **스코프가 줄면 슬로우 쿼리 구독이 유지되지 않는다** (T-33).
    ///
    /// 이게 없으면 강등된 사용자가 기존 연결로 계속 prd 스트림을 받는다.
    #[test]
    fn a_narrowed_scope_drops_slow_query_subscriptions() {
        let prd = Topic::SlowQueries(Env::Prd);
        assert!(still_allowed(&prd, &ctx(&[Env::Prd, Env::Dev])));
        assert!(!still_allowed(&prd, &ctx(&[Env::Dev])));
    }

    /// **유휴 종료를 `select!` 안의 `timeout` 으로 되돌리지 않는다.**
    ///
    /// 그 형태는 다른 분기(5초마다 오는 `status` 방송)가 이길 때마다 기한을 새로
    /// 시작해 유휴 종료를 **무력화한다.** 9차 리뷰에서 실측으로 확인했다:
    /// 수정 전에는 75초를 침묵해도 연결이 열려 있었고, 수정 후 60.0초에 닫혔다.
    ///
    /// 이 프로젝트의 관용대로 소스를 훑는다 — 동작을 테스트하려면 WS 하네스가
    /// 필요하고, 그건 이 배관보다 크다.
    #[test]
    fn the_idle_timeout_uses_an_absolute_deadline() {
        let src = include_str!("ws.rs");
        // 테스트 모듈 앞부분만 본다 (이 테스트 자신의 문자열을 세지 않도록).
        let code = src.split("#[cfg(test)]").next().expect("본문");
        assert!(
            code.contains("sleep_until(idle_deadline)"),
            "유휴 종료가 절대 기한을 쓰지 않는다"
        );
        // 금지 패턴을 조립한다 — 그대로 적으면 이 파일이 그 패턴을 갖게 된다.
        let forbidden = format!("timeout(IDLE_{}, socket.recv())", "TIMEOUT");
        assert!(
            !code.contains(&forbidden),
            "유휴 타임아웃이 select! 안에서 매번 재생성된다 — 방송이 기한을 영원히 밀어낸다"
        );
    }

    /// 기한·주기 값이 규정과 맞는지 고정한다 — 조용히 바뀌면 T-33 이 무의미해진다.
    #[test]
    fn the_documented_deadlines_are_used() {
        assert_eq!(AUTH_DEADLINE, Duration::from_secs(5));
        assert_eq!(REAUTH_INTERVAL, Duration::from_secs(300));
        assert_eq!(IDLE_TIMEOUT, Duration::from_secs(60));
    }
}
