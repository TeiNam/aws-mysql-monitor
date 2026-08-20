/**
 * WebSocket **배관**. 판정은 [`./live-reduce`] 에 있다.
 *
 * # 연결은 하나다
 *
 * 화면마다 소켓을 열면 (a) 서버의 재인증·구독 상한이 화면 수만큼 늘어나고
 * (b) 같은 방송을 N배로 받는다. 그래서 모듈 싱글턴 하나를 공유하고, 화면은
 * "이 토픽이 필요하다" 만 말한다([`LiveClient.retain`]).
 *
 * # 순서를 지킨다
 *
 * 서버는 **첫 메시지가 `auth` 가 아니면 즉시 끊는다**(`ws.rs`). 그래서 `open`
 * 직후 `auth` 를 보내고, 구독은 `ready` 를 받은 뒤에 보낸다.
 *
 * # `unauthorized` 면 재접속하지 않는다
 *
 * 같은 토큰으로 무한 재접속하면 서버 로그만 더럽히고 화면은 "연결 중" 에서
 * 멈춘 것처럼 보인다. 사용자가 새 토큰을 들고 오면 [`LiveClient.reconnectNow`]
 * 로 다시 시작한다.
 */

import { currentToken } from "./auth";
import {
  INITIAL_SNAPSHOT,
  applyMessage,
  parseServerMessage,
  type LiveSnapshot,
} from "./live-reduce";

/** 하트비트 주기. 서버의 무응답 종료(60초)의 절반으로 둔다. */
const PING_INTERVAL_MS = 30_000;
const RECONNECT_BASE_MS = 500;
const RECONNECT_MAX_MS = 15_000;

export class LiveClient {
  private snapshot: LiveSnapshot = INITIAL_SNAPSHOT;
  private readonly listeners = new Set<() => void>();
  private socket: WebSocket | null = null;
  /** 이번 연결이 `ready` 를 받았는가. 구독은 그 뒤에만 보낸다. */
  private authed = false;
  /** 토픽 → 요구한 화면 수. 0 이 되면 구독을 해제한다. */
  private readonly wanted = new Map<string, number>();
  /** 이번 연결에서 이미 보낸 구독. 거부된 토픽을 반복 전송하지 않기 위해 남긴다. */
  private readonly sent = new Set<string>();
  private attempt = 0;
  private pingTimer: number | null = null;
  private reconnectTimer: number | null = null;

  constructor(
    private readonly url: string,
    private readonly now: () => number = Date.now,
  ) {}

