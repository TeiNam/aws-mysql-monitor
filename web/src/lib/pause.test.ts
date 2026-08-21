import { describe, expect, it } from "vitest";
import {
  ALL_SCOPE,
  canControlScope,
  envScope,
  instanceScope,
  isInstancePaused,
  scopeLabel,
  scopeOptions,
  supportsScopedPause,
} from "./pause";
import type { CollectorStatus, Env, InstanceView } from "./types";

/**
 * 스코프 판정만 테스트한다.
 *
 * 두 가지가 조용히 틀릴 수 있는 자리다:
 *
 * 1. **키 문자열**이 서버(`crates/core/src/pause.rs`)와 어긋나면 정지가 아무 인스턴스에도
 *    걸리지 않는다 — 에러가 아니라 "눌렀는데 계속 수집된다" 로 나타난다.
 * 2. **권한 판정**이 서버보다 느슨하면 항상 403 을 받는 선택지를 제시하게 된다
 *    (`Filters.tsx` 규칙 1과 같은 실패).
 */
function inst(name: string, env: Env): InstanceView {
  return {
    id: `000000000000/ap-northeast-2/${name}`,
    name,
    env,
    env_from_tags: env,
    env_override: null,
    state: "collecting",
    engine: "mysql",
    engine_version: "8.4.6",
    endpoint: "127.0.0.1",
    port: 3306,
    instance_class: null,
    allocated_storage_gb: null,
    cluster_id: null,
    is_cluster_writer: false,
    iam_auth_enabled: false,
    vpc_id: null,
    availability_zone: null,
    collectible: true,
    tags: {},
    first_seen_ms: 0,
    last_seen_ms: 0,
    deleted_at_ms: null,
    cert_valid_till_ms: null,
  };
}

function status(over: Partial<CollectorStatus>): CollectorStatus {
  return {
    paused: false,
    paused_since_ms: null,
    paused_scopes: [],
    is_leader: true,
    collecting: 1,
    last_tick_ms: 0,
    last_discovery_ms: null,
    last_backfill_ms: null,
    discovery_requested: false,
    backfill_requested: false,
    worker_id: "all-local",
    scope: "deployment",
    runs_collector: true,
    can_control: false,
    controllable_envs: [],
    role: "operator",
    ...over,
  };
}

const ordersPrd = inst("orders-prd-01", "prd");
const ordersDev = inst("orders-dev-01", "dev");
const billingDev = inst("billing-dev-01", "dev");
const all = [ordersPrd, ordersDev, billingDev];

describe("정지 판정", () => {
  it("전체·환경·인스턴스가 정확히 그것만 멈춘다", () => {
    const paused = (scope: string) => [{ scope, since_ms: 1 }];

    for (const i of all) {
      expect(isInstancePaused(paused(ALL_SCOPE), i)).toBe(true);
    }
    expect(isInstancePaused(paused(envScope("dev")), ordersDev)).toBe(true);
    expect(isInstancePaused(paused(envScope("dev")), billingDev)).toBe(true);
    expect(isInstancePaused(paused(envScope("dev")), ordersPrd)).toBe(false);

    expect(isInstancePaused(paused(instanceScope(ordersDev.id)), ordersDev)).toBe(true);
    expect(isInstancePaused(paused(instanceScope(ordersDev.id)), billingDev)).toBe(false);

    expect(isInstancePaused([], ordersDev)).toBe(false);
  });

  /**
   * **적용 환경으로 본다.** 화면에서 `dev` 로 지정한 prd 태그 인스턴스는 `env:dev`
   * 정지에 걸려야 한다 — 서버 `PauseSet::is_paused` 도 `effective` 를 본다.
   */
  it("환경 오버라이드가 적용된 환경을 따른다", () => {
    const overridden: InstanceView = {
      ...inst("orders-x", "dev"),
      env_from_tags: "prd",
      env_override: "dev",
    };
    expect(isInstancePaused([{ scope: envScope("dev"), since_ms: 1 }], overridden)).toBe(true);
    expect(isInstancePaused([{ scope: envScope("prd"), since_ms: 1 }], overridden)).toBe(false);
  });
});

