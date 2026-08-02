//! Hyprland IPC backend.
//!
//! State is read from `hyprctl -j`; mutations use Hyprland's command socket via
//! `hyprctl dispatch`.  Input injection is intentionally isolated here so it can
//! be replaced by the direct virtual-keyboard implementation without changing
//! basm or the VM.

use koto_core::{Backend, CoreError, Observation, ObserveMode, Selector, SelectorOperator, Wait};
use serde::{Deserialize, Serialize};
use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub focus_history_id: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

#[derive(Default)]
pub struct HyprBackend;
impl HyprBackend {
    pub fn available() -> bool {
        Command::new("hyprctl")
            .arg("-j")
            .arg("clients")
            .output()
            .is_ok_and(|o| o.status.success())
    }
    pub fn windows(&self) -> Result<Vec<Window>, CoreError> {
        let output = hyprctl(["-j", "clients"])?;
        serde_json::from_str(&output)
            .map_err(|e| CoreError::Backend(format!("invalid hyprctl clients JSON: {e}")))
    }
    pub fn resolve(&self, raw: &str) -> Result<Window, CoreError> {
        let selector = Selector::parse(raw).map_err(CoreError::Backend)?;
        let matches: Vec<_> = self
            .windows()?
            .into_iter()
            .filter(|window| {
                selector.terms.iter().all(|term| match term.field.as_str() {
                    "class" => compare(&window.class, term),
                    "title" => compare(&window.title, term),
                    "addr" => compare(&window.address, term),
                    "pid" => compare(&window.pid.to_string(), term),
                    "ws" => {
                        compare(&window.workspace.id.to_string(), term)
                            || compare(&window.workspace.name, term)
                    }
                    "focused" => window.focus_history_id == 0,
                    "last" => window.focus_history_id == 1,
                    _ => false,
                })
            })
            .collect();
        match matches.len() {
            0 => Err(CoreError::SelectorNotFound(raw.into())),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(CoreError::SelectorAmbiguous(raw.into())),
        }
    }
    fn dispatch(&self, command: &str) -> Result<(), CoreError> {
        hyprctl(["dispatch", command]).map(|_| ())
    }
}
fn compare(actual: &str, term: &koto_core::SelectorTerm) -> bool {
    match term.operator {
        SelectorOperator::Exact => actual == term.value,
        SelectorOperator::Regex => {
            regex::Regex::new(&term.value).is_ok_and(|regex| regex.is_match(actual))
        }
        SelectorOperator::Bare => true,
    }
}
fn hyprctl<const N: usize>(args: [&str; N]) -> Result<String, CoreError> {
    let output = Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|e| CoreError::Backend(format!("Hyprland IPC unavailable: {e}")))?;
    if !output.status.success() {
        return Err(CoreError::Backend(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

impl Backend for HyprBackend {
    fn key(&mut self, keys: &[String]) -> Result<(), CoreError> {
        if keys.is_empty() {
            return Err(CoreError::Backend("empty chord".into()));
        }
        let key = keys.last().unwrap();
        let mods = keys[..keys.len() - 1]
            .iter()
            .map(|key| match key.as_str() {
                "super" => "SUPER",
                "ctrl" => "CTRL",
                "alt" => "ALT",
                "shift" => "SHIFT",
                other => other,
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.dispatch(&format!("sendshortcut {mods}, {key}"))
    }
    fn text(&mut self, text: &str, paste: bool) -> Result<(), CoreError> {
        if paste {
            let mut child = Command::new("wl-copy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| CoreError::Backend(format!("wl-copy unavailable: {e}")))?;
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(text.as_bytes())
                .map_err(|e| CoreError::Backend(e.to_string()))?;
            if !child.wait().is_ok_and(|s| s.success()) {
                return Err(CoreError::Backend("wl-copy failed".into()));
            }
            return self.key(&["ctrl".into(), "v".into()]);
        }
        for character in text.chars() {
            self.dispatch(&format!("sendkeystate , {character}, down"))?;
            self.dispatch(&format!("sendkeystate , {character}, up"))?;
        }
        Ok(())
    }
    fn wait(&mut self, wait: &Wait, default_timeout: Duration) -> Result<(), CoreError> {
        let timeout = wait
            .timeout
            .as_deref()
            .map(parse_duration)
            .transpose()?
            .unwrap_or(default_timeout);
        match wait.kind.as_str() {
            "duration" => {
                thread::sleep(parse_duration(&wait.value)?);
                Ok(())
            }
            "window" => poll(timeout, || self.resolve(&wait.value).map(|_| ())),
            "gone" => poll(timeout, || match self.resolve(&wait.value) {
                Err(CoreError::SelectorNotFound(_)) => Ok(()),
                Ok(_) => Err(CoreError::Backend("window still exists".into())),
                Err(e) => Err(e),
            }),
            "idle" => {
                thread::sleep(parse_duration(&wait.value)?);
                Ok(())
            }
            _ => Err(CoreError::Unsupported(format!("wait {}", wait.kind))),
        }
    }
    fn focus(&mut self, selector: &str) -> Result<(), CoreError> {
        let window = self.resolve(selector)?;
        self.dispatch(&format!("focuswindow address:{}", window.address))
    }
    fn observe(&mut self, _mode: ObserveMode) -> Result<Observation, CoreError> {
        let focused = self.resolve("focused")?;
        Ok(Observation {
            source: "hypr".into(),
            fidelity: "metadata".into(),
            text: Some(format!(
                "class={} title={} ws={} addr={}",
                focused.class, focused.title, focused.workspace.id, focused.address
            )),
            image: None,
        })
    }
    fn list(&mut self, subject: &str) -> Result<String, CoreError> {
        match subject {
            "windows" | "clients" => hyprctl(["-j", "clients"]),
            "workspaces" => hyprctl(["-j", "workspaces"]),
            "monitors" => hyprctl(["-j", "monitors"]),
            "devices" => hyprctl(["-j", "devices"]),
            _ => Err(CoreError::Unsupported(format!("list {subject}"))),
        }
    }
}
fn poll<F>(timeout: Duration, mut check: F) -> Result<(), CoreError>
where
    F: FnMut() -> Result<(), CoreError>,
{
    let started = Instant::now();
    loop {
        match check() {
            Ok(()) => return Ok(()),
            Err(CoreError::SelectorNotFound(_)) | Err(CoreError::Backend(_))
                if started.elapsed() < timeout =>
            {
                thread::sleep(Duration::from_millis(50))
            }
            Err(error) => return Err(error),
        }
        if started.elapsed() >= timeout {
            return Err(CoreError::Timeout(format!(
                "wait {} {}",
                "condition",
                timeout.as_millis()
            )));
        }
    }
}
fn parse_duration(value: &str) -> Result<Duration, CoreError> {
    let (number, multiplier) = if let Some(v) = value.strip_suffix("ms") {
        (v, 1_u64)
    } else if let Some(v) = value.strip_suffix('s') {
        (v, 1_000)
    } else if let Some(v) = value.strip_suffix('m') {
        (v, 60_000)
    } else {
        return Err(CoreError::Backend(format!("invalid duration `{value}`")));
    };
    let millis = number
        .parse::<u64>()
        .map_err(|_| CoreError::Backend(format!("invalid duration `{value}`")))?
        .checked_mul(multiplier)
        .ok_or_else(|| CoreError::Backend("duration overflow".into()))?;
    Ok(Duration::from_millis(millis))
}
