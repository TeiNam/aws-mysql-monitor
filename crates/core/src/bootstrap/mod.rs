//! 모니터링 계정 부트스트랩 (M3, [07](../../../../docs/07-credentials-bootstrap.md)).
//!
//! # 이 모듈은 순수하다
//!
//! DB 도 AWS 도 부르지 않는다. **현재 상태**(어댑터가 `SHOW CREATE USER` /
//! `SHOW GRANTS` 로 읽어 온 것)와 **원하는 상태**(설정)를 받아 실행할 액션 목록을
//! 만든다. 그래서 T-28(계정 선점) 방어와 멱등성 판정을 DB 없이 테스트할 수 있다 —
//! 그 두 가지가 이 모듈에서 가장 틀리기 쉬운 부분이다.
//!
//! # 왜 계획과 실행을 나누는가
//!
//! 실행은 **마스터 권한**으로 대상 DB 에 `CREATE USER` / `GRANT` 를 보낸다. 사람이
//! 승인할 대상은 "부트스트랩을 실행한다" 가 아니라 **정확히 이 문장들**이어야 한다.
//! 그래서 계획이 문장 전문을 담고, 실행은 계획을 다시 도출해 **지문이 같을 때만**
//! 진행한다([`Plan::fingerprint`]).

pub mod grants;
pub mod schemas;
pub mod sql;

use grants::{GrantScope, GrantSet};

/// 권한 모드 ([07 §2.3](../../../../docs/07-credentials-bootstrap.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivilegeMode {
    /// `SELECT ON *.*` — 전체 읽기. **기본값이다.**
    ///
    /// # 왜 기본값인가 (문서의 B 에서 바뀌었다)
    ///
    /// 문서는 모드 B(스키마 화이트리스트)를 기본값으로 적었다. 실제로 B 는 **새 스키마가
    /// 생길 때마다 조용히 관측을 잃는다** — 화이트리스트에 없는 스키마는 카디널리티·DDL
    /// 이 안 걸리고, 그 사실이 화면에 "정보 없음" 으로만 나온다. 모니터링 도구가 새
    /// 스키마를 못 보는 것은 관측 실패이므로 A 로 둔다.
    ///
    /// 잃는 것을 분명히 적는다: 이 계정은 **운영 데이터를 읽을 수 있다.**
    /// `rds-db:connect` 를 가진 주체는 누구나 이 계정으로 붙어 조회할 수 있으므로,
    /// 그 IAM 권한을 앱의 태스크 롤에만 부여하는 것이 유일한 방어선이다.
    /// 시스템 스키마는 권한이 아니라 [`schemas`] 의 제외 목록으로 **안 읽는다** —
    /// 읽을 수 없는 것과 다르다.
    #[default]
    Broad,
    /// 지정한 스키마만 `SELECT`. 화이트리스트를 사람이 관리한다.
    Least,
    /// 데이터 스키마 `SELECT` 없음. `performance_schema`·`sys` 만.
    Minimal,
}

impl PrivilegeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Broad => "broad",
            Self::Least => "least",
            Self::Minimal => "minimal",
        }
    }
}

/// 계정 인증 방식.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// `IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS'` — 비밀번호가 없다.
    #[default]
    IamDbAuth,
    /// 비밀번호 폴백 (FR-CRD-05). IAM DB 인증을 쓸 수 없을 때만.
    Password,
}

impl AuthMethod {
    /// 대상 DB 의 `plugin` 컬럼에 들어 있어야 하는 값.
    pub fn expected_plugin(self) -> &'static str {
        match self {
            Self::IamDbAuth => "AWSAuthenticationPlugin",
            // RDS MySQL 8.0/8.4 의 기본 플러그인.
            Self::Password => "mysql_native_password",
        }
    }
}

/// 원하는 상태 — 설정에서 온다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub user: String,
    /// 호스트 패턴. **`%` 를 기본값으로 두지 않는다** (T-04). 앱 서브넷 CIDR 을 넣는다.
    pub host: String,
    pub auth: AuthMethod,
    pub mode: PrivilegeMode,
    /// 모드 B 의 화이트리스트. 다른 모드에서는 무시된다.
    pub schemas: Vec<String>,
}

/// 전역 권한 — 모드와 무관하게 항상 같다 ([07 §2.2](../../../../docs/07-credentials-bootstrap.md)).
pub const GLOBAL_PRIVILEGES: &[&str] = &[
    "PROCESS",
    "REPLICATION CLIENT",
    "SHOW DATABASES",
    "SHOW VIEW",
];

/// 관측용으로 항상 읽어야 하는 스키마.
pub const OBSERVABILITY_SCHEMAS: &[&str] = &["performance_schema", "sys"];

impl Desired {
    /// 이 설정이 요구하는 권한 집합.
    pub fn grant_set(&self) -> GrantSet {
        let mut set = GrantSet::default();
        set.add(
            GrantScope::Global,
            GLOBAL_PRIVILEGES.iter().map(|s| s.to_string()),
        );
        for s in OBSERVABILITY_SCHEMAS {
            set.add(GrantScope::Schema(s.to_string()), ["SELECT".to_string()]);
        }
        match self.mode {
            PrivilegeMode::Broad => {
                set.add(GrantScope::Global, ["SELECT".to_string()]);
            }
            PrivilegeMode::Least => {
                for s in &self.schemas {
                    set.add(GrantScope::Schema(s.clone()), ["SELECT".to_string()]);
                }
                // 모드 B 는 `mysql.*` 이 없으므로 통계 신선도 테이블을 따로 연다
                // (ADR-016). 모드 A 는 `*.*` 이 이미 덮는다.
                for (db, tbl) in schemas::STATS_TABLES {
                    set.add(
                        GrantScope::Table(db.to_string(), tbl.to_string()),
                        ["SELECT".to_string()],
                    );
                }
            }
            PrivilegeMode::Minimal => {}
        }
        // **최소화한다.** 모드 A 의 전역 `SELECT` 가 관측 스키마의 `SELECT` 를 덮으므로,
        // 최소화하지 않으면 승인 화면에 중복 `GRANT` 가 셋 나온다.
        set.minimized()
    }

