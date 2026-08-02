//! Dedicated tmux backend.  `-L koto` keeps agent panes out of the user's tmux
//! server while retaining tmux's exact, scrollback-backed text observation.

use koto_core::CoreError;
use std::{
    collections::BTreeMap,
    process::Command,
    thread,
    time::{Duration, Instant},
};

pub struct Tmux {
    socket: String,
    session: String,
    panes: BTreeMap<String, String>,
    active: Option<String>,
}

impl Default for Tmux {
    fn default() -> Self {
        Self::new("koto", "default")
    }
}
impl Tmux {
    pub fn new(socket: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
            session: session.into(),
            panes: BTreeMap::new(),
            active: None,
        }
    }
    pub fn ensure(&self) -> Result<(), CoreError> {
        self.run(["start-server"])?;
        if self.run(["has-session", "-t", &self.session]).is_err() {
            self.run(["new-session", "-d", "-s", &self.session])?;
        }
        Ok(())
    }
    pub fn new_pane(&mut self, name: Option<&str>) -> Result<String, CoreError> {
        self.ensure()?;
        let id = self.run([
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &self.session,
        ])?;
        let name = name.unwrap_or(&id).to_owned();
        self.panes.insert(name.clone(), id);
        self.active = Some(name.clone());
        Ok(name)
    }
    pub fn send(&mut self, text: &str, enter: bool) -> Result<(), CoreError> {
        let target = self.target()?;
        self.run(["send-keys", "-t", &target, "-l", text])?;
        if enter {
            self.run(["send-keys", "-t", &target, "Enter"])?;
        }
        Ok(())
    }
    pub fn read(&mut self, lines: Option<usize>) -> Result<String, CoreError> {
        let target = self.target()?;
        self.capture(&target, lines)
    }
    /// Reads only a pane explicitly created or selected by the program. Unlike
    /// `read`, this never starts a tmux server as an observation side effect.
    pub fn read_active(&self, lines: Option<usize>) -> Result<String, CoreError> {
        let name = self
            .active
            .as_ref()
            .ok_or_else(|| CoreError::Backend("no koto pane is active".into()))?;
        let target = self
            .panes
            .get(name)
            .ok_or_else(|| CoreError::Backend("active pane disappeared".into()))?;
        self.capture(target, lines)
    }
    fn capture(&self, target: &str, lines: Option<usize>) -> Result<String, CoreError> {
        let mut args = vec!["capture-pane", "-p", "-J", "-t", target];
        let lines_string;
        if let Some(lines) = lines {
            lines_string = format!("-{}", lines);
            args.extend(["-S", &lines_string]);
        }
        self.run_vec(args)
    }
    pub fn wait(&mut self, pattern: &str, timeout: Duration) -> Result<(), CoreError> {
        let regex = regex::Regex::new(pattern.trim_start_matches('~'))
            .map_err(|error| CoreError::Parse(format!("invalid pane wait regex: {error}")))?;
        let start = Instant::now();
        loop {
            if regex.is_match(&self.read(None)?) {
                return Ok(());
            }
            if start.elapsed() >= timeout {
                return Err(CoreError::Timeout(format!("pane wait {pattern}")));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    pub fn kill(&mut self, name: Option<&str>) -> Result<(), CoreError> {
        let target = match name {
            Some(name) => self
                .panes
                .get(name)
                .cloned()
                .ok_or_else(|| CoreError::SelectorNotFound(name.into()))?,
            None => self.target()?,
        };
        self.run(["kill-pane", "-t", &target])?;
        self.panes.retain(|_, pane| pane != &target);
        if self
            .active
            .as_ref()
            .is_some_and(|name| !self.panes.contains_key(name))
        {
            self.active = self.panes.keys().next().cloned();
        }
        Ok(())
    }
    fn target(&mut self) -> Result<String, CoreError> {
        self.ensure()?;
        if self.active.is_none() {
            self.new_pane(Some("main"))?;
        }
        let name = self.active.as_ref().unwrap();
        self.panes
            .get(name)
            .cloned()
            .ok_or_else(|| CoreError::Backend("active pane disappeared".into()))
    }
    fn run<const N: usize>(&self, args: [&str; N]) -> Result<String, CoreError> {
        self.run_vec(args.into())
    }
    fn run_vec(&self, args: Vec<&str>) -> Result<String, CoreError> {
        let output = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .map_err(|error| CoreError::Backend(format!("tmux unavailable: {error}")))?;
        if !output.status.success() {
            return Err(CoreError::Backend(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_owned())
    }
}
