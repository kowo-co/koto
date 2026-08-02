//! Capability policy resolution. Denials are applied last and always win.

use koto_core::CoreError;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs, path::Path};

pub const DEFAULT_ALLOW: &[&str] = &["input", "window", "spawn"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub allow: BTreeSet<String>,
    #[serde(default)]
    pub deny: BTreeSet<String>,
    pub budget_ops: Option<u32>,
    pub budget_time: Option<String>,
    pub seat: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub default: Profile,
    #[serde(default, rename = "profile")]
    pub profiles: std::collections::BTreeMap<String, Profile>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    pub allow: BTreeSet<String>,
    pub deny: BTreeSet<String>,
}

impl Policy {
    pub fn default_profile() -> Self {
        Self {
            allow: DEFAULT_ALLOW.iter().map(|value| (*value).into()).collect(),
            deny: BTreeSet::new(),
        }
    }
    pub fn load(path: &Path, profile: &str) -> Result<(Self, Profile), CoreError> {
        if !path.exists() {
            return Ok((Self::default_profile(), Profile::default()));
        }
        let source = fs::read_to_string(path)
            .map_err(|error| CoreError::Backend(format!("read {}: {error}", path.display())))?;
        let config: Config = toml::from_str(&source)
            .map_err(|error| CoreError::Parse(format!("{}: {error}", path.display())))?;
        let selected = config.profiles.get(profile).cloned().unwrap_or_default();
        let mut allow = if config.default.allow.is_empty() {
            Self::default_profile().allow
        } else {
            config.default.allow
        };
        allow.extend(selected.allow.iter().cloned());
        let mut deny = config.default.deny;
        deny.extend(selected.deny.iter().cloned());
        Ok((Self { allow, deny }, selected))
    }
    pub fn effective(&self, allow: &[String], deny: &[String]) -> BTreeSet<String> {
        let mut effective = self.allow.clone();
        effective.extend(allow.iter().cloned());
        let mut revoked = self.deny.clone();
        revoked.extend(deny.iter().cloned());
        for value in revoked {
            effective.remove(&value);
        }
        effective
    }
    pub fn require_all(
        &self,
        effective: &BTreeSet<String>,
        required: &[String],
    ) -> Result<(), CoreError> {
        required
            .iter()
            .find(|capability| !effective.contains(*capability))
            .map(|capability| CoreError::Capability(capability.clone()))
            .map_or(Ok(()), Err)
    }
}

pub fn default_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".config"))
                .unwrap_or_else(|| ".".into())
        })
        .join("koto/policy.toml")
}
pub fn split_capabilities(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denial_wins_over_cli_allow() {
        let policy = Policy {
            allow: ["web.eval".into()].into(),
            deny: ["web.eval".into()].into(),
        };
        assert!(
            !policy
                .effective(&["web.eval".into()], &[])
                .contains("web.eval")
        );
    }
    #[test]
    fn profile_merges_with_default() {
        let config: Config =
            toml::from_str("[default]\nallow=['web']\n[profile.work]\ndeny=['web']\n").unwrap();
        assert!(config.profiles.contains_key("work"));
    }
}