    /// 사용자·호스트·스키마 이름이 화이트리스트를 통과하는가 (T-18 1번).
    ///
    /// 통과하지 못하면 **계획을 만들지 않는다.** 인용 유틸이 2차 방어선이지만, 1차는
    /// 애초에 이상한 이름을 받지 않는 것이다.
    pub fn validate(&self) -> Result<(), InvalidDesired> {
        if !is_valid_account_name(&self.user) {
            return Err(InvalidDesired::User(self.user.clone()));
        }
        if !is_valid_host_pattern(&self.host) {
            return Err(InvalidDesired::Host(self.host.clone()));
        }
        for s in &self.schemas {
            if !is_valid_schema_name(s) {
                return Err(InvalidDesired::Schema(s.clone()));
            }
        }
        if self.mode == PrivilegeMode::Least && self.schemas.is_empty() {
            return Err(InvalidDesired::EmptyWhitelist);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidDesired {
    User(String),
    Host(String),
    Schema(String),
    EmptyWhitelist,
}

impl std::fmt::Display for InvalidDesired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User(u) => write!(
                f,
                "모니터링 계정 이름이 규칙에 맞지 않는다 ({u:?}) — 소문자로 시작하는 소문자·숫자·밑줄 32자 이내"
            ),
            Self::Host(h) => write!(
                f,
                "호스트 패턴이 규칙에 맞지 않는다 ({h:?}) — IPv4 숫자와 `%`·`_` 만 쓴다"
            ),
            Self::Schema(s) => write!(f, "스키마 이름이 규칙에 맞지 않는다 ({s:?})"),
            Self::EmptyWhitelist => {
                write!(f, "권한 모드 least 인데 스키마 화이트리스트가 비어 있다")
            }
        }
    }
}

/// `^[a-z][a-z0-9_]{0,31}$` (T-18 1번).
pub fn is_valid_account_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() <= 32
        && first.is_ascii_lowercase()
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// 호스트 패턴 — IPv4 숫자·점과 MySQL 와일드카드(`%`, `_`)만.
///
/// # 왜 이름을 받지 않는가
///
/// `dbmon@app.internal` 같은 이름 기반 호스트는 MySQL 이 **역방향 DNS 로 판정**한다.
/// DNS 를 인증 경계로 쓰면 그 경계가 DNS 침해로 넘어간다. CIDR 축약(`10.1.%`)만 받는다.
pub fn is_valid_host_pattern(s: &str) -> bool {
    if s.is_empty() || s.len() > 60 {
        return false;
    }

    // **IPv4 접두어 패턴만 받는다.**
    //
    // "숫자가 하나라도 있으면 통과" 로 판정했다가 `%1%`·`_%1%` 가 통과했다 —
    // 그것들은 임의의 IP·호스트를 대량으로 매칭하므로 앱 서브넷 제한이 아니다
    // (교차 리뷰 2차가 잡았다).
    //
    // 규칙:
    //
    // | 조각 | 허용 |
    // |---|---|
    // | 옥텟 수 | 1~4개 (점으로 나뉜다) |
    // | 각 옥텟 | 숫자(0~255) **또는** 와일드카드만(`%`/`_` 하나 이상) |
    // | **첫 옥텟** | 숫자여야 한다 |
    // | 숫자 옥텟 뒤 | 자유 |
    // | 와일드카드 옥텟 뒤 | 숫자 옥텟이 올 수 없다 (접두어여야 한다) |
    //
    // 마지막 규칙이 중요하다: `10.%.1.%` 를 허용하면 `10.x.1.y` 를 매칭하는데, 그건
    // 서브넷이 아니라 흩어진 주소 집합이다. 접두어 형태만 받는다.
    let octets: Vec<&str> = s.split('.').collect();
    if octets.is_empty() || octets.len() > 4 {
        return false;
    }

    let mut seen_wildcard = false;
    for (idx, octet) in octets.iter().enumerate() {
        if octet.is_empty() {
            // `10..1` 이나 앞뒤 점.
            return false;
        }
        let is_numeric = octet.bytes().all(|b| b.is_ascii_digit());
        let is_wildcard = octet.bytes().all(|b| b == b'%' || b == b'_');

        if is_numeric {
            // **와일드카드 뒤에 숫자가 오면 접두어가 아니다.**
            if seen_wildcard {
                return false;
            }
            // 옥텟 값 범위. `999` 는 IPv4 가 아니다.
            if octet.len() > 3 || octet.parse::<u16>().is_ok_and(|v| v > 255) {
                return false;
            }
        } else if is_wildcard {
            // **첫 옥텟은 숫자여야 한다.** `%.1.2.3` 은 어디서든이다.
            if idx == 0 {
                return false;
            }
            seen_wildcard = true;
        } else {
            // 숫자와 와일드카드가 섞였다 (`1%`, `%1%`).
            //
            // 섞인 옥텟을 허용하면 `%1%` 같은 값이 통과한다. MySQL 은 그것을
            // 문자열 패턴으로 매칭하므로 `10.1.1.1`·`210.1.1.199` 등이 모두 걸린다.
            return false;
        }
    }
    true
}

/// 스키마 이름 — 영숫자·밑줄·하이픈. 64자.
pub fn is_valid_schema_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 대상 DB 에서 읽어 온 현재 상태.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentState {
    pub user_exists: bool,
    /// `mysql.user.plugin`. 계정이 없으면 `None`.
    pub auth_plugin: Option<String>,
    /// `SHOW CREATE USER` 에 `REQUIRE SSL` 이 있는가.
    pub requires_ssl: bool,
    /// 계정이 잠겨 있는가 (`ACCOUNT LOCK`).
    pub locked: bool,
    pub grants: GrantSet,
    /// 읽을 수 없었던 `SHOW GRANTS` 줄. **비어 있지 않으면 진행하지 않는다.**
    pub unparsed_grants: Vec<String>,
    /// 인스턴스에서 IAM DB 인증이 켜져 있는가 (`DescribeDB*`).
    pub iam_auth_enabled: bool,
    /// `mysql.user.authentication_string` 의 **해시**. 원문을 담지 않는다.
    ///
    /// # 왜 필요한가 (교차 리뷰가 잡은 결함)
    ///
    /// 계획과 실행 사이에 **비밀번호가 바뀌면** 플러그인·SSL·권한은 그대로이므로
    /// 기존 검사가 전부 통과한다. 비밀번호 폴백 경로에서는 그 계정의 비밀번호를
    /// 아는 쪽이 우리가 아닐 수 있고, 그 계정에 `GRANT` 하면 T-28 시나리오가 성립한다.
    ///
    /// 원문(해시된 비밀번호)을 담지 않는 이유: 그 값 자체가 오프라인 대입 공격의
    /// 재료다. 우리가 필요한 것은 "바뀌었는가" 뿐이므로 해시로 충분하다.
    pub auth_string_digest: Option<String>,
}

