//! IAM DB Auth 토큰을 한 개 출력한다. **교차 검증 전용.**
//!
//! `scripts/verify-iam-token.sh` 가 이걸 `aws rds generate-db-auth-token` 과 나란히
//! 돌려 서명이 같은지 확인한다. 우리 구현이 문서대로 생겼는지(단위 테스트)와
//! **AWS 와 실제로 같은 값을 내는지**는 다른 질문이다.
//!
//! 자격증명은 AWS 공개 문서의 예시 값이다. 실제 자격증명이 아니다.

fn main() {
    let creds = aws_credential_types::Credentials::new(
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        None,
        None,
        "verify-iam-token",
    );
    let token = dbmon::aws::auth_token::presign(
        &creds,
        "ap-northeast-2",
        "orders-01.abc.ap-northeast-2.rds.amazonaws.com",
        3306,
        "dbmon",
        std::time::SystemTime::now(),
    )
    .expect("서명");
    println!("{token}");
}
