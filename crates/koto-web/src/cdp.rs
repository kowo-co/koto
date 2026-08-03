//! Minimal managed Chromium CDP transport over `--remote-debugging-pipe`.
use koto_core::{CoreError, Selector, SelectorOperator};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    os::unix::{net::UnixStream, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Cdp {
    child: Option<Child>,
    write: UnixStream,
    read: BufReader<UnixStream>,
    next: u64,
    session: String,
}
impl Cdp {
    pub fn launch(browser: Option<&str>) -> Result<Self, CoreError> {
        let (parent_write, child_read) = UnixStream::pair().map_err(ioerr)?;
        let (child_write, parent_read) = UnixStream::pair().map_err(ioerr)?;
        let read_fd = child_read.as_raw_fd();
        let write_fd = child_write.as_raw_fd();
        let browser = browser
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(default_browser);
        let data_dir = chrome_data_dir();
        std::fs::create_dir_all(&data_dir)
            .map_err(|error| CoreError::Backend(format!("chrome data dir: {error}")))?;
        let mut command = Command::new(&browser);
        command
            .args([
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                // Unwrapped binaries (e.g. /opt/google/chrome/chrome) never read
                // the distro flags conf; without this they pick X11 under a
                // Wayland session and the compositor scales the buffer.
                "--ozone-platform-hint=auto",
            ])
            .arg(format!("--user-data-dir={}", data_dir.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(read_fd, 3) < 0 || libc::dup2(write_fd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| CoreError::Backend(format!("launch {browser}: {error}")))?;
        drop(child_read);
        drop(child_write);
        parent_read
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(ioerr)?;
        let mut cdp = Self {
            child: Some(child),
            write: parent_write,
            read: BufReader::new(parent_read),
            next: 1,
            session: String::new(),
        };
        // Chrome already opened a window for the startup URL. Adopt that one:
        // Target.createTarget would add a second window nobody asked for.
        let target = cdp.first_page_target()?;
        cdp.session = cdp.request(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
            None,
        )?["result"]["sessionId"]
            .as_str()
            .ok_or_else(|| CoreError::Backend("CDP did not attach to target".into()))?
            .to_owned();
        cdp.enable_page()
    }
    /// Attaches a live browser whose parent passed read/write pipe ends as
    /// fd 3 and fd 4, as Chromium specifies for `--remote-debugging-pipe`.
    /// Both descriptors are validated before ownership is taken. An optional
    /// selector picks the page target by title.
    pub fn attach_inherited(selector: Option<&str>) -> Result<Self, CoreError> {
        for fd in [3, 4] {
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(CoreError::Backend(
                    "web attach: no inherited CDP pipe on fd 3/4 (browser must be launched with --remote-debugging-pipe)".into(),
                ));
            }
        }
        unsafe { Self::attach_pipe(3, 4, selector) }
    }
    /// Attaches explicit inherited pipe descriptors, taking ownership of both
    /// immediately: the caller must not use them afterwards.
    pub unsafe fn attach_pipe(
        read_fd: i32,
        write_fd: i32,
        selector: Option<&str>,
    ) -> Result<Self, CoreError> {
        use std::os::fd::FromRawFd;
        let read = unsafe { UnixStream::from_raw_fd(read_fd) };
        let write = unsafe { UnixStream::from_raw_fd(write_fd) };
        read.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(ioerr)?;
        let mut cdp = Self {
            child: None,
            write,
            read: BufReader::new(read),
            next: 1,
            session: String::new(),
        };
        let targets = cdp.request("Target.getTargets", json!({}), None)?["result"]["targetInfos"]
            .as_array()
            .ok_or_else(|| CoreError::Backend("CDP returned no targets".into()))?
            .clone();
        let pages: Vec<&Value> = targets
            .iter()
            .filter(|target| target["type"].as_str() == Some("page"))
            .collect();
        let target = match selector {
            None => pages
                .first()
                .and_then(|target| target["targetId"].as_str())
                .ok_or_else(|| CoreError::Backend("CDP has no page target".into()))?
                .to_owned(),
            Some(raw) => {
                let parsed = Selector::parse(raw).map_err(CoreError::Parse)?;
                let mut matched = Vec::new();
                for target in &pages {
                    let title = target["title"].as_str().unwrap_or_default();
                    let mut keep = true;
                    for term in &parsed.terms {
                        if term.field != "title" {
                            return Err(CoreError::Unsupported(
                                "web attach matches on title only over CDP".into(),
                            ));
                        }
                        let hit = match term.operator {
                            SelectorOperator::Exact => title == term.value,
                            SelectorOperator::Regex => regex::Regex::new(&term.value)
                                .map_err(|error| CoreError::Parse(error.to_string()))?
                                .is_match(title),
                            SelectorOperator::Bare => false,
                        };
                        if !hit {
                            keep = false;
                            break;
                        }
                    }
                    if keep && let Some(id) = target["targetId"].as_str() {
                        matched.push(id.to_owned());
                    }
                }
                match matched.len() {
                    0 => return Err(CoreError::SelectorNotFound(raw.into())),
                    1 => matched.remove(0),
                    _ => return Err(CoreError::SelectorAmbiguous(raw.into())),
                }
            }
        };
        cdp.session = cdp.request(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
            None,
        )?["result"]["sessionId"]
            .as_str()
            .ok_or_else(|| CoreError::Backend("CDP did not attach to target".into()))?
            .to_owned();
        cdp.enable_page()
    }
    /// The startup window's page target. A freshly spawned browser may answer
    /// before it has one, so poll rather than race it.
    fn first_page_target(&mut self) -> Result<String, CoreError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let targets = self.request("Target.getTargets", json!({}), None)?["result"]
                ["targetInfos"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if let Some(id) = targets
                .iter()
                .find(|target| target["type"].as_str() == Some("page"))
                .and_then(|target| target["targetId"].as_str())
            {
                return Ok(id.to_owned());
            }
            if Instant::now() >= deadline {
                return Err(CoreError::Backend("browser opened no page target".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn enable_page(mut self) -> Result<Self, CoreError> {
        let session = self.session.clone();
        self.request("Page.enable", json!({}), Some(&session))?;
        Ok(self)
    }
    pub fn action(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        match action {
            "goto" => { self.call("Page.navigate", json!({"url": need(args, "web goto")?}))?; Ok(None) }
            "read" => Ok(Some(self.call("Accessibility.getFullAXTree", json!({"depth": -1}))?.to_string())),
            "eval" => Ok(Some(self.call("Runtime.evaluate", json!({"expression":need(args,"web eval")?,"returnByValue":true,"awaitPromise":true}))?["result"]["result"]["value"].to_string())),
            "click" => { let selector = need(args,"web click")?; self.evaluate(&format!("document.querySelector({selector:?})?.click()"))?; Ok(None) }
            "fill" => { let selector = need(args,"web fill")?; let value = args.get(1).ok_or_else(|| CoreError::Parse("web fill needs text".into()))?; self.evaluate(&format!("(()=>{{const e=document.querySelector({selector:?});if(!e)throw Error('selector not found');e.value={value:?};e.dispatchEvent(new Event('input',{{bubbles:true}}));}})()"))?; Ok(None) }
            "wait" => { let selector = need(args,"web wait")?; let start=Instant::now(); while start.elapsed()<timeout { if self.evaluate(&format!("!!document.querySelector({selector:?})"))? == "true" { return Ok(None); } std::thread::sleep(Duration::from_millis(50)); } Err(CoreError::Timeout(format!("web wait {selector}"))) }
            _ => Err(CoreError::Parse(format!("unknown web action `{action}`"))),
        }
    }
    fn evaluate(&mut self, expression: &str) -> Result<String, CoreError> {
        Ok(self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
        )?["result"]["result"]["value"]
            .to_string())
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CoreError> {
        let session = self.session.clone();
        self.request(method, params, Some(&session))
    }
    fn request(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, CoreError> {
        let id = self.next;
        self.next += 1;
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.into());
        }
        serde_json::to_writer(&mut self.write, &message)
            .map_err(|error| CoreError::Backend(error.to_string()))?;
        self.write.write_all(&[0]).map_err(ioerr)?;
        self.write.flush().map_err(ioerr)?;
        loop {
            let mut line = Vec::new();
            self.read.read_until(0, &mut line).map_err(|error| {
                if error.kind() == std::io::ErrorKind::TimedOut {
                    CoreError::Timeout(format!("CDP {method}"))
                } else {
                    ioerr(error)
                }
            })?;
            if line.last() == Some(&0) {
                line.pop();
            }
            let value: Value = serde_json::from_slice(&line)
                .map_err(|error| CoreError::Backend(format!("invalid CDP response: {error}")))?;
            if value["id"].as_u64() == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(CoreError::Backend(format!("CDP {method}: {error}")));
                }
                return Ok(value);
            }
        }
    }
}
/// A browser this process launched dies with the program; the pipe closing
/// does not make Chromium exit on its own, and orphaned windows pile up run
/// after run. An inherited pipe belongs to someone else's browser and is
/// left alone.
impl Drop for Cdp {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        let _ = self.request("Browser.close", json!({}), None);
        if let Some(child) = self.child.as_mut() {
            for _ in 0..20 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    _ => std::thread::sleep(Duration::from_millis(100)),
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn default_browser() -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join("google-chrome-stable"))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    });
    if found {
        "google-chrome-stable".into()
    } else {
        "chromium".into()
    }
}
fn chrome_data_dir() -> PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(state).join("koto/chrome")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".local/state/koto/chrome")
    }
}
fn need<'a>(args: &'a [String], name: &str) -> Result<&'a str, CoreError> {
    args.first()
        .map(String::as_str)
        .ok_or_else(|| CoreError::Parse(format!("{name} needs an argument")))
}
fn ioerr(error: std::io::Error) -> CoreError {
    CoreError::Backend(format!("CDP pipe: {error}"))
}
