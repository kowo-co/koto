//! BetterWright engine: a node sidecar driven over NDJSON on stdio.
use koto_core::CoreError;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::LazyLock,
    sync::mpsc::{Receiver, RecvTimeoutError, channel},
    time::{Duration, Instant},
};

const INSTALL_HINT: &str =
    "betterwright not installed: npm i -g betterwright or set KOTO_BETTERWRIGHT_DIR";
const READY: Duration = Duration::from_secs(15);

pub struct BwWorker {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next: u64,
}

#[derive(Debug)]
struct Envelope {
    ok: bool,
    result: Value,
    artifacts: Vec<String>,
    challenges: Vec<Value>,
    error: Option<String>,
}
impl Envelope {
    fn parse(value: &Value) -> Self {
        Self {
            ok: value["ok"].as_bool().unwrap_or(false),
            result: value.get("result").cloned().unwrap_or(Value::Null),
            artifacts: value["artifacts"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            challenges: value["challenges"].as_array().cloned().unwrap_or_default(),
            error: value["error"].as_str().map(str::to_owned),
        }
    }
}

impl BwWorker {
    pub fn spawn(profile: Option<&str>, session: Option<&str>) -> Result<Self, CoreError> {
        let dir = runtime_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| CoreError::Backend(format!("sidecar dir: {error}")))?;
        let path = dir.join("bw-sidecar-v1.mjs");
        std::fs::write(&path, include_str!("sidecar.mjs"))
            .map_err(|error| CoreError::Backend(format!("sidecar write: {error}")))?;
        let path = path.to_string_lossy().into_owned();
        Self::spawn_with_program("node", &[&path], profile, session)
    }
    pub(crate) fn spawn_with_program(
        program: &str,
        args: &[&str],
        profile: Option<&str>,
        session: Option<&str>,
    ) -> Result<Self, CoreError> {
        let dir = runtime_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| CoreError::Backend(format!("sidecar dir: {error}")))?;
        let log_path = dir.join("bw-sidecar.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| CoreError::Backend(format!("sidecar log: {error}")))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|_| {
                CoreError::Backend(
                    "node not found: the betterwright engine requires node >= 22".into(),
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Backend("sidecar stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Backend("sidecar stdout unavailable".into()))?;
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut worker = Self {
            child,
            stdin,
            lines,
            next: 1,
        };
        loop {
            match worker.lines.recv_timeout(READY) {
                Ok(line) => {
                    let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if value["event"].as_str() == Some("ready") {
                        break;
                    }
                    if value["error"].as_str() == Some("module-not-found") {
                        let _ = worker.child.kill();
                        return Err(CoreError::Backend(INSTALL_HINT.into()));
                    }
                }
                Err(_) => {
                    let _ = worker.child.kill();
                    return Err(CoreError::Backend(format!(
                        "betterwright sidecar failed to start; see {}",
                        log_path.display()
                    )));
                }
            }
        }
        let mut init = json!({"op":"init"});
        if let Some(profile) = profile {
            init["profile"] = Value::String(profile.into());
        }
        if let Some(session) = session {
            init["session"] = Value::String(session.into());
        }
        worker.call(init, READY)?;
        Ok(worker)
    }
    fn call(&mut self, mut payload: Value, deadline: Duration) -> Result<Envelope, CoreError> {
        let id = self.next;
        self.next += 1;
        payload["id"] = json!(id);
        let op = payload["op"].as_str().unwrap_or("?").to_owned();
        let line = format!("{payload}\n");
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|error| CoreError::Backend(format!("betterwright {op}: {error}")))?;
        let start = Instant::now();
        loop {
            let left = deadline.saturating_sub(start.elapsed());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if value["id"].as_u64() != Some(id) {
                        continue;
                    }
                    return Ok(Envelope::parse(&value));
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ = self.child.kill();
                    return Err(CoreError::Timeout(format!("betterwright {op}")));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CoreError::Backend(format!(
                        "betterwright sidecar closed during {op}"
                    )));
                }
            }
        }
    }
    fn run_snippet(
        &mut self,
        code: String,
        timeout: Duration,
        approved: Option<Vec<String>>,
    ) -> Result<Envelope, CoreError> {
        let mut payload = json!({"op":"run","code":code,"timeout_ms":timeout.as_millis() as u64});
        if let Some(approved) = approved {
            payload["approved_downloads"] = json!(approved);
        }
        let envelope = self.call(payload, timeout + Duration::from_secs(5))?;
        if !envelope.ok {
            return Err(map_bw_error(
                envelope.error.as_deref().unwrap_or("betterwright run failed"),
            ));
        }
        Ok(envelope)
    }
    pub fn action(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        match action {
            "goto" => {
                let url = need(args, "web goto")?;
                let envelope =
                    self.run_snippet(format!("await page.goto({})", json_str(url)), timeout, None)?;
                Ok(trailer(&envelope, None))
            }
            "read" => {
                let code = if args.first().map(String::as_str) == Some("full") {
                    "return await snapshot({diff:false, maxChars:30000})"
                } else {
                    "return await snapshot({interactive:true})"
                };
                let envelope = self.run_snippet(code.into(), timeout, None)?;
                let text = result_text(&envelope.result);
                Ok(trailer(&envelope, Some(text)))
            }
            "click" => {
                let target = need(args, "web click")?;
                let envelope =
                    self.run_snippet(format!("await {}.click()", locator(target)), timeout, None)?;
                Ok(trailer(&envelope, None))
            }
            "fill" => {
                let target = need(args, "web fill")?;
                let text = args
                    .get(1)
                    .ok_or_else(|| CoreError::Parse("web fill needs text".into()))?;
                let envelope = self.run_snippet(
                    format!("await {}.fill({})", locator(target), json_str(text)),
                    timeout,
                    None,
                )?;
                Ok(trailer(&envelope, None))
            }
            "wait" => {
                let target = need(args, "web wait")?;
                let envelope = self.run_snippet(
                    format!(
                        "await {}.waitFor({{timeout:{}}})",
                        locator(target),
                        timeout.as_millis()
                    ),
                    timeout,
                    None,
                )?;
                Ok(trailer(&envelope, None))
            }
            "eval" => {
                let code = need(args, "web eval")?;
                let envelope = self.run_snippet(code.into(), timeout, None)?;
                let text = result_text(&envelope.result);
                Ok(trailer(&envelope, Some(text)))
            }
            "login" => {
                let host = need(args, "web login")?;
                let user = args.iter().skip(1).find_map(|arg| arg.strip_prefix("user="));
                let host = json_str(host);
                let fill = match user {
                    Some(user) => format!(
                        "await credentials.fill({{username: {}, submit: true}})",
                        json_str(user)
                    ),
                    None => "await credentials.fill({submit: true})".to_owned(),
                };
                let code = format!(
                    "if (!page.url().includes({host})) {{ await page.goto(\"https://\" + {host}); }}\n{fill}"
                );
                let envelope = self.run_snippet(code, timeout, None)?;
                Ok(trailer(&envelope, None))
            }
            "download" => {
                let target = need(args, "web download")?;
                let to = args.iter().skip(1).find_map(|arg| arg.strip_prefix("to="));
                let url = target.starts_with("http");
                let approved = url.then(|| vec![target.to_owned()]);
                let code = if url {
                    format!("await page.goto({})", json_str(target))
                } else {
                    format!("await {}.click()", locator(target))
                };
                let envelope = self.run_snippet(code, timeout, approved)?;
                let source = envelope
                    .artifacts
                    .first()
                    .ok_or_else(|| CoreError::Backend("download produced no artifact".into()))?
                    .clone();
                let path = match to {
                    Some(dir) => {
                        std::fs::create_dir_all(dir).map_err(|error| {
                            CoreError::Backend(format!("download dir: {error}"))
                        })?;
                        let name = Path::new(&source)
                            .file_name()
                            .ok_or_else(|| CoreError::Backend("download has no file name".into()))?;
                        let destination = Path::new(dir).join(name);
                        std::fs::copy(&source, &destination)
                            .map_err(|error| CoreError::Backend(format!("download copy: {error}")))?;
                        destination.display().to_string()
                    }
                    None => source,
                };
                Ok(trailer(&envelope, Some(path)))
            }
            "shot" => {
                let path = self.screenshot_named(args.first().map(String::as_str), timeout)?;
                Ok(Some(path.display().to_string()))
            }
            _ => Err(CoreError::Parse(format!("unknown web action `{action}`"))),
        }
    }
    pub fn screenshot(&mut self, timeout: Duration) -> Result<PathBuf, CoreError> {
        self.screenshot_named(None, timeout)
    }
    fn screenshot_named(
        &mut self,
        name: Option<&str>,
        timeout: Duration,
    ) -> Result<PathBuf, CoreError> {
        let code = match name {
            Some(name) => format!(
                "return await screenshot({{kind:\"debug\", name:{}}})",
                json_str(name)
            ),
            None => "return await screenshot({kind:\"debug\"})".to_owned(),
        };
        let envelope = self.run_snippet(code, timeout, None)?;
        let path = envelope
            .artifacts
            .first()
            .cloned()
            .or_else(|| {
                envelope
                    .result
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| CoreError::Backend("screenshot produced no artifact".into()))?;
        Ok(PathBuf::from(path))
    }
}
impl Drop for BwWorker {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"{\"id\":0,\"op\":\"close\"}\n");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static TIMEOUT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)timeout|timed out").unwrap());
static MISSING_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)no element|not found|failed to find|resolved to 0").unwrap()
});
static REF_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^(e[0-9]+|f[0-9]+e[0-9]+)$").unwrap());