describe("구 버전 서버", () => {
  /**
   * **실측:** 스코프를 모르는 서버는 본문을 무시하고 전체를 멈춘다. 200 이 오고 화면도
   * "정지" 로 보이므로, 여기서 막지 않으면 "이 인스턴스만" 을 누른 사람이 전 환경
   * 관측을 멈추게 된다. 롤링 배포 중에는 그 조합이 반드시 생긴다.
   */
  it("스코프를 모르는 서버에서는 아무 스코프도 조작할 수 없다", () => {
    const old = status({
      scope: "process",
      can_control: true,
      controllable_envs: ["prd", "stg", "dev", "unknown"],
    });
    expect(supportsScopedPause(old)).toBe(false);
    expect(canControlScope(ALL_SCOPE, old, all)).toBe(false);
    expect(canControlScope(envScope("dev"), old, all)).toBe(false);
    expect(canControlScope(instanceScope(ordersDev.id), old, all)).toBe(false);
    expect(scopeOptions(["dev"], old).every((o) => o.disabled)).toBe(true);

    // 필드가 아예 없는 응답(구 백엔드)도 같다 — `withArrayDefaults` 가 빈 배열로 접는다.
    expect(supportsScopedPause(undefined)).toBe(false);
  });
});

describe("스코프 권한", () => {
  it("전체는 전 환경 스코프만, 환경·인스턴스는 그 환경만", () => {
    const devOnly = status({ can_control: false, controllable_envs: ["dev"] });
    expect(canControlScope(ALL_SCOPE, devOnly, all)).toBe(false);
    expect(canControlScope(envScope("dev"), devOnly, all)).toBe(true);
    expect(canControlScope(envScope("prd"), devOnly, all)).toBe(false);
    expect(canControlScope(instanceScope(ordersDev.id), devOnly, all)).toBe(true);
    expect(canControlScope(instanceScope(ordersPrd.id), devOnly, all)).toBe(false);

    const full = status({
      can_control: true,
      controllable_envs: ["prd", "stg", "dev", "unknown"],
    });
    expect(canControlScope(ALL_SCOPE, full, all)).toBe(true);
    expect(canControlScope(instanceScope(ordersPrd.id), full, all)).toBe(true);
  });

  /** 등록부에 없는 인스턴스는 **판단할 수 없다** — 서버는 404 를 준다. */
  it("모르는 인스턴스와 상태 미확인은 조작할 수 없다", () => {
    const full = status({ can_control: true, controllable_envs: ["prd", "dev"] });
    expect(canControlScope("id:000000000000/ap-northeast-2/ghost", full, all)).toBe(false);
    expect(canControlScope("nonsense", full, all)).toBe(false);
    expect(canControlScope(ALL_SCOPE, undefined, all)).toBe(false);
  });
});

describe("선택지", () => {
  /**
   * **인스턴스는 선택지에 없다** — 개별 정지는 표의 행 버튼이 한다. 500대 등록부에서
   * 셀렉터가 500줄이 되는 것을 막고, 판단 근거(환경·상태·마지막 관측)가 있는 자리에서
   * 누르게 한다.
   */
  it("전체와 환경만 담고, 못 누를 것은 비활성이다", () => {
    const devOnly = status({ controllable_envs: ["dev"] });
    const options = scopeOptions(["prd", "dev"], devOnly);

    expect(options.map((o) => o.key)).toEqual([ALL_SCOPE, envScope("prd"), envScope("dev")]);
    // **숨기지 않고 비활성화한다** — 왜 못 누르는지 알려야 한다.
    expect(options.filter((o) => !o.disabled).map((o) => o.key)).toEqual([envScope("dev")]);
    // 전 환경 스코프면 전체도 열린다.
    const full = status({ can_control: true, controllable_envs: ["prd", "dev"] });
    expect(scopeOptions(["prd"], full).every((o) => !o.disabled)).toBe(true);
  });
});

describe("칩 이름", () => {
  it("인스턴스는 등록부 이름으로, 없으면 식별자만 남긴다", () => {
    expect(scopeLabel(ALL_SCOPE, all)).toBe("전체");
    expect(scopeLabel(envScope("prd"), all)).toBe("prd");
    expect(scopeLabel(instanceScope(ordersDev.id), all)).toBe("orders-dev-01");
    // 삭제됐거나 스코프 밖 — 계정·리전까지 그대로 띄우면 칩이 줄을 넘긴다.
    expect(scopeLabel("id:000000000000/ap-northeast-2/ghost", all)).toBe("ghost");
  });
});
