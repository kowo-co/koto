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
    write: UnixStream,
    read: BufReader<UnixStream>,
    next: u64,
    session: String,
}
/// The browser process itself, owned by the session holder. It hands out the
/// raw pipe ends; framing and protocol are the client's problem.
pub struct Browser {
    child: Child,
    write: Option<UnixStream>,
    read: Option<UnixStream>,
}
impl Browser {
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
                // No startup window means no restored tabs and no profile
                // surprises: the only window is the one created below, and it
                // is ours. A user profile with "continue where you left off"
                // would otherwise hand us somebody else's tab to drive.
                "--no-startup-window",
            ])
            .arg(format!("--user-data-dir={}", data_dir.display()))
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
        Ok(Self {
            child,
            write: Some(parent_write),
            read: Some(parent_read),
        })
    }
    pub fn writer(&mut self) -> Result<UnixStream, CoreError> {
        self.write
            .as_ref()
            .ok_or_else(|| CoreError::Backend("browser pipe is gone".into()))?
            .try_clone()
            .map_err(ioerr)
    }
    pub fn reader(&mut self) -> Result<UnixStream, CoreError> {
        self.read
            .as_ref()
            .ok_or_else(|| CoreError::Backend("browser pipe is gone".into()))?
            .try_clone()
            .map_err(ioerr)
    }
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}
impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Cdp {
    /// Joins the persistent session, adopting the page it already has open so
    /// a run continues where the last one stopped. Only opens a fresh tab when
    /// the session has none.
    pub fn connect(browser: Option<&str>) -> Result<Self, CoreError> {
        let stream = crate::session::connect_or_start(browser)?;
        let read = stream.try_clone().map_err(ioerr)?;
        // Answers to the previous client's last questions can still be in
        // flight. Swallow them before asking anything, and number requests
        // from a per-process base so a stale frame can never be mistaken for
        // this run's reply.
        read.set_read_timeout(Some(Duration::from_millis(120)))
            .map_err(ioerr)?;
        let mut drain = BufReader::new(read.try_clone().map_err(ioerr)?);
        let mut scratch = Vec::new();
        while drain.read_until(0, &mut scratch).is_ok_and(|count| count > 0) {
            scratch.clear();
        }
        read.set_read_timeout(Some(Duration::from_secs(15)))
            .map_err(ioerr)?;
        let mut cdp = Self {
            write: stream,
            read: BufReader::new(read),
            next: u64::from(std::process::id() % 2000) * 1000 + 1,
            session: String::new(),
        };
        let target = match cdp.existing_page()? {
            Some(id) => id,
            None => {
                // Nothing open means this session is new. Creating the first
                // window is also what prompts a profile to restore its old
                // tabs, so give them a moment to appear and then throw them
                // out: a fresh session starts with one tab, ours.
                let id = cdp.request(
                    "Target.createTarget",
                    json!({"url":"about:blank","newWindow":true}),
                    None,
                )?["result"]["targetId"]
                    .as_str()
                    .ok_or_else(|| CoreError::Backend("CDP did not create a target".into()))?
                    .to_owned();
                std::thread::sleep(Duration::from_millis(600));
                cdp.close_other_targets(&id)?;
                id
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
    fn close_other_targets(&mut self, keep: &str) -> Result<(), CoreError> {
        let targets = self.request("Target.getTargets", json!({}), None)?["result"]["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for id in targets
            .iter()
            .filter(|target| target["type"].as_str() == Some("page"))
            .filter_map(|target| target["targetId"].as_str())
            .filter(|id| *id != keep)
            .map(str::to_owned)
            .collect::<Vec<_>>()
        {
            let _ = self.request("Target.closeTarget", json!({"targetId":id}), None);
        }
        Ok(())
    }
    /// The page this session is already on, if any. A profile that restores
    /// old tabs can offer several; the most recently created one is the one a
    /// previous run was driving.
    fn existing_page(&mut self) -> Result<Option<String>, CoreError> {
        let targets = self.request("Target.getTargets", json!({}), None)?["result"]["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(targets
            .iter()
            .filter(|target| target["type"].as_str() == Some("page"))
            .filter_map(|target| target["targetId"].as_str())
            .next_back()
            .map(str::to_owned))
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
            "click" => { let selector = need(args,"web click")?; let find = locator(selector); self.evaluate(&format!("(()=>{{const e={find};if(!e)throw Error('selector not found');e.click();}})()"))?; Ok(None) }
            // React and friends install their own `value` setter and ignore a
            // plain assignment, so the text lands in the DOM and never reaches
            // the app's state: the form looks filled and saves empty. Drive the
            // prototype setter the way a keystroke would.
            "fill" => { let selector = need(args,"web fill")?; let value = args.get(1).ok_or_else(|| CoreError::Parse("web fill needs text".into()))?; let find = locator(selector); self.evaluate(&format!("(()=>{{const e={find};if(!e)throw Error('selector not found');const p=e instanceof HTMLTextAreaElement?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;const d=Object.getOwnPropertyDescriptor(p,'value');if(d&&d.set)d.set.call(e,{value:?});else e.value={value:?};e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));}})()"))?; Ok(None) }
            "wait" => { let selector = need(args,"web wait")?; let start=Instant::now(); while start.elapsed()<timeout { if self.evaluate(&format!("!!({})", locator(selector)))? == "true" { return Ok(None); } std::thread::sleep(Duration::from_millis(50)); } Err(CoreError::Timeout(format!("web wait {selector}"))) }
            _ => Err(CoreError::Parse(format!("unknown web action `{action}`"))),
        }
    }
    /// A thrown exception is a failed instruction, not a quiet `undefined`.
    /// CDP reports it in the envelope rather than as a protocol error, so it
    /// has to be dug out by hand or every miss looks like success.
    fn evaluate(&mut self, expression: &str) -> Result<String, CoreError> {
        let response = self.call(
            "Runtime.evaluate",
            json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
        )?;
        if let Some(details) = response["result"].get("exceptionDetails") {
            let message = details["exception"]["description"]
                .as_str()
                .or_else(|| details["text"].as_str())
                .unwrap_or("script threw");
            return Err(if message.contains("selector not found") {
                CoreError::SelectorNotFound(message.into())
            } else {
                CoreError::Backend(format!("web eval: {message}"))
            });
        }
        Ok(response["result"]["result"]["value"].to_string())
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
/// Resolves a web target to a JS expression. `text=` matches an element by the
/// words a person would read on it, which survives a class-name change and is
/// often the only handle a component library leaves behind. Anything else is
/// CSS.
fn locator(target: &str) -> String {
    match target.strip_prefix("text=") {
        Some(text) => {
            let text = text.trim();
            format!("Array.from(document.querySelectorAll('button,a,[role=button],input[type=submit],label,summary')).find(e=>(e.textContent||e.value||'').trim()==={text:?})")
        }
        None => format!("document.querySelector({target:?})"),
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