pub fn map_bw_error(message: &str) -> CoreError {
    if message.contains("module-not-found") {
        CoreError::Backend(INSTALL_HINT.into())
    } else if message.contains("strict mode violation") {
        CoreError::SelectorAmbiguous(message.into())
    } else if TIMEOUT_RE.is_match(message) {
        CoreError::Timeout(message.into())
    } else if MISSING_RE.is_match(message) {
        CoreError::SelectorNotFound(message.into())
    } else {
        CoreError::Backend(message.into())
    }
}

/// The injection boundary: every agent-supplied string reaches the snippet
/// through here as a JSON (hence JS) string literal.
fn json_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}
fn locator(target: &str) -> String {
    if REF_RE.is_match(target) {
        format!("page.locator({})", json_str(&format!("aria-ref={target}")))
    } else if let Some(rest) = target.strip_prefix("text=") {
        format!("page.getByText({}, {{exact:false}})", json_str(rest))
    } else {
        format!("page.locator({})", json_str(target))
    }
}
fn result_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
fn trailer(envelope: &Envelope, text: Option<String>) -> Option<String> {
    if envelope.challenges.is_empty() {
        return text;
    }
    let lines: Vec<String> = envelope
        .challenges
        .iter()
        .map(|challenge| {
            format!(
                "#challenge provider={} url={}",
                challenge["provider"].as_str().unwrap_or("?"),
                challenge["url"].as_str().unwrap_or("?")
            )
        })
        .collect();
    Some(match text {
        Some(text) => format!("{text}\n{}", lines.join("\n")),
        None => lines.join("\n"),
    })
}
fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("koto"),
        None => std::env::temp_dir().join("koto"),
    }
}
fn need<'a>(args: &'a [String], name: &str) -> Result<&'a str, CoreError> {
    args.first()
        .map(String::as_str)
        .ok_or_else(|| CoreError::Parse(format!("{name} needs an argument")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_snippet_keeps_hostile_text_inert() {
        let target = "#user";
        let text = r#"x");process.exit(1);//"#;
        let expected = format!(
            "await page.locator({}).fill({})",
            serde_json::to_string(target).unwrap(),
            serde_json::to_string(text).unwrap()
        );
        assert_eq!(
            format!("await {}.fill({})", locator(target), json_str(text)),
            expected
        );
        assert_eq!(
            expected,
            r##"await page.locator("#user").fill("x\");process.exit(1);//")"##
        );
    }
    #[test]
    fn click_snippet_resolves_refs() {
        let expected = format!(
            "await page.locator({}).click()",
            serde_json::to_string("aria-ref=e12").unwrap()
        );
        assert_eq!(format!("await {}.click()", locator("e12")), expected);
        assert_eq!(
            format!("await {}.click()", locator("f1e7")),
            format!(
                "await page.locator({}).click()",
                serde_json::to_string("aria-ref=f1e7").unwrap()
            )
        );
    }
    #[test]
    fn text_snippet_uses_get_by_text() {
        let expected = format!(
            "await page.getByText({}, {{exact:false}}).click()",
            serde_json::to_string("Sign in\"").unwrap()
        );
        assert_eq!(
            format!("await {}.click()", locator("text=Sign in\"")),
            expected
        );
    }
    #[test]
    fn error_mapping_table() {
        assert!(matches!(
            map_bw_error("module-not-found"),
            CoreError::Backend(message) if message == INSTALL_HINT
        ));
        assert!(matches!(
            map_bw_error("strict mode violation: resolved to 3 elements"),
            CoreError::SelectorAmbiguous(_)
        ));
        assert!(matches!(
            map_bw_error("strict mode violation after Timeout 30000ms"),
            CoreError::SelectorAmbiguous(_)
        ));
        assert!(matches!(
            map_bw_error("Timeout 30000ms exceeded"),
            CoreError::Timeout(_)
        ));
        assert!(matches!(
            map_bw_error("the operation timed out"),
            CoreError::Timeout(_)
        ));
        assert!(matches!(
            map_bw_error("No element matches selector"),
            CoreError::SelectorNotFound(_)
        ));
        assert!(matches!(
            map_bw_error("locator resolved to 0 elements"),
            CoreError::SelectorNotFound(_)
        ));
        assert!(matches!(
            map_bw_error("page crashed"),
            CoreError::Backend(message) if message == "page crashed"
        ));
    }

    fn fake_sidecar(body: &str, tag: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "koto-bw-fake-{tag}-{}.sh",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().into_owned()
    }
    #[test]
    fn ndjson_round_trip() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":1}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"id":%s,"ok":true,"result":"pong","artifacts":[],"challenges":[],"error":null}\n' "$id"
done
"#,
            "echo",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None).unwrap();
        let out = worker
            .action("read", &[], Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.as_deref(), Some("pong"));
    }
    #[test]
    fn deadline_kills_the_child() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":1}\n'
IFS= read -r line
printf '{"id":1,"ok":true}\n'
sleep 3600
"#,
            "stall",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None).unwrap();
        let error = worker
            .call(json!({"op":"run"}), Duration::from_millis(200))
            .unwrap_err();
        assert!(matches!(error, CoreError::Timeout(op) if op == "betterwright run"));
        let mut gone = false;
        for _ in 0..50 {
            if worker.child.try_wait().ok().flatten().is_some() {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "child should be killed after the deadline");
    }
}