impl CurrentState {
    /// **보안 판정에 쓰이는 상태의 지문.**
    ///
    /// 계획과 실행 사이에 이 값이 바뀌면 거부한다. [`Plan::fingerprint`] 는 *액션* 을
    /// 해시하므로 액션이 같아지는 상태 변화(비밀번호 교체 등)를 못 잡는다 — 그게
    /// 교차 리뷰가 지적한 자리다.
    pub fn security_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update([
            self.user_exists as u8,
            self.requires_ssl as u8,
            self.locked as u8,
        ]);
        h.update(self.auth_plugin.as_deref().unwrap_or("<none>").as_bytes());
        h.update(b"\0");
        h.update(
            self.auth_string_digest
                .as_deref()
                .unwrap_or("<none>")
                .as_bytes(),
        );
        h.update(b"\0");
        h.update([self.grants.has_grant_option as u8]);
        // 권한 집합 전체. 정렬된 `BTreeMap`/`BTreeSet` 이라 순회가 결정적이다.
        for (scope, privs) in self.grants.scopes() {
            h.update(format!("{scope}").as_bytes());
            for p in privs {
                h.update(b"|");
                h.update(p.as_bytes());
            }
            h.update(b"\n");
        }
        for r in self.grants.roles() {
            h.update(b"role|");
            h.update(r.as_bytes());
        }
        format!("{:x}", h.finalize())
    }
}

/// 실행할 액션 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 대상 DB 에 보낼 SQL. [`sql::Statement`] 가 비밀 유출을 타입으로 막는다.
    Sql(sql::Statement),
    /// RDS 설정 변경. **우리가 실행하지 않는다** — 명령을 보여준다
    /// ([07 §2.1](../../../../docs/07-credentials-bootstrap.md), 기본값
    /// `allow_rds_modify = false`).
    EnableIamAuth { instance_id: String },
}

/// 진행을 막는 사유 — **경고가 아니라 차단이다** (§2.6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// 계정이 이미 있는데 인증 플러그인이 기대와 다르다.
    ///
    /// 이게 T-28 의 핵심이다. `CREATE USER IF NOT EXISTS` 는 조용히 통과하고, 그 뒤의
    /// `GRANT` 가 **공격자가 비밀번호를 아는 계정**에 권한을 준다.
    AuthPluginMismatch { found: String, expected: String },
    /// 계정이 있는데 `REQUIRE SSL` 이 없다.
    SslNotRequired,
    /// 계정이 잠겨 있다. 잠긴 계정에 권한을 주는 것은 의도를 알 수 없다.
    AccountLocked,
    /// `SHOW GRANTS` 를 다 읽지 못했다 — 못 읽은 줄에 초과 권한이 있을 수 있다.
    UnreadableGrants(Vec<String>),
    /// `WITH GRANT OPTION` 이 붙어 있다. 모니터링 계정에 있어서는 안 된다.
    GrantOptionPresent,
    /// prd 인데 초과 권한이 있다 (비prd 는 경고).
    ExcessPrivilegesInProduction(Vec<String>),
    /// IAM DB 인증이 꺼져 있는데 IAM 방식을 요구했다.
    IamAuthDisabled,
    /// 계획 시점과 실행 시점의 상태가 다르다.
    PlanStale { expected: String, found: String },
    /// 비밀번호 방식인데 계정이 이미 있다.
    ///
    /// # 왜 차단인가 (교차 리뷰가 잡은 결함)
    ///
    /// 플러그인이 기대와 같아도 **그 계정의 비밀번호를 우리가 안다고 증명할 수 없다.**
    /// 누가 먼저 만들었을 수 있고, 그러면 `GRANT` 는 남이 아는 계정에 권한을 준다.
    /// IAM 방식은 비밀번호가 없으므로 이 문제가 없다 — 그래서 비밀번호 방식만 막는다.
    ///
    /// 되살리려면 사람이 계정을 지우거나 `ALTER USER` 로 비밀번호를 다시 세운다.
    PasswordAccountAlreadyExists,
}

impl std::fmt::Display for Blocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthPluginMismatch { found, expected } => write!(
                f,
                "계정이 이미 있고 인증 플러그인이 다르다 (있는 것: {found}, 기대: {expected}). \
                 누가 이 계정을 먼저 만들었을 수 있으므로 권한을 주지 않는다"
            ),
            Self::SslNotRequired => write!(
                f,
                "계정에 REQUIRE SSL 이 없다 — 평문 연결이 가능한 계정에 권한을 주지 않는다"
            ),
            Self::AccountLocked => write!(f, "계정이 잠겨 있다 (ACCOUNT LOCK)"),
            Self::UnreadableGrants(lines) => write!(
                f,
                "SHOW GRANTS 를 {}줄 읽지 못했다 — 초과 권한이 숨어 있을 수 있다",
                lines.len()
            ),
            Self::GrantOptionPresent => write!(
                f,
                "계정에 WITH GRANT OPTION 이 있다 — 권한을 남에게 줄 수 있다"
            ),
            Self::ExcessPrivilegesInProduction(items) => write!(
                f,
                "prd 계정에 요구하지 않은 권한이 {}개 있다: {}",
                items.len(),
                items.join(", ")
            ),
            Self::IamAuthDisabled => write!(
                f,
                "인스턴스에 IAM DB 인증이 꺼져 있다 — 계정을 만들어도 접속할 수 없다"
            ),
            Self::PlanStale { expected, found } => write!(
                f,
                "계획을 만든 뒤 상태가 바뀌었다 (계획: {expected}, 현재: {found})"
            ),
            Self::PasswordAccountAlreadyExists => write!(
                f,
                "비밀번호 방식인데 계정이 이미 있다 — 그 비밀번호를 우리가 안다고 \
                 증명할 수 없으므로 권한을 주지 않는다. 계정을 지우거나 비밀번호를 \
                 다시 세운 뒤 계획을 만든다"
            ),
        }
    }
}

