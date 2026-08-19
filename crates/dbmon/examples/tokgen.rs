//! IAM DB Auth 토큰을 한 개 출력한다. **교차 검증 전용.**
//!
//! `scripts/verify-iam-token.sh` 가 이걸 `aws rds generate-db-auth-token` 과 나란히
//! 돌려 **토큰 문자열 전체**가 같은지 확인한다. 서명 16진수만 비교하면
//! 퍼센트 인코딩 차이를 놓친다 — 실제로 그렇게 놓쳤다.
//!
//! 자격증명은 환경변수에서 읽는다. 세션 토큰이 있으면 함께 서명한다 —
//! ECS 태스크 롤은 항상 임시 자격증명이고 base64 세션 토큰에는 `+`·`/`·`=` 가 있다.

fn main() {
    let key = std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID");
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY");
    let session = std::env::var("AWS_SESSION_TOKEN").ok();
    let creds =
        aws_credential_types::Credentials::new(key, secret, session, None, "verify-iam-token");
    let token = dbmon::aws::auth_token::presign(
        &creds,
        &std::env::var("AWS_REGION").unwrap_or_else(|_| "ap-northeast-2".into()),
        "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
        3306,
        "dbmon",
        std::time::SystemTime::now(),
    )
    .expect("서명");
    println!("{token}");
}