  /** `useSyncExternalStore` 용. **참조가 안정적이어야** 하므로 화살표 필드다. */
  readonly getSnapshot = (): LiveSnapshot => this.snapshot;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    // 첫 관찰자가 붙을 때 연결한다. **화면이 없을 때 소켓을 들고 있지 않는다.**
    this.connect();
    return () => {
      this.listeners.delete(listener);
      // 여기서 소켓을 닫지는 않는다 — 라우트 전환마다 재연결하면 5초 인증
      // 왕복과 구독을 매번 다시 하게 된다. SPA 수명 동안 하나를 유지한다.
    };
  };

  /**
   * 토픽을 요구한다. 반환된 함수를 부르면 요구를 취소한다.
   *
   * 같은 토픽을 두 화면이 요구할 수 있으므로 **참조 계수**를 센다. 계수 없이
   * 해제하면 한 화면이 언마운트될 때 다른 화면의 스트림이 끊긴다.
   */
  retain(topics: readonly string[]): () => void {
    for (const topic of topics) {
      this.wanted.set(topic, (this.wanted.get(topic) ?? 0) + 1);
    }
    this.syncTopics();

    return () => {
      for (const topic of topics) {
        const left = (this.wanted.get(topic) ?? 1) - 1;
        if (left <= 0) this.wanted.delete(topic);
        else this.wanted.set(topic, left);
      }
      this.syncTopics();
    };
  }

  /** 사용자가 "다시 연결" 을 눌렀다. 새 토큰을 들고 왔을 수 있으므로 상태를 푼다. */
  reconnectNow(): void {
    this.clearTimer("reconnectTimer");
    this.attempt = 0;
    if (this.snapshot.conn === "unauthorized") {
      this.set({ ...this.snapshot, conn: "idle", errorCode: null });
    }
    this.connect();
  }

  private connect(): void {
    if (this.socket !== null || this.snapshot.conn === "unauthorized") return;

    this.set({ ...this.snapshot, conn: "connecting" });
    const socket = new WebSocket(this.url);
    this.socket = socket;
    this.authed = false;
    this.sent.clear();

    socket.onopen = () => {
      this.attempt = 0;
      const token = currentToken();
      // 로컬 우회 모드에는 토큰이 없다. 서버가 `{"t":"auth"}` 를 허용한다.
      socket.send(JSON.stringify(token === null ? { t: "auth" } : { t: "auth", token }));
      this.startPing();
    };

    socket.onmessage = (event: MessageEvent<unknown>) => {
      // 이진 프레임은 프로토콜에 없다. 문자열만 읽는다.
      if (typeof event.data !== "string") return;
      const msg = parseServerMessage(event.data);
      if (msg === null) return;
      this.set(applyMessage(this.snapshot, msg, this.now()));
      if (msg.t === "ready") {
        this.authed = true;
        this.syncTopics();
      }
    };

    socket.onclose = () => {
      this.teardown();
      this.scheduleReconnect();
    };

    // `onerror` 뒤에는 항상 `onclose` 가 온다. 여기서 처리하면 두 번 세게 된다.
    socket.onerror = () => {};
  }

  private teardown(): void {
    this.clearTimer("pingTimer");
    this.socket = null;
    this.authed = false;
    this.sent.clear();
    // `unauthorized` 를 `closed` 로 덮으면 재접속 금지가 풀린다.
    if (this.snapshot.conn !== "unauthorized") {
      this.set({ ...this.snapshot, conn: "closed", user: null });
    }
  }

  private scheduleReconnect(): void {
    if (this.snapshot.conn === "unauthorized" || this.reconnectTimer !== null) return;
    if (this.listeners.size === 0) return;

    const backoff = Math.min(RECONNECT_MAX_MS, RECONNECT_BASE_MS * 2 ** this.attempt);
    this.attempt += 1;
    // 지터. 여러 탭이 동시에 끊겼을 때 같은 순간에 몰려 들어가지 않게 한다.
    const delay = backoff * (0.5 + Math.random() * 0.5);
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  private startPing(): void {
    this.clearTimer("pingTimer");
    this.pingTimer = window.setInterval(() => {
      if (this.socket?.readyState === WebSocket.OPEN) {
        this.socket.send(JSON.stringify({ t: "ping" }));
      }
    }, PING_INTERVAL_MS);
  }

  private clearTimer(which: "pingTimer" | "reconnectTimer"): void {
    const id = this[which];
    if (id === null) return;
    if (which === "pingTimer") window.clearInterval(id);
    else window.clearTimeout(id);
    this[which] = null;
  }

  /** 요구 집합과 실제 전송 집합의 차이만 보낸다. */
  private syncTopics(): void {
    const socket = this.socket;
    if (!this.authed || socket === null || socket.readyState !== WebSocket.OPEN) return;

    const add = [...this.wanted.keys()].filter((t) => !this.sent.has(t));
    const drop = [...this.sent].filter((t) => !this.wanted.has(t));

    if (add.length > 0) {
      socket.send(JSON.stringify({ t: "subscribe", topics: add }));
      for (const topic of add) this.sent.add(topic);
    }
    if (drop.length > 0) {
      socket.send(JSON.stringify({ t: "unsubscribe", topics: drop }));
      for (const topic of drop) this.sent.delete(topic);
    }
  }

  private set(next: LiveSnapshot): void {
    if (next === this.snapshot) return;
    this.snapshot = next;
    for (const listener of this.listeners) listener();
  }
}

/** 같은 오리진의 `/api/ws`. 개발 서버는 Vite 프록시가 넘긴다. */
function sameOriginWsUrl(): string {
  const url = new URL("/api/ws", window.location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}

/** 앱 전체가 공유하는 하나뿐인 연결. */
export const liveClient = new LiveClient(sameOriginWsUrl());