/// 실행 계획.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// 진행을 막는 사유. **비어 있지 않으면 실행하지 않는다.**
    pub blockers: Vec<Blocker>,
    /// 진행은 하되 화면에 보여줄 것.
    pub warnings: Vec<String>,
    /// 요구하지 않은 권한. `REVOKE` 하지 않는다 (07 §2.6 4단계).
    pub excess: Vec<String>,
}

impl Plan {
    /// 실행해도 되는가.
    pub fn is_executable(&self) -> bool {
        self.blockers.is_empty()
    }

    /// 할 일이 없는가 (이미 원하는 상태다).
    pub fn is_noop(&self) -> bool {
        self.actions.is_empty()
    }

    /// 실행할 SQL 문장만.
    pub fn statements(&self) -> impl Iterator<Item = &sql::Statement> {
        self.actions.iter().filter_map(|a| match a {
            Action::Sql(s) => Some(s),
            Action::EnableIamAuth { .. } => None,
        })
    }

    /// **계획과 실행 사이에 상태가 바뀌었는지 판정하는 지문** (§2.6.1 a).
    ///
    /// 실행 시점에 현재 상태로 계획을 다시 만들어 이 값을 비교한다. 다르면 거부한다 —
    /// 그게 없으면 "사용자가 승인한 SQL" 과 "실제 실행 SQL" 이 다를 수 있다.
    ///
    /// 문장의 **마스킹된** 형태를 넣는다. 비밀번호 폴백 경로는 비밀번호가 매번 달라도
    /// 지문이 같아야 한다 — 사람이 승인한 것은 "이 계정을 비밀번호로 만든다" 이고
    /// 비밀번호 값이 아니다.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for a in &self.actions {
            match a {
                Action::Sql(s) => {
                    h.update(b"sql\0");
                    h.update(s.redacted().as_bytes());
                }
                Action::EnableIamAuth { instance_id } => {
                    h.update(b"iam\0");
                    h.update(instance_id.as_bytes());
                }
            }
            h.update(b"\n");
        }
        // 차단 사유도 넣는다 — 계획 시점에 차단이던 것이 실행 시점에 풀렸다면 그것도
        // 상태 변화이므로 사람이 다시 봐야 한다.
        for b in &self.blockers {
            h.update(b"block\0");
            h.update(format!("{b:?}").as_bytes());
            h.update(b"\n");
        }
        format!("{:x}", h.finalize())
    }
}

/// 계획을 만든다. **이 함수가 M3 의 판정 전부다.**
///
/// `is_production` 은 초과 권한을 차단으로 볼지 경고로 볼지 가른다 (§2.6.1 c).
pub fn plan(
    desired: &Desired,
    current: &CurrentState,
    instance_id: &str,
    is_production: bool,
) -> Result<Plan, InvalidDesired> {
    desired.validate()?;

    let mut actions = Vec::new();
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if desired.auth == AuthMethod::IamDbAuth && !current.iam_auth_enabled {
        // 차단이면서 액션이기도 하다 — 사람이 명령을 실행하면 풀린다.
        actions.push(Action::EnableIamAuth {
            instance_id: instance_id.to_string(),
        });
        blockers.push(Blocker::IamAuthDisabled);
    }

    // **읽지 못한 줄이 있으면 아무것도 하지 않는다.**
    if !current.unparsed_grants.is_empty() {
        blockers.push(Blocker::UnreadableGrants(current.unparsed_grants.clone()));
    }

    if current.user_exists {
        // ── T-28: 기존 계정의 상태를 검사한다. 어긋나면 GRANT 하지 않는다 ──
        let expected = desired.auth.expected_plugin();
        match current.auth_plugin.as_deref() {
            Some(found) if !found.eq_ignore_ascii_case(expected) => {
                blockers.push(Blocker::AuthPluginMismatch {
                    found: found.to_string(),
                    expected: expected.to_string(),
                });
            }
            // 계정이 있다는데 플러그인을 못 읽었으면 판정할 수 없다 → 차단.
            None => blockers.push(Blocker::AuthPluginMismatch {
                found: "<unknown>".into(),
                expected: expected.to_string(),
            }),
            Some(_) => {}
        }
        if !current.requires_ssl {
            blockers.push(Blocker::SslNotRequired);
        }
        if current.locked {
            blockers.push(Blocker::AccountLocked);
        }
        if current.grants.has_grant_option {
            blockers.push(Blocker::GrantOptionPresent);
        }
        // **비밀번호 방식은 기존 계정을 받지 않는다.** 근거는 변형 문서에 있다.
        if desired.auth == AuthMethod::Password {
            blockers.push(Blocker::PasswordAccountAlreadyExists);
        }
    } else {
        actions.push(Action::Sql(sql::create_user(desired)?));
    }

    // ── 권한 차집합: 부족한 것만 GRANT 한다 (멱등성) ──
    let want = desired.grant_set();
    for (scope, privileges) in current.grants.missing_from(&want) {
        actions.push(Action::Sql(sql::grant(desired, &scope, &privileges)?));
    }

    // ── 초과 권한: REVOKE 하지 않는다 ──
    let excess: Vec<String> = current
        .grants
        .excess_over(&want)
        .into_iter()
        .map(|(scope, p)| format!("{p} ON {scope}"))
        .chain(current.grants.roles().iter().map(|r| format!("ROLE {r}")))
        .collect();
    if !excess.is_empty() {
        if is_production {
            blockers.push(Blocker::ExcessPrivilegesInProduction(excess.clone()));
        } else {
            warnings.push(format!(
                "요구하지 않은 권한이 {}개 있다. 우리가 회수하지 않는다 — 의도적으로 준 것일 수 있다",
                excess.len()
            ));
        }
    }

    if is_production {
        warnings.push("prd 환경이다. 인스턴스 식별자를 타이핑해야 실행된다".into());
    }
    if desired.mode == PrivilegeMode::Broad {
        warnings.push(
            "권한 모드 broad — 이 계정은 모든 스키마를 읽을 수 있다. \
             rds-db:connect 를 앱 태스크 롤에만 부여했는지 확인한다"
                .into(),
        );
    }

    Ok(Plan {
        actions,
        blockers,
        warnings,
        excess,
    })
}

