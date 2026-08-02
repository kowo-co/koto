//! Capability resolution. Denials are applied last and always win.

use koto_core::CoreError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const DEFAULT_ALLOW: &[&str] = &["input", "window", "spawn"];

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
    pub fn effective(&self, allow: &[String], deny: &[String]) -> BTreeSet<String> {
        let mut effective = self.allow.clone();
        effective.extend(allow.iter().cloned());
        effective.extend(self.deny.iter().map(|value| format!("!{value}")));
        for value in deny {
            effective.insert(format!("!{value}"));
        }
        let denied: Vec<_> = effective
            .iter()
            .filter_map(|value| value.strip_prefix('!').map(str::to_owned))
            .collect();
        for value in denied {
            effective.remove(&value);
        }
        effective.retain(|value| !value.starts_with('!'));
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
}
