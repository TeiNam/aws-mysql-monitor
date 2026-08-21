/**
 * AWS 리전 코드 → 사람이 읽는 이름.
 *
 * # 왜 필요한가
 *
 * `ap-northeast-2` 와 `ap-northeast-1` 은 **한 글자 차이**다. 리전을 고르는 화면과
 * 머리말에서 그 둘을 눈으로 구분하는 것은 어렵고, 틀리면 엉뚱한 리전의 DB 를 보게 된다
 * (계정 착각과 같은 부류의 실수다).
 *
 * # 모르는 코드는 그대로 보여준다
 *
 * AWS 는 리전을 계속 추가한다. 목록에 없는 코드를 "알 수 없음" 으로 바꾸면 **실재하는
 * 리전이 화면에서 사라진 것처럼** 보인다. 이름을 모르면 코드만 보여주는 것이 맞다.
 */

/** 코드 → 한국어 이름. 도시명이 관례다(AWS 콘솔도 그렇게 적는다). */
const NAMES: Record<string, string> = {
  "us-east-1": "버지니아 북부",
  "us-east-2": "오하이오",
  "us-west-1": "캘리포니아 북부",
  "us-west-2": "오리건",
  "af-south-1": "케이프타운",
  "ap-east-1": "홍콩",
  "ap-south-1": "뭄바이",
  "ap-south-2": "하이데라바드",
  "ap-northeast-1": "도쿄",
  "ap-northeast-2": "서울",
  "ap-northeast-3": "오사카",
  "ap-southeast-1": "싱가포르",
  "ap-southeast-2": "시드니",
  "ap-southeast-3": "자카르타",
  "ap-southeast-4": "멜버른",
  "ap-southeast-5": "말레이시아",
  "ap-southeast-7": "태국",
  "ca-central-1": "캐나다 중부",
  "ca-west-1": "캘거리",
  "eu-central-1": "프랑크푸르트",
  "eu-central-2": "취리히",
  "eu-west-1": "아일랜드",
  "eu-west-2": "런던",
  "eu-west-3": "파리",
  "eu-north-1": "스톡홀름",
  "eu-south-1": "밀라노",
  "eu-south-2": "스페인",
  "il-central-1": "텔아비브",
  "me-south-1": "바레인",
  "me-central-1": "UAE",
  "mx-central-1": "멕시코 중부",
  "sa-east-1": "상파울루",
  // GovCloud·중국은 별 파티션이지만 코드가 보이면 이름도 보여야 한다.
  "us-gov-east-1": "GovCloud 동부",
  "us-gov-west-1": "GovCloud 서부",
  "cn-north-1": "베이징",
  "cn-northwest-1": "닝샤",
};

/** 이 코드의 이름. 모르면 `null` — 호출부가 코드만 보여준다. */
export function regionName(code: string): string | null {
  return NAMES[code] ?? null;
}

/**
 * 화면에 적는 형태: `ap-northeast-2 (서울)`.
 *
 * 이름을 모르면 코드만 남는다 — 괄호 안이 빈 채로 보이는 것보다 낫다.
 */
export function regionLabel(code: string): string {
  const name = regionName(code);
  return name === null ? code : `${code} (${name})`;
}
