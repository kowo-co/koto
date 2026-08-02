//! Hyprland IPC backend.
//!
//! State is read from `hyprctl -j`; mutations use Hyprland's command socket via
//! `hyprctl dispatch`.  Input injection is intentionally isolated here so it can
//! be replaced by the direct virtual-keyboard implementation without changing
//! basm or the VM.

use koto_core::{
    Backend, CoreError, Observation, ObserveMode, Selector, SelectorOperator, Wait, WindowRecord,
};
use koto_input::InputBackend;
use serde::{Deserialize, Serialize};
use std::{
    io::{ErrorKind, Read},
    os::unix::net::UnixStream,
    path::PathBuf,
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
    // Hyprland spells this `focusHistoryID`. Without the rename serde never
    // finds it, `default` makes it 0 for every window, and `focused` then
    // matches all of them while `last` matches none.
    #[serde(rename = "focusHistoryID", default)]
    pub focus_history_id: i64,
    #[serde(default)]
    pub at: [i32; 2],
    #[serde(default)]
    pub size: [i32; 2],
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MonitorGeometry {
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

pub struct HyprBackend {
    input: Option<InputBackend>,
}
impl Default for HyprBackend {
    fn default() -> Self {
        Self { input: None }
    }
}
impl HyprBackend {
    fn input(&mut self) -> Result<&mut InputBackend, CoreError> {
        if self.input.is_none() {
            self.input = Some(
                InputBackend::connect().map_err(|error| CoreError::Backend(error.to_string()))?,
            );
        }
        Ok(self.input.as_mut().unwrap())
    }
    pub fn release_held(&mut self) {
        if let Some(input) = &self.input {
            let _ = input.release_all();
        }
    }
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
    pub fn resolve_all(&self, raw: &str) -> Result<Vec<Window>, CoreError> {
        let selector = Selector::parse(raw).map_err(CoreError::Backend)?;
        Ok(self
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
            .collect())
    }
    pub fn resolve(&self, raw: &str) -> Result<Window, CoreError> {
        let matches = self.resolve_all(raw)?;
        match matches.len() {
            0 => Err(CoreError::SelectorNotFound(raw.into())),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(CoreError::SelectorAmbiguous(raw.into())),
        }
    }
    fn dispatch(&self, command: &str) -> Result<(), CoreError> {
        hyprctl(["dispatch", command]).map(|_| ())
    }
    fn pointer_coordinates(&self, window: &Window) -> Result<(u32, u32, u32, u32), CoreError> {
        let monitors: Vec<MonitorGeometry> = serde_json::from_str(&hyprctl(["-j", "monitors"])?)
            .map_err(|error| {
                CoreError::Backend(format!("invalid hyprctl monitors JSON: {error}"))
            })?;
        let left = monitors
            .iter()
            .map(|monitor| monitor.x)
            .min()
            .ok_or_else(|| CoreError::Backend("no monitors".into()))?;
        let top = monitors
            .iter()
            .map(|monitor| monitor.y)
            .min()
            .ok_or_else(|| CoreError::Backend("no monitors".into()))?;
        let right = monitors
            .iter()
            .map(|monitor| monitor.x + monitor.width)
            .max()
            .unwrap();
        let bottom = monitors
            .iter()
            .map(|monitor| monitor.y + monitor.height)
            .max()
            .unwrap();
        let x = window.at[0] + window.size[0] / 2 - left;
        let y = window.at[1] + window.size[1] / 2 - top;
        if x < 0 || y < 0 || right <= left || bottom <= top {
            return Err(CoreError::Backend("invalid window geometry".into()));
        }
        Ok((
            x as u32,
            y as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        ))
    }
    fn event_socket() -> Result<UnixStream, CoreError> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| CoreError::Backend("XDG_RUNTIME_DIR is unset".into()))?;
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
            .ok_or_else(|| CoreError::Backend("HYPRLAND_INSTANCE_SIGNATURE is unset".into()))?;
        UnixStream::connect(
            PathBuf::from(runtime)
                .join("hypr")
                .join(signature)
                .join(".socket2.sock"),
        )
        .map_err(|error| CoreError::Backend(format!("Hyprland event socket unavailable: {error}")))
    }
    fn wait_idle(&self, quiet: Duration, timeout: Duration) -> Result<(), CoreError> {
        let mut socket = Self::event_socket()?;
        let started = Instant::now();
        loop {
            if started.elapsed() >= timeout {
                return Err(CoreError::Timeout("wait idle".into()));
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            socket
                .set_read_timeout(Some(quiet.min(remaining)))
                .map_err(|error| CoreError::Backend(error.to_string()))?;
            let mut byte = [0_u8; 1];
            match socket.read(&mut byte) {
                Ok(0) => return Err(CoreError::Backend("Hyprland event socket closed".into())),
                Ok(_) => continue,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(());
                }
                Err(error) => {
                    return Err(CoreError::Backend(format!(
                        "Hyprland event socket: {error}"
                    )));
                }
            }
        }
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

/// Separates a program's mistake from an environment failure.
///
/// An unknown key name is the caller's error and is fixable by rewriting the
/// instruction; an unreachable compositor is not. Reporting both as `Backend`
/// tells the caller the machine is broken when it merely misspelled a key,
/// which is the difference between "retry differently" and "give up".
fn input_error(error: koto_input::InputError) -> CoreError {
    match error {
        koto_input::InputError::Key(_) => CoreError::Parse(error.to_string()),
        koto_input::InputError::Unavailable(_) => CoreError::Backend(error.to_string()),
    }
}

impl Backend for HyprBackend {
    fn key(&mut self, keys: &[String]) -> Result<(), CoreError> {
        self.input()?.chord(keys).map_err(input_error)
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
        if text.len() > 200 {
            return self.text(text, true);
        }
        self.input()?.text(text).map_err(input_error)
    }
    fn key_state(&mut self, keys: &[String], pressed: bool) -> Result<(), CoreError> {
        let input = self.input()?;
        for key in keys {
            input.key(key, pressed).map_err(input_error)?;
        }
        Ok(())
    }
    fn pointer(&mut self, action: &str, args: &[String]) -> Result<(), CoreError> {
        match action {
            "click" => {
                let selector = args
                    .first()
                    .ok_or_else(|| CoreError::Parse("click needs a selector".into()))?;
                let window = self.resolve(selector)?;
                let (x, y, width, height) = self.pointer_coordinates(&window)?;
                self.dispatch(&format!("focuswindow address:{}", window.address))?;
                let input = self.input()?;
                input
                    .move_absolute(x, y, width, height)
                    .map_err(|error| CoreError::Backend(error.to_string()))?;
                input
                    .click_primary()
                    .map_err(|error| CoreError::Backend(error.to_string()))
            }
            "scroll" => {
                let direction = args
                    .first()
                    .ok_or_else(|| CoreError::Parse("scroll needs a direction".into()))?;
                let count = args
                    .get(1)
                    .ok_or_else(|| CoreError::Parse("scroll needs a count".into()))?
                    .parse::<i32>()
                    .map_err(|_| CoreError::Parse("scroll count must be an integer".into()))?;
                let (vertical, sign) = match direction.as_str() {
                    "up" => (true, 1),
                    "down" => (true, -1),
                    "left" => (false, -1),
                    "right" => (false, 1),
                    _ => return Err(CoreError::Parse("invalid scroll direction".into())),
                };
                self.input()?
                    .scroll(vertical, sign * count)
                    .map_err(|error| CoreError::Backend(error.to_string()))
            }
            _ => Err(CoreError::Unsupported(format!("pointer {action}"))),
        }
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
            "title" => poll(timeout, || {
                let pattern = regex::Regex::new(&wait.value)
                    .map_err(|error| CoreError::Parse(format!("invalid title regex: {error}")))?;
                if self
                    .windows()?
                    .iter()
                    .any(|window| pattern.is_match(&window.title))
                {
                    Ok(())
                } else {
                    Err(CoreError::SelectorNotFound(format!("title~{}", wait.value)))
                }
            }),
            "exit" => poll(timeout, || {
                let pid = wait
                    .value
                    .parse::<u32>()
                    .map_err(|_| CoreError::Parse("wait exit needs a PID".into()))?;
                if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    Ok(())
                } else {
                    Err(CoreError::Backend(format!("pid {pid} is still running")))
                }
            }),
            "idle" => self.wait_idle(parse_duration(&wait.value)?, timeout),
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
    fn window(&mut self, action: &str, args: &[String]) -> Result<(), CoreError> {
        let command = match action {
            "ws" => format!(
                "workspace {}",
                args.first()
                    .ok_or_else(|| CoreError::Parse("ws needs a workspace".into()))?
            ),
            "send" => format!(
                "movetoworkspace {}",
                args.first()
                    .ok_or_else(|| CoreError::Parse("send needs a workspace".into()))?
            ),
            "close" => match args.first() {
                Some(selector) => {
                    let windows = self.resolve_all(selector)?;
                    if windows.is_empty() {
                        return Err(CoreError::SelectorNotFound(selector.clone()));
                    }
                    for window in windows {
                        self.dispatch(&format!("closewindow address:{}", window.address))?;
                    }
                    return Ok(());
                }
                None => "killactive".into(),
            },
            "float" => "togglefloating".into(),
            "tile" => "settiled".into(),
            "full" => "fullscreen".into(),
            "pin" => "pin".into(),
            "swap" => format!(
                "swapwindow {}",
                args.first()
                    .ok_or_else(|| CoreError::Parse("swap needs a direction".into()))?
            ),
            "move" => format!(
                "movewindow {}",
                args.first()
                    .ok_or_else(|| CoreError::Parse("move needs a direction".into()))?
            ),
            "monitor" => format!(
                "focusmonitor {}",
                args.first()
                    .ok_or_else(|| CoreError::Parse("monitor needs a name".into()))?
            ),
            _ => return Err(CoreError::Unsupported(format!("window {action}"))),
        };
        self.dispatch(&command)
    }
    fn checkpoint(&mut self, name: &str, rollback: bool) -> Result<(), CoreError> {
        if name.is_empty() || name.contains('/') || name.contains("..") {
            return Err(CoreError::Parse(
                "checkpoint name must be a simple name".into(),
            ));
        }
        let source = std::env::var_os("KOTO_BTRFS_SUBVOLUME")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| {
                CoreError::Backend(
                    "set KOTO_BTRFS_SUBVOLUME to the btrfs subvolume to checkpoint".into(),
                )
            })?;
        let snapshots = std::env::var_os("KOTO_BTRFS_SNAPSHOT_DIR")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| {
                CoreError::Backend(
                    "set KOTO_BTRFS_SNAPSHOT_DIR to a sibling btrfs snapshot directory".into(),
                )
            })?;
        let snapshot = snapshots.join(name);
        if !rollback {
            std::fs::create_dir_all(&snapshots).map_err(|error| {
                CoreError::Backend(format!("create checkpoint directory: {error}"))
            })?;
            let status = Command::new("btrfs")
                .args(["subvolume", "snapshot", "-r"])
                .arg(&source)
                .arg(&snapshot)
                .status()
                .map_err(|error| CoreError::Backend(format!("btrfs unavailable: {error}")))?;
            return if status.success() {
                Ok(())
            } else {
                Err(CoreError::Backend(format!(
                    "could not create checkpoint {}",
                    snapshot.display()
                )))
            };
        }
        if std::env::var_os("KOTO_BTRFS_ALLOW_ROLLBACK").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return Err(CoreError::Backend("rollback requires KOTO_BTRFS_ALLOW_ROLLBACK=1; it replaces the configured subvolume".into()));
        }
        if !snapshot.exists() {
            return Err(CoreError::SelectorNotFound(format!("checkpoint {name}")));
        }
        let delete = Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(&source)
            .status()
            .map_err(|error| CoreError::Backend(format!("btrfs unavailable: {error}")))?;
        if !delete.success() {
            return Err(CoreError::Backend(format!(
                "could not remove subvolume {}",
                source.display()
            )));
        }
        let restore = Command::new("btrfs")
            .args(["subvolume", "snapshot"])
            .arg(&snapshot)
            .arg(&source)
            .status()
            .map_err(|error| CoreError::Backend(format!("btrfs unavailable: {error}")))?;
        if restore.success() {
            Ok(())
        } else {
            Err(CoreError::Backend(format!(
                "could not restore checkpoint {name}"
            )))
        }
    }
    fn kill(&mut self, selector: &str) -> Result<(), CoreError> {
        if let Some(scope) = selector.strip_prefix("scope=") {
            let status = Command::new("systemctl")
                .args(["--user", "kill", scope])
                .status()
                .map_err(|error| CoreError::Backend(format!("systemctl unavailable: {error}")))?;
            return if status.success() {
                Ok(())
            } else {
                Err(CoreError::Backend(format!("could not kill scope {scope}")))
            };
        }
        let windows = self.resolve_all(selector)?;
        if windows.is_empty() {
            return Err(CoreError::SelectorNotFound(selector.into()));
        }
        for window in windows {
            let status = Command::new("kill")
                .args(["-TERM", &window.pid.to_string()])
                .status()
                .map_err(|error| CoreError::Backend(format!("kill unavailable: {error}")))?;
            if !status.success() {
                return Err(CoreError::Backend(format!(
                    "could not kill pid {}",
                    window.pid
                )));
            }
        }
        Ok(())
    }
    fn selector_count(&mut self, selector: &str) -> Result<usize, CoreError> {
        Ok(self.resolve_all(selector)?.len())
    }
    fn focused_window(&mut self) -> Result<WindowRecord, CoreError> {
        let window = self.resolve("focused")?;
        Ok(WindowRecord {
            class: window.class,
            addr: window.address,
            ws: window.workspace.id,
            title: window.title,
            pid: window.pid,
        })
    }
    fn metadata(&mut self, field: &str) -> Result<String, CoreError> {
        let window = self.resolve("focused")?;
        match field {
            "title" => Ok(window.title),
            "class" => Ok(window.class),
            "addr" => Ok(window.address),
            "pid" => Ok(window.pid.to_string()),
            "ws" => Ok(window.workspace.id.to_string()),
            _ => Err(CoreError::Unsupported(format!("peek {field}"))),
        }
    }
    fn spawn(&mut self, command: &[String]) -> Result<String, CoreError> {
        if command.is_empty() {
            return Err(CoreError::Parse("spawn needs a command".into()));
        }
        let has_uwsm = Command::new("uwsm").arg("--version").output().is_ok();
        let mut process = if has_uwsm {
            let mut process = Command::new("uwsm");
            process.arg("app").arg("--").args(command);
            process
        } else {
            let mut process = Command::new(&command[0]);
            process.args(&command[1..]);
            process
        };
        let child = process
            .spawn()
            .map_err(|error| CoreError::Backend(format!("spawn {}: {error}", command[0])))?;
        Ok(serde_json::json!({"pid": child.id(), "scope": serde_json::Value::Null}).to_string())
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
