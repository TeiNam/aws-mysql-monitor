import { describe, expect, it } from "vitest";
import { instancesInEnv, instancesMatching, visibleEnvs, visibleInstances } from "./Filters";
import type { InstanceView } from "../lib/types";

/**
 * 필터 판정만 테스트한다 — 여기서 두 번 틀렸다.
 *
 * 1. 인스턴스 드롭다운이 env 만 좁혀서 `orders` 를 치고도 `billing-prd-01` 이 남았다.
 * 2. "몇 대 걸렸나" 가 env 를 무시해서 `env=prd` + `orders` 에 **3대**라고 적었다
 *    (`orders-stg-01` 까지 세어서 — 그 인스턴스는 이 조회에 들어오지 않는다).
 *
 * 둘 다 "두 필터가 조합되지 않는다" 는 같은 원인이었고, 브라우저에서 숫자를 재보고서야
 * 드러났다. 그래서 조합을 순수 함수로 두고 여기서 고정한다.
 */
function inst(name: string, env: string): InstanceView {
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

const FLEET = [
  inst("orders-prd-01", "prd"),
  inst("orders-prd-02", "prd"),
  inst("billing-prd-01", "prd"),
  inst("orders-stg-01", "stg"),
  inst("mysql84-local", "dev"),
];

describe("환경 선택지", () => {
  it("보이는 인스턴스에서만 만들고 운영을 먼저 놓는다", () => {
    expect(visibleEnvs(FLEET)).toEqual(["prd", "stg", "dev"]);
    // **스코프 밖 환경을 제시하지 않는다** — 고르면 서버가 403 이므로 항상 실패하는 선택지다.
    expect(visibleEnvs([inst("a", "dev")])).toEqual(["dev"]);
    expect(visibleEnvs([])).toEqual([]);
  });

  it("규정에 없는 값도 숨기지 않는다 — 숨기면 그 인스턴스를 고를 방법이 없다", () => {
    expect(visibleEnvs([inst("a", "sandbox"), inst("b", "prd")])).toEqual(["prd", "sandbox"]);
  });
});

describe("이름 조각", () => {
  it("이름 어디에 있어도 걸리고 대소문자를 무시한다", () => {
    expect(instancesMatching(FLEET, "orders").map((i) => i.name)).toEqual([
      "orders-prd-01",
      "orders-prd-02",
      "orders-stg-01",
    ]);
    expect(instancesMatching(FLEET, "ORDERS")).toHaveLength(3);
    expect(instancesMatching(FLEET, "prd-01").map((i) => i.name)).toEqual([
      "orders-prd-01",
      "billing-prd-01",
    ]);
    // 빈 조각은 필터가 아니다.
    expect(instancesMatching(FLEET, "  ")).toHaveLength(FLEET.length);
  });

  it("계정·리전 조각에는 걸리지 않는다 — 걸리면 필터가 아무 일도 안 한다", () => {
    // `id` 는 `계정/리전/이름` 이지만 판정은 **이름만** 본다(서버 `name_matches` 와 같다).
    expect(instancesMatching(FLEET, "ap-northeast-2")).toHaveLength(0);
    expect(instancesMatching(FLEET, "000000000000")).toHaveLength(0);
  });
});

describe("두 필터의 조합", () => {
  it("환경과 이름을 **함께** 적용한다", () => {
    expect(visibleInstances(FLEET, "prd", "orders").map((i) => i.name)).toEqual([
      "orders-prd-01",
      "orders-prd-02",
    ]);
    // env 만 적용하면 `billing-prd-01` 이 남는다(결함 1).
    expect(instancesInEnv(FLEET, "prd")).toHaveLength(3);
    // 이름만 적용하면 `orders-stg-01` 이 섞인다(결함 2 — 개수 표시가 3대였다).
    expect(instancesMatching(FLEET, "orders")).toHaveLength(3);
    expect(visibleInstances(FLEET, "prd", "orders")).toHaveLength(2);
  });

  it("어느 쪽이 비어도 나머지만 적용한다", () => {
    expect(visibleInstances(FLEET, "", "orders")).toHaveLength(3);
    expect(visibleInstances(FLEET, "stg", "")).toHaveLength(1);
    expect(visibleInstances(FLEET, "", "")).toHaveLength(FLEET.length);
  });

  it("걸리는 인스턴스가 없으면 빈 집합이다 — 화면은 그 수를 그대로 보여준다", () => {
    expect(visibleInstances(FLEET, "dev", "orders")).toHaveLength(0);
  });
});
