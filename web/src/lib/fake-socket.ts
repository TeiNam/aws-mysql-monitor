/**
 * 테스트용 WebSocket 대역. **프로덕션 코드가 import 하지 않는다.**
 *
 * 진짜 소켓을 열지 않고 순서만 본다. `live.test.ts` 와 `app.test.tsx` 가 함께
 * 쓰므로 파일로 뺐다.
 */

export class FakeSocket {
  static readonly OPEN = 1;
  static instances: FakeSocket[] = [];

  readyState = 0;
  readonly sent: string[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onclose?.();
  }

  // ── 테스트 조작 ────────────────────────────────────────────────────────
  accept(): void {
    this.readyState = FakeSocket.OPEN;
    this.onopen?.();
  }

  deliver(msg: unknown): void {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }

  /** 서버가 인증까지 통과시켰다. */
  makeReady(): void {
    this.accept();
    this.deliver({
      t: "ready",
      user: { subject: "u", role: "admin", env_scope: ["dev"], can_see_literals: false },
    });
  }

  parsedSent(): Array<Record<string, unknown>> {
    return this.sent.map((raw) => JSON.parse(raw) as Record<string, unknown>);
  }

  static latest(): FakeSocket {
    const socket = FakeSocket.instances.at(-1);
    if (socket === undefined) throw new Error("소켓이 만들어지지 않았다");
    return socket;
  }

  static install(): void {
    FakeSocket.instances = [];
    globalThis.WebSocket = FakeSocket as unknown as typeof WebSocket;
  }
}
