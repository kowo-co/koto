//! Minimal managed Chromium CDP transport over `--remote-debugging-pipe`.
use koto_core::CoreError;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    os::unix::{net::UnixStream, process::CommandExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Cdp {
    _child: Child,
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
            .unwrap_or("chromium");
        let mut command = Command::new(browser);
        command
            .args([
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "about:blank",
            ])
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
            _child: child,
            write: parent_write,
            read: BufReader::new(parent_read),
            next: 1,
            session: String::new(),
        };
        let target = cdp.request("Target.createTarget", json!({"url":"about:blank"}), None)?["result"]["targetId"].as_str().ok_or_else(|| CoreError::Backend("CDP did not create a target".into()))?.to_owned();
        cdp.session = cdp.request(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
            None,
        )?["result"]["sessionId"]
            .as_str()
            .ok_or_else(|| CoreError::Backend("CDP did not attach to target".into()))?
            .to_owned();
        let session = cdp.session.clone();
        cdp.request("Page.enable", json!({}), Some(&session))?;
        Ok(cdp)
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
fn need<'a>(args: &'a [String], name: &str) -> Result<&'a str, CoreError> {
    args.first()
        .map(String::as_str)
        .ok_or_else(|| CoreError::Parse(format!("{name} needs an argument")))
}
fn ioerr(error: std::io::Error) -> CoreError {
    CoreError::Backend(format!("CDP pipe: {error}"))
}
