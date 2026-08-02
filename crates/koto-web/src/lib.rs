//! Browser rung: a CDP engine over `--remote-debugging-pipe` and a BetterWright
//! engine driven through a node sidecar.
mod bw;
mod cdp;

pub use bw::{BwWorker, map_bw_error};
pub use cdp::Cdp;

use koto_core::CoreError;
use std::{path::PathBuf, sync::LazyLock, time::Duration};

pub enum WebEngine {
    Cdp(Cdp),
    Bw(BwWorker),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachSpec {
    LaunchDefault,
    LaunchBrowser(String),
    InheritedPipe(String),
    Bw {
        profile: Option<String>,
        session: Option<String>,
    },
}

static BW_KV: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^(profile|session)=(.+)$").unwrap());

pub fn parse_attach(args: &[String]) -> Result<AttachSpec, CoreError> {
    let Some(first) = args.first() else {
        return Ok(AttachSpec::LaunchDefault);
    };
    if first == "bw" || first == "betterwright" {
        let (mut profile, mut session) = (None, None);
        for arg in &args[1..] {
            let caps = BW_KV.captures(arg).ok_or_else(|| {
                CoreError::Parse(format!(
                    "web attach bw: expected profile=<p> or session=<s>, got `{arg}`"
                ))
            })?;
            let value = caps[2].to_owned();
            if &caps[1] == "profile" {
                profile = Some(value);
            } else {
                session = Some(value);
            }
        }
        return Ok(AttachSpec::Bw { profile, session });
    }
    if args.len() > 1 {
        return Err(CoreError::Parse(
            "web attach accepts at most one target".into(),
        ));
    }
    if first.starts_with("pid=") || first.starts_with("title") || first.starts_with("class") {
        Ok(AttachSpec::InheritedPipe(first.clone()))
    } else {
        Ok(AttachSpec::LaunchBrowser(first.clone()))
    }
}

pub fn attach(spec: AttachSpec) -> Result<WebEngine, CoreError> {
    Ok(match spec {
        AttachSpec::LaunchDefault => WebEngine::Cdp(Cdp::launch(None)?),
        AttachSpec::LaunchBrowser(browser) => WebEngine::Cdp(Cdp::launch(Some(&browser))?),
        AttachSpec::InheritedPipe(selector) => {
            WebEngine::Cdp(Cdp::attach_inherited(Some(&selector))?)
        }
        AttachSpec::Bw { profile, session } => {
            WebEngine::Bw(BwWorker::spawn(profile.as_deref(), session.as_deref())?)
        }
    })
}

impl WebEngine {
    pub fn action(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        match self {
            Self::Cdp(cdp) => match action {
                "shot" | "login" | "download" => Err(needs_bw(action)),
                _ => cdp.action(action, args, timeout),
            },
            Self::Bw(worker) => worker.action(action, args, timeout),
        }
    }
    pub fn is_bw(&self) -> bool {
        matches!(self, Self::Bw(_))
    }
    pub fn screenshot(&mut self, timeout: Duration) -> Result<PathBuf, CoreError> {
        match self {
            Self::Cdp(_) => Err(needs_bw("shot")),
            Self::Bw(worker) => worker.screenshot(timeout),
        }
    }
}
fn needs_bw(action: &str) -> CoreError {
    CoreError::Unsupported(format!(
        "web {action} needs the betterwright engine (web attach bw)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_owned()).collect()
    }
    #[test]
    fn attach_grammar_table() {
        assert_eq!(parse_attach(&[]).unwrap(), AttachSpec::LaunchDefault);
        assert_eq!(
            parse_attach(&args(&["firefox"])).unwrap(),
            AttachSpec::LaunchBrowser("firefox".into())
        );
        assert_eq!(
            parse_attach(&args(&["title~build"])).unwrap(),
            AttachSpec::InheritedPipe("title~build".into())
        );
        assert_eq!(
            parse_attach(&args(&["pid=42"])).unwrap(),
            AttachSpec::InheritedPipe("pid=42".into())
        );
        assert_eq!(
            parse_attach(&args(&["bw"])).unwrap(),
            AttachSpec::Bw {
                profile: None,
                session: None
            }
        );
        assert_eq!(
            parse_attach(&args(&["betterwright", "profile=work", "session=s1"])).unwrap(),
            AttachSpec::Bw {
                profile: Some("work".into()),
                session: Some("s1".into())
            }
        );
        assert!(matches!(
            parse_attach(&args(&["bw", "bogus=1"])),
            Err(CoreError::Parse(_))
        ));
        assert!(matches!(
            parse_attach(&args(&["bw", "profile="])),
            Err(CoreError::Parse(_))
        ));
        assert!(matches!(
            parse_attach(&args(&["a", "b"])),
            Err(CoreError::Parse(_))
        ));
    }
}