/// 실행 직전에 계획이 아직 유효한지 확인한다 (§2.6.1 a·b).
///
/// `stored_fingerprint` 는 계획 시점의 지문이고 `planned_user_exists` 는 그때의
/// 계정 존재 여부다. 현재 상태로 계획을 다시 만들어 비교한다.
pub fn revalidate(
    desired: &Desired,
    current: &CurrentState,
    instance_id: &str,
    is_production: bool,
    stored_fingerprint: &str,
    planned_user_exists: bool,
    stored_state_digest: &str,
) -> Result<Plan, Vec<Blocker>> {
    // ── 검사 순서는 **진단 품질** 순이다 ──
    //
    // 세 검사가 같은 사건을 잡을 수 있다. 사람에게 무슨 일이 있었는지 말해 주는
    // 검사를 먼저 둔다 — "지문이 다르다" 는 맞지만 아무것도 알려주지 않는다.

    // **(b) 계획은 계정이 없다고 했는데 지금 있다.**
    //
    // T-28 시나리오의 머리말이다. 이 사실을 그대로 말해 줘야 사람이 대응할 수 있다.
    if !planned_user_exists && current.user_exists {
        return Err(vec![Blocker::PlanStale {
            expected: "계정 없음".into(),
            found: "계정이 이미 존재한다 — 계획 이후에 누가 만들었다".into(),
        }]);
    }

    // **(a′) 보안 상태가 그대로인가.**
    //
    // 계획 이후에 비밀번호가 바뀌었거나 권한이 늘었으면 **액션이 같아도** 상황이
    // 다르다. 액션 지문은 그것을 못 잡는다 — 교차 리뷰가 잡은 T-28 잔여 경로다.
    if current.security_digest() != stored_state_digest {
        return Err(vec![Blocker::PlanStale {
            expected: "계획 시점의 계정 상태".into(),
            found: "계정 상태가 바뀌었다 (인증 자료·권한·SSL·잠금 중 하나) — 다시 계획한다".into(),
        }]);
    }

    let fresh = match plan(desired, current, instance_id, is_production) {
        Ok(p) => p,
        Err(e) => {
            return Err(vec![Blocker::PlanStale {
                expected: stored_fingerprint.to_string(),
                found: format!("설정이 무효해졌다: {e}"),
            }]);
        }
    };

    let fresh_fp = fresh.fingerprint();
    if fresh_fp != stored_fingerprint {
        return Err(vec![Blocker::PlanStale {
            expected: stored_fingerprint.to_string(),
            found: fresh_fp,
        }]);
    }
    if !fresh.is_executable() {
        return Err(fresh.blockers);
    }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired() -> Desired {
        Desired {
            user: "dbmon".into(),
            host: "10.1.%".into(),
            auth: AuthMethod::IamDbAuth,
            mode: PrivilegeMode::Broad,
            schemas: vec![],
        }
    }

    fn fresh_instance() -> CurrentState {
        CurrentState {
            iam_auth_enabled: true,
            ..Default::default()
        }
    }

    /// 계정이 있고 모드 A 의 권한이 이미 전부 있는 상태.
    fn fully_granted() -> CurrentState {
        let (grants, unparsed) = grants::parse_grants([
            "GRANT SELECT, PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
        ]);
        assert!(unparsed.is_empty());
        CurrentState {
            user_exists: true,
            auth_plugin: Some("AWSAuthenticationPlugin".into()),
            requires_ssl: true,
            locked: false,
            grants,
            unparsed_grants: vec![],
            iam_auth_enabled: true,
            auth_string_digest: None,
        }
    }

    fn redacted_sqls(p: &Plan) -> Vec<String> {
        p.statements().map(|s| s.redacted().to_string()).collect()
    }

    #[test]
    fn a_fresh_instance_gets_create_user_then_grants() {
        let p = plan(&desired(), &fresh_instance(), "inst", false).expect("계획");
        assert!(p.is_executable(), "{:?}", p.blockers);
        let sqls = redacted_sqls(&p);
        assert!(sqls[0].starts_with("CREATE USER"), "{:?}", sqls[0]);
        assert!(
            sqls[0].contains("AWSAuthenticationPlugin") && sqls[0].contains("REQUIRE SSL"),
            "{:?}",
            sqls[0]
        );
        assert!(
            sqls.iter().any(|s| s.contains("PROCESS")),
            "전역 권한이 없다: {sqls:?}"
        );
        assert!(
            sqls.iter()
                .any(|s| s.contains("SELECT") && s.contains("*.*")),
            "모드 A 의 전체 SELECT 가 없다: {sqls:?}"
        );
    }

    /// **M3-8 멱등성** — 이미 원하는 상태면 액션이 없다.
    #[test]
    fn a_fully_granted_account_produces_no_actions() {
        let p = plan(&desired(), &fully_granted(), "inst", false).expect("계획");
        assert!(p.is_executable(), "{:?}", p.blockers);
        assert!(p.is_noop(), "다시 실행할 것이 있다: {:?}", p.actions);
    }

    /// 부족한 권한만 추가한다 — 이미 있는 것을 다시 주지 않는다.
    #[test]
    fn only_missing_privileges_are_granted() {
        let (grants, _) =
            grants::parse_grants(["GRANT PROCESS, SHOW DATABASES ON *.* TO `dbmon`@`10.1.%`"]);
        let current = CurrentState {
            grants,
            ..fully_granted()
        };
        let p = plan(&desired(), &current, "inst", false).expect("계획");
        let sqls = redacted_sqls(&p);
        assert_eq!(sqls.len(), 1, "한 문장으로 묶여야 한다: {sqls:?}");
        let only = &sqls[0];
        assert!(only.contains("REPLICATION CLIENT") && only.contains("SHOW VIEW"));
        assert!(only.contains("SELECT"));
        assert!(
            !only.contains("PROCESS"),
            "이미 있는 PROCESS 를 다시 준다: {only}"
        );
    }

    /// **T-28 — 계정 선점.** 플러그인이 다르면 `GRANT` 하지 않는다.
    ///
    /// 이 테스트가 M3 에서 가장 중요하다. 이게 깨지면 공격자가 비밀번호를 아는
    /// 계정에 prd 스키마 `SELECT` + `PROCESS` 가 부여된다.
    #[test]
    fn t28_a_preexisting_account_with_a_different_plugin_is_blocked() {
        let current = CurrentState {
            auth_plugin: Some("mysql_native_password".into()),
            ..fully_granted()
        };
        let p = plan(&desired(), &current, "inst", false).expect("계획");
        assert!(!p.is_executable(), "선점된 계정에 권한을 주려 한다");
        assert!(
            p.blockers
                .iter()
                .any(|b| matches!(b, Blocker::AuthPluginMismatch { .. })),
            "{:?}",
            p.blockers
        );
    }

    /// 플러그인을 **읽지 못한** 경우도 차단이다 — 판정할 수 없으면 진행하지 않는다.
    #[test]
    fn an_unknown_plugin_on_an_existing_account_is_blocked() {
        let current = CurrentState {
            auth_plugin: None,
            ..fully_granted()
        };
        assert!(
            !plan(&desired(), &current, "inst", false)
                .expect("계획")
                .is_executable()
        );
    }

    #[test]
    fn ssl_not_required_is_blocked() {
        let current = CurrentState {
            requires_ssl: false,
            ..fully_granted()
        };
        let p = plan(&desired(), &current, "inst", false).expect("계획");
        assert!(p.blockers.contains(&Blocker::SslNotRequired));
    }

    #[test]
    fn a_locked_account_is_blocked() {
        let current = CurrentState {
            locked: true,
            ..fully_granted()
        };
        assert!(
            plan(&desired(), &current, "inst", false)
                .expect("계획")
                .blockers
                .contains(&Blocker::AccountLocked)
        );
    }

    #[test]
    fn grant_option_is_blocked() {
        let (grants, _) = grants::parse_grants([
            "GRANT SELECT, PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO `dbmon`@`10.1.%` WITH GRANT OPTION",
        ]);
        let current = CurrentState {
            grants,
            ..fully_granted()
        };
        assert!(
            plan(&desired(), &current, "inst", false)
                .expect("계획")
                .blockers
                .contains(&Blocker::GrantOptionPresent)
        );
    }

    /// **못 읽은 `SHOW GRANTS` 줄이 있으면 진행하지 않는다.**
    #[test]
    fn unreadable_grant_lines_block_everything() {
        let current = CurrentState {
            unparsed_grants: vec!["GRANT ??? ON ??? TO ???".into()],
            ..fully_granted()
        };
        assert!(
            !plan(&desired(), &current, "inst", false)
                .expect("계획")
                .is_executable()
        );
    }

    /// 초과 권한: prd 는 차단, 비prd 는 경고.
    #[test]
    fn excess_privileges_block_production_but_only_warn_elsewhere() {
        let (grants, _) = grants::parse_grants([
            "GRANT SELECT, PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
            "GRANT INSERT ON `shop`.* TO `dbmon`@`10.1.%`",
        ]);
        let current = CurrentState {
            grants,
            ..fully_granted()
        };

        let prd = plan(&desired(), &current, "inst", true).expect("계획");
        assert!(!prd.is_executable(), "prd 에서 초과 권한을 통과시켰다");

        let dev = plan(&desired(), &current, "inst", false).expect("계획");
        assert!(dev.is_executable());
        assert!(!dev.warnings.is_empty());
        assert!(dev.excess.iter().any(|e| e.contains("INSERT")));
    }

    /// 롤도 초과 권한으로 센다 — 롤 안을 펼치지 않으므로.
    #[test]
    fn granted_roles_count_as_excess() {
        let (grants, _) = grants::parse_grants([
            "GRANT SELECT, PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
            "GRANT `app_writer`@`%` TO `dbmon`@`10.1.%`",
        ]);
        let current = CurrentState {
            grants,
            ..fully_granted()
        };
        let p = plan(&desired(), &current, "inst", false).expect("계획");
        assert!(
            p.excess.iter().any(|e| e.starts_with("ROLE")),
            "{:?}",
            p.excess
        );
    }

    /// IAM 인증이 꺼져 있으면 차단하되 **활성화 액션을 보여준다.**
    #[test]
    fn iam_auth_disabled_blocks_and_surfaces_the_enable_action() {
        let current = CurrentState {
            iam_auth_enabled: false,
            ..fresh_instance()
        };
        let p = plan(&desired(), &current, "orders-prd-01", false).expect("계획");
        assert!(p.blockers.contains(&Blocker::IamAuthDisabled));
        assert!(p.actions.contains(&Action::EnableIamAuth {
            instance_id: "orders-prd-01".into()
        }));
    }

    // ── 입력 검증 (T-18 1번) ─────────────────────────────────────────────────

    /// 인젝션 페이로드 표. **계획 자체가 만들어지지 않아야 한다.**
    #[test]
    fn t18_injection_payloads_are_rejected_before_any_sql_is_built() {
        let payloads = [
            "dbmon'@'%' IDENTIFIED BY 'p'; GRANT ALL ON *.* TO 'evil'@'%'; -- ",
            "dbmon`@`%`",
            "dbmon; DROP DATABASE shop",
            "dbmon'",
            "dbmon\"",
            "db mon",
            "DBMON",
            "1dbmon",
            "",
            "dbmon\n",
            "dbmon\\",
        ];
        for p in payloads {
            let d = Desired {
                user: p.into(),
                ..desired()
            };
            assert!(
                matches!(d.validate(), Err(InvalidDesired::User(_))),
                "사용자명 {p:?} 가 통과했다"
            );
            assert!(plan(&d, &fresh_instance(), "i", false).is_err());
        }
    }

    #[test]
    fn t04_a_bare_wildcard_host_is_rejected() {
        let d = Desired {
            host: "%".into(),
            ..desired()
        };
        assert!(matches!(d.validate(), Err(InvalidDesired::Host(_))));
    }

    /// 이름 기반 호스트는 받지 않는다 — 역방향 DNS 를 인증 경계로 쓰지 않는다.
    #[test]
    fn hostname_patterns_are_rejected() {
        for h in [
            "app.internal",
            "%.example.com",
            "localhost",
            "10.1.%; --",
            "",
        ] {
            let d = Desired {
                host: h.into(),
                ..desired()
            };
            assert!(
                matches!(d.validate(), Err(InvalidDesired::Host(_))),
                "{h:?} 가 통과했다"
            );
        }
        for h in ["10.1.%", "10.1.2.3", "10.%.%.%", "10.1.__"] {
            let d = Desired {
                host: h.into(),
                ..desired()
            };
            assert!(d.validate().is_ok(), "{h} 가 막혔다");
        }
    }

    #[test]
    fn least_mode_requires_a_whitelist() {
        let d = Desired {
            mode: PrivilegeMode::Least,
            schemas: vec![],
            ..desired()
        };
        assert_eq!(d.validate(), Err(InvalidDesired::EmptyWhitelist));
    }

    /// 모드 B 는 스키마별 `SELECT` + 통계 테이블을 요구한다.
    #[test]
    fn least_mode_grants_per_schema_plus_stats_tables() {
        let d = Desired {
            mode: PrivilegeMode::Least,
            schemas: vec!["shop".into()],
            ..desired()
        };
        let want = d.grant_set();
        assert!(want.covers(&GrantScope::Schema("shop".into()), "SELECT"));
        assert!(!want.covers(&GrantScope::Schema("other".into()), "SELECT"));
        assert!(want.covers(
            &GrantScope::Table("mysql".into(), "innodb_table_stats".into()),
            "SELECT"
        ));
        // 전역 SELECT 는 주지 않는다.
        assert!(!want.covers(&GrantScope::Global, "SELECT"));
    }

    /// 모드 C 는 데이터 `SELECT` 가 없다.
    #[test]
    fn minimal_mode_grants_no_data_select() {
        let d = Desired {
            mode: PrivilegeMode::Minimal,
            ..desired()
        };
        let want = d.grant_set();
        assert!(!want.covers(&GrantScope::Global, "SELECT"));
        assert!(!want.covers(&GrantScope::Schema("shop".into()), "SELECT"));
        // 관측 스키마는 여전히 읽는다.
        assert!(want.covers(&GrantScope::Schema("performance_schema".into()), "SELECT"));
        assert!(want.covers(&GrantScope::Global, "PROCESS"));
    }

    /// 모드 A 는 전역 `SELECT` 하나로 시스템 스키마까지 덮는다 — 그래서 제외는
    /// 권한이 아니라 수집·표시 단계에서 한다.
    #[test]
    fn broad_mode_covers_everything_including_system_schemas() {
        let want = desired().grant_set();
        assert!(want.covers(&GrantScope::Global, "SELECT"));
        assert!(want.covers(&GrantScope::Schema("mysql".into()), "SELECT"));
        assert!(schemas::is_system_schema("mysql"));
    }

    /// **`%` 와 같은 뜻인 호스트 패턴을 전부 막는다** (교차 리뷰가 잡은 결함).
    ///
    /// `s != "%"` 만 보면 `%%`·`%.%`·`_` 가 통과하는데 MySQL 에서는 전부 모든 호스트를
    /// 매칭한다 — T-04 가 막으려던 것이 그대로 통과한다.
    #[test]
    fn host_patterns_equivalent_to_wildcard_are_rejected() {
        for h in [
            "%",
            "%%",
            "%.%",
            "_",
            "___",
            "%._",
            "..",
            ".",
            "%%%",
            // **숫자를 하나 섞어도 안 된다** (2차 리뷰가 잡았다). MySQL 은 이것을
            // 문자열 패턴으로 매칭하므로 `10.1.1.1`·`210.1.1.199` 가 모두 걸린다.
            "%1%",
            "%0%",
            "_%1%",
            "1%2",
            "%.1.%",
            // 와일드카드 뒤의 숫자는 접두어가 아니다 — 흩어진 주소 집합이다.
            "10.%.1.%",
            "10.%.1",
            "10._.2.3",
            // 첫 옥텟이 와일드카드면 어디서든이다.
            "%.1.2.3",
            "_.1.2.3",
            // 옥텟이 5개 이상이거나 값이 범위를 넘는다.
            "10.1.2.3.4",
            "999.1.1.1",
            "10.999.%",
            // 빈 옥텟.
            "10..1",
            ".10.1",
            "10.1.",
        ] {
            let d = Desired {
                host: h.into(),
                ..desired()
            };
            assert!(
                matches!(d.validate(), Err(InvalidDesired::Host(_))),
                "{h:?} 가 통과했다 — 모든 호스트를 허용한다"
            );
        }
        // 숫자로 좁힌 패턴만 받는다.
        // 받는 것: **IPv4 접두어**다.
        for h in [
            "10.1.%",
            "10.1.2.3",
            "10.%.%.%",
            "10.1.__",
            "192.168.%",
            "10",
            "10.1",
            "172.16.%.%",
            "10.255.%",
        ] {
            let d = Desired {
                host: h.into(),
                ..desired()
            };
            assert!(d.validate().is_ok(), "{h} 가 막혔다");
        }
    }

    /// **비밀번호 방식은 기존 계정을 받지 않는다** (교차 리뷰가 잡은 결함).
    ///
    /// 플러그인이 같아도 그 비밀번호를 우리가 안다고 증명할 수 없다. IAM 방식은
    /// 비밀번호가 없으므로 이 제약이 없다.
    #[test]
    fn password_mode_refuses_a_preexisting_account() {
        let current = CurrentState {
            auth_plugin: Some("mysql_native_password".into()),
            ..fully_granted()
        };
        let pw_desired = Desired {
            auth: AuthMethod::Password,
            ..desired()
        };
        let p = plan(&pw_desired, &current, "inst", false).expect("계획");
        assert!(
            p.blockers.contains(&Blocker::PasswordAccountAlreadyExists),
            "{:?}",
            p.blockers
        );

        // IAM 방식은 같은 상황에서 이 차단이 없다 (플러그인 불일치로 걸릴 뿐이다).
        let iam = plan(&desired(), &fully_granted(), "inst", false).expect("계획");
        assert!(
            !iam.blockers
                .contains(&Blocker::PasswordAccountAlreadyExists)
        );
    }

    /// **인증 자료가 바뀌면 재검증이 거부한다** (교차 리뷰가 잡은 T-28 잔여 경로).
    ///
    /// 플러그인·SSL·권한이 그대로면 액션 지문이 같으므로, 그것만으로는 "계획 후
    /// 비밀번호가 바뀌었다" 를 못 잡는다.
    #[test]
    fn a_credential_change_between_plan_and_apply_is_refused() {
        let before = CurrentState {
            auth_string_digest: Some("digest-of-old-password".into()),
            ..fully_granted()
        };
        let planned = plan(&desired(), &before, "inst", false).expect("계획");
        let fp = planned.fingerprint();
        let sd = before.security_digest();

        // 같은 상태면 통과한다.
        assert!(revalidate(&desired(), &before, "inst", false, &fp, true, &sd).is_ok());

        // 비밀번호만 바뀌었다 — 액션은 같다(둘 다 noop).
        let after = CurrentState {
            auth_string_digest: Some("digest-of-NEW-password".into()),
            ..fully_granted()
        };
        assert_eq!(
            plan(&desired(), &after, "inst", false)
                .expect("계획")
                .fingerprint(),
            fp,
            "전제: 액션 지문은 같다"
        );
        let err = revalidate(&desired(), &after, "inst", false, &fp, true, &sd)
            .expect_err("거부해야 한다");
        assert!(
            matches!(err.first(), Some(Blocker::PlanStale { .. })),
            "{err:?}"
        );
    }

    /// 상태 지문이 **권한 변화도** 잡는다.
    #[test]
    fn the_state_digest_covers_grant_changes() {
        let a = fully_granted();
        let (grants, _) = grants::parse_grants([
            "GRANT SELECT, PROCESS, REPLICATION CLIENT, SHOW DATABASES, SHOW VIEW ON *.* TO `dbmon`@`10.1.%`",
            "GRANT INSERT ON `shop`.* TO `dbmon`@`10.1.%`",
        ]);
        let b = CurrentState {
            grants,
            ..fully_granted()
        };
        assert_ne!(a.security_digest(), b.security_digest());
        // 같은 상태는 같은 지문이다 (재검증이 성립하려면 필요하다).
        assert_eq!(a.security_digest(), fully_granted().security_digest());
    }

    // ── 지문과 재검증 (§2.6.1 a·b) ────────────────────────────────────────────

    #[test]
    fn the_fingerprint_changes_when_the_actions_change() {
        let a = plan(&desired(), &fresh_instance(), "inst", false).expect("계획");
        let b = plan(&desired(), &fully_granted(), "inst", false).expect("계획");
        assert_ne!(a.fingerprint(), b.fingerprint());
        // 같은 입력이면 같은 지문 — 실행 시 재도출이 성립하려면 필요하다.
        let a2 = plan(&desired(), &fresh_instance(), "inst", false).expect("계획");
        assert_eq!(a.fingerprint(), a2.fingerprint());
    }

    /// **§2.6.1 (b)** — 계획은 계정이 없다고 했는데 실행 시 있으면 거부한다.
    #[test]
    fn apply_refuses_when_an_account_appeared_after_planning() {
        let planned = plan(&desired(), &fresh_instance(), "inst", false).expect("계획");
        let fp = planned.fingerprint();

        // 그 사이에 누가 계정을 만들었다.
        let hijacked = CurrentState {
            user_exists: true,
            auth_plugin: Some("mysql_native_password".into()),
            requires_ssl: false,
            ..fresh_instance()
        };
        let sd = fresh_instance().security_digest();
        let err = revalidate(&desired(), &hijacked, "inst", false, &fp, false, &sd)
            .expect_err("거부해야 한다");
        // **"계정이 나타났다" 로 진단돼야 한다.** 상태 지문 불일치로만 걸리면
        // 사람이 무슨 일이 있었는지 모른다 — T-28 시나리오는 그 사실이 요점이다.
        assert!(
            matches!(err.first(), Some(Blocker::PlanStale { found, .. }) if found.contains("이미 존재")),
            "{err:?}"
        );
    }

    #[test]
    fn revalidate_passes_when_nothing_changed() {
        let current = fresh_instance();
        let planned = plan(&desired(), &current, "inst", false).expect("계획");
        let fp = planned.fingerprint();
        let ok = revalidate(
            &desired(),
            &current,
            "inst",
            false,
            &fp,
            false,
            &current.security_digest(),
        )
        .expect("통과");
        assert_eq!(ok.actions, planned.actions);
    }

    /// 지문이 다르면 거부한다 — 상태가 어떻게 바뀌었든.
    #[test]
    fn revalidate_refuses_on_any_fingerprint_change() {
        let planned = plan(&desired(), &fresh_instance(), "inst", false).expect("계획");
        let fp = planned.fingerprint();
        // 계정은 여전히 없지만 IAM 인증이 꺼졌다.
        let changed = CurrentState {
            iam_auth_enabled: false,
            ..fresh_instance()
        };
        assert!(
            revalidate(
                &desired(),
                &changed,
                "inst",
                false,
                &fp,
                false,
                &fresh_instance().security_digest()
            )
            .is_err()
        );
    }
}
