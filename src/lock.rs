//! Stable lockfile models.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Lockfile {
    pub version: u32,
    pub okr_version: String,
    pub generated: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub config_hash: String,
    pub environment_digest: String,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
    #[serde(default, rename = "reference")]
    pub references: Vec<LockedReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub fetch_method: FetchMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,
    pub tree_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LockedReference {
    pub name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub fetch_method: FetchMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,
    pub tree_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FetchMethod {
    Tarball,
    ForgeTarball,
    Gh,
    GitClone,
}

#[cfg(test)]
mod tests {
    use super::{FetchMethod, LockedPackage, LockedReference, Lockfile};

    fn example_lock() -> Lockfile {
        Lockfile {
            version: 1,
            okr_version: "0.1.0".into(),
            generated: "2026-08-11T17:03:00Z".into(),
            snapshot: Some("2026-06-30".into()),
            config_hash: format!("sha256:{}", "a".repeat(64)),
            environment_digest: format!("sha256:{}", "b".repeat(64)),
            packages: vec![LockedPackage {
                name: "rpact".into(),
                version: "4.2.1".into(),
                source: "cran".into(),
                url: Some("https://example.test/rpact_4.2.1.tar.gz".into()),
                reference: None,
                commit: None,
                fetch_method: FetchMethod::Tarball,
                tarball_sha256: Some("c".repeat(64)),
                tree_digest: format!("sha256:{}", "d".repeat(64)),
                license: Some("LGPL-2.1".into()),
            }],
            references: vec![LockedReference {
                name: "standards".into(),
                source: "git::git@example.test:stds/standards.git".into(),
                url: None,
                reference: Some("main".into()),
                commit: Some("e".repeat(40)),
                fetch_method: FetchMethod::GitClone,
                tarball_sha256: Some("f".repeat(64)),
                tree_digest: format!("sha256:{}", "0".repeat(64)),
                license: None,
            }],
        }
    }

    #[test]
    fn lock_model_round_trips_all_fields() {
        let lock = example_lock();
        let encoded = toml::to_string(&lock).unwrap();
        let decoded: Lockfile = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, lock);
        assert!(encoded.contains("fetch-method = \"git-clone\""));
        assert!(encoded.contains("[[package]]"));
        assert!(encoded.contains("[[reference]]"));
    }

    #[test]
    fn lock_unknown_keys_are_rejected() {
        let encoded = toml::to_string(&example_lock()).unwrap();
        let malformed = encoded.replacen("version = 1", "version = 1\ntyop = true", 1);
        let error = toml::from_str::<Lockfile>(&malformed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
