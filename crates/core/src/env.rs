//! 환경 분류 (FR-DSC-03, FR-DSC-04).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Env {
    Prd,
    Stg,
    Dev,
    /// 태그가 없거나 알 수 없는 값. **`Dev` 로 추측하지 않는다** —
    /// 프로덕션을 개발로 오분류하면 리터럴 정책이 느슨해진다.
    Unknown,
}

impl Env {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prd => "prd",
            Self::Stg => "stg",
            Self::Dev => "dev",
            Self::Unknown => "unknown",
        }
    }

    pub const ALL: [Env; 4] = [Self::Prd, Self::Stg, Self::Dev, Self::Unknown];

    /// 프로덕션으로 **취급해야 하는가**.
    ///
    /// `Unknown` 도 true 다. 프로덕션 안전 규칙과 같은 방향으로 기운다:
    /// 판단할 수 없으면 프로덕션으로 가정한다.
    pub fn treat_as_production(self) -> bool {
        matches!(self, Self::Prd | Self::Unknown)
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 어떤 태그 키를 환경 태그로 볼지. 대소문자를 구분하지 않는다.
pub const ENV_TAG_KEYS: &[&str] = &["env", "environment", "stage", "tier"];

/// 태그 값 → 환경 매핑 규칙.
///
/// 설정으로 덮어쓸 수 있어야 하므로 구조체로 둔다(FR-DSC-03 "매핑표 기반").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvMapping {
    /// 소문자 태그 값 → 환경.
    pub values: BTreeMap<String, Env>,
    /// 어떤 태그 키를 볼지. 순서가 우선순위다.
    pub keys: Vec<String>,
}

impl Default for EnvMapping {
    fn default() -> Self {
        let mut values = BTreeMap::new();
        for v in ["prd", "prod", "production", "live", "real"] {
            values.insert(v.to_string(), Env::Prd);
        }
        for v in ["stg", "stage", "staging", "qa", "uat"] {
            values.insert(v.to_string(), Env::Stg);
        }
        for v in [
            "dev",
            "develop",
            "development",
            "sandbox",
            "test",
            "local",
            "alpha",
            "beta",
        ] {
            values.insert(v.to_string(), Env::Dev);
        }
        Self {
            values,
            keys: ENV_TAG_KEYS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl EnvMapping {
    /// 태그에서 환경을 분류한다. 매칭되는 키가 없거나 값이 매핑에 없으면 `Unknown`.
    pub fn classify(&self, tags: &BTreeMap<String, String>) -> Env {
        // 태그 키를 소문자로 색인해 대소문자 차이를 흡수한다 (`Env` / `env` / `ENV`).
        let lower: BTreeMap<String, &String> =
            tags.iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
        for key in &self.keys {
            if let Some(raw) = lower.get(&key.to_lowercase()) {
                let v = raw.trim().to_lowercase();
                if let Some(env) = self.values.get(&v) {
                    return *env;
                }
            }
        }
        Env::Unknown
    }
}

/// 분류 결과 + 근거. UI 가 "태그 불일치"를 표시할 수 있어야 한다(FR-DSC-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvResolution {
    /// 실제로 적용되는 환경.
    pub effective: Env,
    /// 태그에서 유도한 값.
    pub from_tags: Env,
    /// 사용자가 UI 에서 지정한 값. 있으면 이쪽이 이긴다.
    pub override_value: Option<Env>,
}

impl EnvResolution {
    pub fn resolve(from_tags: Env, override_value: Option<Env>) -> Self {
        Self {
            effective: override_value.unwrap_or(from_tags),
            from_tags,
            override_value,
        }
    }

    /// 오버라이드가 태그와 어긋난다 — UI 에 배지를 표시한다.
    pub fn is_conflicting(&self) -> bool {
        self.override_value.is_some_and(|o| o != self.from_tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn classifies_common_tag_spellings() {
        let m = EnvMapping::default();
        assert_eq!(m.classify(&tags(&[("env", "prd")])), Env::Prd);
        assert_eq!(
            m.classify(&tags(&[("Environment", "Production")])),
            Env::Prd
        );
        assert_eq!(m.classify(&tags(&[("ENV", " STG ")])), Env::Stg);
        assert_eq!(m.classify(&tags(&[("environment", "dev")])), Env::Dev);
    }

    #[test]
    fn unknown_when_absent_or_unmapped() {
        let m = EnvMapping::default();
        assert_eq!(m.classify(&tags(&[])), Env::Unknown);
        assert_eq!(m.classify(&tags(&[("Name", "orders")])), Env::Unknown);
        assert_eq!(m.classify(&tags(&[("env", "권한없음")])), Env::Unknown);
    }

    #[test]
    fn unknown_is_treated_as_production() {
        // 오분류의 방향이 중요하다. prd 를 dev 로 보면 리터럴 정책이 느슨해진다.
        assert!(Env::Unknown.treat_as_production());
        assert!(Env::Prd.treat_as_production());
        assert!(!Env::Dev.treat_as_production());
        assert!(!Env::Stg.treat_as_production());
    }

    #[test]
    fn key_order_is_priority() {
        let mut m = EnvMapping::default();
        m.keys = vec!["tier".into(), "env".into()];
        assert_eq!(
            m.classify(&tags(&[("env", "dev"), ("tier", "prd")])),
            Env::Prd
        );
    }

    #[test]
    fn override_wins_and_conflict_is_visible() {
        let r = EnvResolution::resolve(Env::Dev, Some(Env::Prd));
        assert_eq!(r.effective, Env::Prd);
        assert!(r.is_conflicting());

        let same = EnvResolution::resolve(Env::Prd, Some(Env::Prd));
        assert!(!same.is_conflicting());

        let none = EnvResolution::resolve(Env::Stg, None);
        assert_eq!(none.effective, Env::Stg);
        assert!(!none.is_conflicting());
    }
}
