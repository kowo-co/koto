//! BetterWright engine: a node sidecar driven over NDJSON on stdio.
//!
//! Each `web` action maps onto exactly one function of the betterwright 1.6.3
//! browser API (the snippet globals `page`/`pages`/`openPage`/`snapshot`/
//! `screenshot`/`human`/`captcha`/`credentials`/`overlays`/`controls`/`media`/
//! `dialogs`) or one method of the host client (`startLiveView`,
//! `waitForHandoff`, `waitForAsk`, `liveViewPostChat`, `liveViewDrainChat`,
//! `closeSession`), so the agent gets the whole engine through koto commands.
use koto_core::CoreError;
use serde_json::{Map, Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::LazyLock,
    sync::mpsc::{Receiver, RecvTimeoutError, channel},
    time::{Duration, Instant},
};

const INSTALL_HINT: &str =
    "betterwright not installed: npm i -g betterwright@1.6.3 or set KOTO_BETTERWRIGHT_DIR";
const MIN_VERSION: (u64, u64, u64) = (1, 6, 3);
const READY: Duration = Duration::from_secs(15);
/// Default bound for `web handoff` / `web ask`, matching betterwright's own.
const HUMAN_WAIT: Duration = Duration::from_secs(1800);

/// How a worker talks to the sidecar. `Owned` dies with this process (tests);
/// `Daemon` is a socket to a sidecar that outlives us, keeping its browser and
/// pages alive between koto invocations.
enum Transport {
    Owned { child: Child, stdin: ChildStdin },
    Daemon { socket: UnixStream },
}
impl Transport {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        match self {
            Self::Owned { stdin, .. } => {
                stdin.write_all(line.as_bytes())?;
                stdin.flush()
            }
            Self::Daemon { socket } => {
                socket.write_all(line.as_bytes())?;
                socket.flush()
            }
        }
    }
}

pub struct BwWorker {
    transport: Transport,
    lines: Receiver<String>,
    next: u64,
}

#[derive(Debug)]
struct Envelope {
    ok: bool,
    result: Value,
    artifacts: Vec<String>,
    challenges: Vec<Value>,
    warnings: Vec<String>,
    error: Option<String>,
}
impl Envelope {
    fn parse(value: &Value) -> Self {
        Self {
            ok: value["ok"].as_bool().unwrap_or(false),
            result: value.get("result").cloned().unwrap_or(Value::Null),
            artifacts: string_list(&value["artifacts"]),
            challenges: value["challenges"].as_array().cloned().unwrap_or_default(),
            warnings: string_list(&value["warnings"]),
            error: value["error"].as_str().map(str::to_owned),
        }
    }
}
fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

impl BwWorker {
    /// Connect to the persistent sidecar, starting it if it isn't running.
    /// The browser and its pages belong to that daemon, so a page opened by
    /// one koto invocation is still there for the next one.
    pub fn spawn(
        profile: Option<&str>,
        session: Option<&str>,
        platform: Option<&str>,
    ) -> Result<Self, CoreError> {
        let script = install_sidecar()?;
        let socket = bw_socket_path();
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(_) => {
                // A socket with nobody behind it is litter, not a session.
                let _ = std::fs::remove_file(&socket);
                start_daemon(&script, &socket)?;
                UnixStream::connect(&socket).map_err(|error| {
                    CoreError::Backend(format!(
                        "betterwright daemon did not accept a connection: {error}"
                    ))
                })?
            }
        };
        // No socket read timeout: the reader thread must block, not error out
        // between requests. Deadlines are enforced by `recv_timeout`.
        let reader = stream
            .try_clone()
            .map_err(|error| CoreError::Backend(format!("sidecar socket clone: {error}")))?;
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut worker = Self {
            transport: Transport::Daemon { socket: stream },
            lines,
            next: 1,
        };
        worker.init(profile, session, platform)?;
        Ok(worker)
    }
    pub(crate) fn spawn_with_program(
        program: &str,
        args: &[&str],
        profile: Option<&str>,
        session: Option<&str>,
        platform: Option<&str>,
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
            transport: Transport::Owned { child, stdin },
            lines,
            next: 1,
        };
        loop {
            match worker.lines.recv_timeout(READY) {
                Ok(line) => {
                    let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if value["event"].as_str() == Some("ready") {
                        if let Some(found) = value["version"].as_str().filter(|v| too_old(v)) {
                            worker.kill_owned();
                            return Err(CoreError::Backend(stale_version(found)));
                        }
                        break;
                    }
                    if value["error"].as_str() == Some("module-not-found") {
                        worker.kill_owned();
                        return Err(CoreError::Backend(INSTALL_HINT.into()));
                    }
                }
                Err(_) => {
                    worker.kill_owned();
                    return Err(CoreError::Backend(format!(
                        "betterwright sidecar failed to start; see {}",
                        log_path.display()
                    )));
                }
            }
        }
        worker.init(profile, session, platform)?;
        Ok(worker)
    }
    fn init(
        &mut self,
        profile: Option<&str>,
        session: Option<&str>,
        platform: Option<&str>,
    ) -> Result<(), CoreError> {
        let mut init = json!({"op":"init"});
        if let Some(profile) = profile {
            init["profile"] = Value::String(profile.into());
        }
        if let Some(session) = session {
            init["session"] = Value::String(session.into());
        }
        if let Some(platform) = platform {
            init["platform"] = Value::String(platform.into());
        }
        let envelope = self.call(init, READY)?;
        if !envelope.ok {
            return Err(map_bw_error(
                envelope.error.as_deref().unwrap_or("betterwright init failed"),
            ));
        }
        Ok(())
    }
    fn kill_owned(&mut self) {
        if let Transport::Owned { child, .. } = &mut self.transport {
            let _ = child.kill();
        }
    }
    fn call(&mut self, mut payload: Value, deadline: Duration) -> Result<Envelope, CoreError> {
        let id = self.next;
        self.next += 1;
        payload["id"] = json!(id);
        let op = payload["op"].as_str().unwrap_or("?").to_owned();
        let line = format!("{payload}\n");
        self.transport
            .write_line(&line)
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
                    // An owned sidecar is ours to kill; the shared daemon is
                    // not — other invocations depend on its browser.
                    self.kill_owned();
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
    fn host_call(
        &mut self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Envelope, CoreError> {
        let envelope = self.call(json!({"op":"call","method":method,"params":params}), deadline)?;
        if !envelope.ok {
            return Err(map_bw_error(
                envelope
                    .error
                    .as_deref()
                    .unwrap_or("betterwright host call failed"),
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
            // -- navigation: page.goto / goBack / goForward / reload --
            "goto" => {
                let url = need(args, "web goto")?;
                self.simple(&format!("await page.goto({})", json_str(url)), timeout)
            }
            // History restores from the back-forward cache never fire `load`,
            // so waiting for it deadlocks; `commit` is the state that matters.
            "back" => self.simple("await page.goBack({waitUntil:\"commit\"})", timeout),
            "forward" => self.simple("await page.goForward({waitUntil:\"commit\"})", timeout),
            "reload" => self.simple("await page.reload()", timeout),
            // -- tabs: openPage / usePage / closePage / pages --
            "open" => {
                let code = match args.first() {
                    Some(url) => format!("return await openPage({})", json_str(url)),
                    None => "return await openPage()".into(),
                };
                let envelope = self.run_snippet(code, timeout, None)?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "use" => {
                let target = need(args, "web use")?;
                self.simple(&format!("await usePage({})", page_ref(target)), timeout)
            }
            "close" => {
                let code = match args.first() {
                    Some(target) => format!("await closePage({})", page_ref(target)),
                    None => "await closePage()".into(),
                };
                self.simple(&code, timeout)
            }
            "pages" => {
                // `page` is one of `pages`, so returning both makes the
                // serializer collapse the repeat to "[Circular]" and destroys
                // the listing. Send the current tab's index instead.
                let envelope = self
                    .run_snippet("return [pages.indexOf(page), pages]".into(), timeout, None)?;
                Ok(trailer(&envelope, Some(pages_listing(&envelope.result))))
            }
            // -- reading: snapshot / screenshot / page.pdf / the inspectors --
            "read" => {
                let options = snapshot_options(args)?;
                let envelope = self
                    .run_snippet(format!("return await snapshot({options})"), timeout, None)
                    .map_err(scoped_read_help)?;
                let mut text = result_text(&envelope.result);
                // betterwright's overflow message advertises its own JS option
                // names ({selector}, {maxChars}); translate to koto grammar so
                // an agent's retry parses.
                if text.contains("Retry with {") {
                    text.push_str(
                        "\nhelp: in koto that is `web read selector=<css>`, `web read ref=eN`, `web read depth=<n>`, or `web read max=<chars>` (up to 20000)",
                    );
                }
                Ok(trailer(&envelope, Some(text)))
            }
            "shot" => {
                let path = self.screenshot_with(args, timeout)?;
                Ok(Some(path.display().to_string()))
            }
            "pdf" => {
                let name = json_str(args.first().map_or("page.pdf", String::as_str));
                self.returning(
                    format!("const path = artifactPath({name}); await page.pdf({{path}}); return path"),
                    timeout,
                )
            }
            "overlays" => self.returning("return await overlays.dismiss()".into(), timeout),
            "controls" => self.returning("return await controls.inspect()".into(), timeout),
            "media" => self.returning("return await media.inspect()".into(), timeout),
            // -- acting: human.click / human.type / human.scroll, and the
            // locator surface for the precise variants --
            "click" => {
                let target = need(args, "web click")?;
                let newtab = args.iter().skip(1).any(|arg| arg == "newtab");
                // `newtab` is a Ctrl+click. The modifier must ride inside the
                // mouse event itself — the fork ignores a separately
                // synthesized held Control (same quirk betterwright works
                // around for select-all), so this is the one click that goes
                // through the locator instead of `human.click`. `pages` is
                // live, so wait until the background tab is adopted (or 5s).
                if newtab {
                    let code = format!(
                        "const before = pages.length;\n\
                         await {}.click({{modifiers:[\"Control\"]}});\n\
                         for (let i = 0; i < 25 && pages.length === before; i++) await page.waitForTimeout(200);\n\
                         return pages.length > before ? pages[pages.length - 1] : null",
                        locator(target)
                    );
                    let envelope = self.act_on(code, target, timeout)?;
                    let text = match page_line(&envelope.result) {
                        Some(line) => format!("new tab: {line}"),
                        None => "no new tab appeared; the link may not be a same-context navigation".into(),
                    };
                    return Ok(trailer(&envelope, Some(text)));
                }
                let code = format!("await human.click({})\nreturn page", locator(target));
                let envelope = self.act_on(code, target, timeout)?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "type" | "fill" => {
                let target = need(args, "web type")?;
                let text = args
                    .get(1)
                    .ok_or_else(|| CoreError::Parse(format!("web {action} needs text")))?;
                let clear = !args.iter().skip(2).any(|arg| arg == "append");
                let place = locator(target);
                let write = if action == "type" {
                    format!("await human.type({place}, {}, {{clear:{clear}}})", json_str(text))
                } else {
                    format!("await {place}.fill({})", json_str(text))
                };
                // Echo the field's actual post-action value — the site's own
                // masking or reformatting is part of the outcome — but never a
                // password's.
                let code = format!(
                    "{write}\n\
                     const kind = await {place}.getAttribute(\"type\").catch(() => null);\n\
                     const value = kind === \"password\" ? \"[redacted]\" : await {place}.inputValue().catch(() => null);\n\
                     return [page, value]"
                );
                let envelope = self.act_on(code, target, timeout)?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "scroll" => {
                let delta = int(need(args, "web scroll")?, "web scroll")?;
                if delta == 0 {
                    return Err(CoreError::Parse(
                        "web scroll needs a non-zero delta: positive scrolls down, negative up".into(),
                    ));
                }
                self.simple(&format!("await human.scroll({delta})"), timeout)
            }
            "hover" => {
                let target = need(args, "web hover")?;
                let code = format!("await {}.hover()\nreturn page", locator(target));
                let envelope = self.act_on(code, target, timeout)?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "press" => {
                let key = need(args, "web press")?;
                let code = format!("await page.keyboard.press({})\nreturn page", json_str(key));
                let envelope = self.run_snippet(code, timeout, None).map_err(|error| {
                    with_help(
                        error,
                        "Unknown key",
                        "Playwright key names: Enter, Tab, Escape, Backspace, Home, End, PageDown, PageUp, ArrowDown/ArrowUp/ArrowLeft/ArrowRight, F1-F12, a-z; chords join with `+` (Control+a)",
                    )
                })?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "select" => {
                let target = need(args, "web select")?;
                let values: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
                if values.is_empty() {
                    return Err(CoreError::Parse("web select needs a value".into()));
                }
                let code = format!(
                    "await {}.selectOption({})\nreturn page",
                    locator(target),
                    serde_json::to_string(&values).unwrap_or_else(|_| "[]".into())
                );
                let envelope = self.act_on(code, target, timeout)?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "wait" => {
                let target = need(args, "web wait")?;
                let code = format!(
                    "await {}.waitFor({{timeout:{}}})\nreturn page",
                    locator(target),
                    timeout.as_millis()
                );
                let envelope = self.act_on(code, target, timeout)?;
                Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
            }
            "dialog" => {
                let code = match args.first().map(String::as_str) {
                    Some("accept") => match args.get(1) {
                        Some(text) => format!("dialogs.acceptNext({})", json_str(text)),
                        None => "dialogs.acceptNext()".into(),
                    },
                    Some("dismiss") => "dialogs.dismissNext()".into(),
                    _ => {
                        return Err(CoreError::Parse(
                            "web dialog needs `accept [text]` or `dismiss`".into(),
                        ));
                    }
                };
                self.simple(&code, timeout)
            }
            "eval" => {
                let code = need(args, "web eval")?;
                let envelope = self.run_snippet(code.into(), timeout, None).map_err(|error| {
                    with_help(
                        error,
                        "is not defined",
                        "eval snippets run in the worker, not the page: reach the DOM with `page.evaluate(() => document...)`; worker globals are page, pages, snapshot, human, credentials",
                    )
                })?;
                let text = result_text(&envelope.result);
                Ok(trailer(&envelope, Some(text)))
            }
            // -- challenges: the captcha global --
            "captcha" => self.captcha(args, timeout),
            // -- credentials: the vault helpers, metadata in / secrets never out --
            "creds" => self.creds(args, timeout),
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
                self.simple(&code, timeout)
            }
            "download" => {
                let target = need(args, "web download")?;
                let to = args.iter().skip(1).find_map(|arg| arg.strip_prefix("to="));
                let url = target.starts_with("http");
                let approved = url.then(|| vec![target.to_owned()]);
                let code = if url {
                    format!("await page.goto({})", json_str(target))
                } else {
                    format!("await human.click({})", locator(target))
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
            // -- host client methods over op:"call" --
            "view" | "handoff" | "ask" | "chat" | "session" => {
                self.host_action(action, args, timeout)
            }
            _ => Err(CoreError::Parse(format!("unknown web action `{action}`"))),
        }
    }
    /// Run a snippet and report where the session landed: every success
    /// answers "what page am I on now" so the agent never spends a turn
    /// asking.
    fn simple(&mut self, code: &str, timeout: Duration) -> Result<Option<String>, CoreError> {
        let envelope = self.run_snippet(format!("{code}\nreturn page"), timeout, None)?;
        Ok(trailer(&envelope, Some(expressive_result(&envelope.result))))
    }
    /// Run a snippet whose return value is the output.
    fn returning(&mut self, code: String, timeout: Duration) -> Result<Option<String>, CoreError> {
        let envelope = self.run_snippet(code, timeout, None)?;
        let text = result_text(&envelope.result);
        Ok(trailer(&envelope, Some(text)))
    }
    /// Run a target-taking snippet; when the target misses, spend one more
    /// snippet fetching the current interactive tree so the error carries the
    /// observation the agent would otherwise burn a turn on.
    fn act_on(
        &mut self,
        code: String,
        target: &str,
        timeout: Duration,
    ) -> Result<Envelope, CoreError> {
        match self.run_snippet(code, timeout, None) {
            Ok(envelope) => Ok(envelope),
            Err(error) => Err(self.enrich_miss(error, target, timeout)),
        }
    }
    fn enrich_miss(&mut self, error: CoreError, target: &str, timeout: Duration) -> CoreError {
        let (rebuild, message): (fn(String) -> CoreError, &str) = match &error {
            CoreError::SelectorNotFound(message) => (CoreError::SelectorNotFound, message),
            CoreError::Timeout(message) if message.contains("waiting for") => {
                (CoreError::Timeout, message)
            }
            CoreError::SelectorAmbiguous(message) => {
                return CoreError::SelectorAmbiguous(format!(
                    "{message}\nhelp: disambiguate with a `[ref=eN]` from `web read`, or a more specific selector"
                ));
            }
            _ => return error,
        };
        let Ok(envelope) = self.run_snippet(
            "return await snapshot({interactive:true, maxChars:4000})".into(),
            timeout,
            None,
        ) else {
            return error;
        };
        let Value::String(tree) = &envelope.result else {
            return error;
        };
        rebuild(compose_miss_help(message, target, tree))
    }
    fn captcha(
        &mut self,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        let sub = need(args, "web captcha")?;
        let rest = &args[1..];
        let code = match sub {
            "solve" => "return await captcha.solve()".to_owned(),
            "inspect" | "text" => {
                let helper = if sub == "text" { "readText" } else { "inspect" };
                match bounds(rest, &format!("web captcha {sub}"))? {
                    Some(bounds) => format!("return await captcha.{helper}({bounds})"),
                    None => format!("return await captcha.{helper}()"),
                }
            }
            "click" => {
                let bounds = bounds(rest, "web captcha click")?.ok_or_else(|| {
                    CoreError::Parse("web captcha click needs bounds: x y width height".into())
                })?;
                format!("return await captcha.click({bounds})")
            }
            "drag" => {
                let numbers = ints(rest, "web captcha drag")?;
                let [x1, y1, x2, y2, steps @ ..] = numbers.as_slice() else {
                    return Err(CoreError::Parse(
                        "web captcha drag needs: x1 y1 x2 y2 [steps]".into(),
                    ));
                };
                let steps = match steps {
                    [] => String::new(),
                    [steps] => format!(", {{steps:{steps}}}"),
                    _ => {
                        return Err(CoreError::Parse(
                            "web captcha drag needs: x1 y1 x2 y2 [steps]".into(),
                        ));
                    }
                };
                format!("return await captcha.drag({{x:{x1},y:{y1}}}, {{x:{x2},y:{y2}}}{steps})")
            }
            other => {
                return Err(CoreError::Parse(format!(
                    "unknown web captcha action `{other}` (solve, inspect, click, drag, text)"
                )));
            }
        };
        let envelope = self.run_snippet(code, timeout, None)?;
        let text = result_text(&envelope.result);
        Ok(trailer(&envelope, Some(text)))
    }
    fn creds(&mut self, args: &[String], timeout: Duration) -> Result<Option<String>, CoreError> {
        let sub = need(args, "web creds")?;
        let rest = &args[1..];
        let submit = !rest.iter().any(|arg| arg == "nosubmit");
        let code = match sub {
            "inspect" => "return await credentials.inspect()".to_owned(),
            "list" => {
                let mut filter = Map::new();
                for arg in rest {
                    if let Some(text) = arg.strip_prefix("text=") {
                        filter.insert("text".into(), json!(text));
                    } else if let Some(category) = arg.strip_prefix("category=") {
                        filter.insert("category".into(), json!(category));
                    } else {
                        filter.insert("text".into(), json!(arg));
                    }
                }
                format!("return await credentials.list({})", Value::Object(filter))
            }
            "fill" => {
                let mut options = Map::new();
                options.insert("submit".into(), json!(submit));
                for arg in rest {
                    if let Some(id) = arg.strip_prefix("id=") {
                        options.insert("id".into(), json!(id));
                    } else if let Some(user) = arg.strip_prefix("user=") {
                        options.insert("username".into(), json!(user));
                    }
                }
                format!("return await credentials.fill({})", Value::Object(options))
            }
            "generate" => {
                let mut options = Map::new();
                options.insert("submit".into(), json!(submit));
                for arg in rest {
                    if let Some(user) = arg.strip_prefix("user=") {
                        options.insert("username".into(), json!(user));
                    }
                }
                format!(
                    "return await credentials.generateAndFill({})",
                    Value::Object(options)
                )
            }
            "pending" => "return await credentials.listPending()".to_owned(),
            "commit" | "discard" => {
                let id = rest
                    .first()
                    .ok_or_else(|| CoreError::Parse(format!("web creds {sub} needs a pending id")))?;
                let method = if sub == "commit" {
                    "commitGenerated"
                } else {
                    "discardGenerated"
                };
                format!(
                    "return await credentials.{method}({{pendingId: {}}})",
                    json_str(id)
                )
            }
            other => {
                return Err(CoreError::Parse(format!(
                    "unknown web creds action `{other}` (list, inspect, fill, generate, pending, commit, discard)"
                )));
            }
        };
        let envelope = self.run_snippet(code, timeout, None)?;
        let text = result_text(&envelope.result);
        Ok(trailer(&envelope, Some(text)))
    }
    fn host_action(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        let (method, params, deadline) = match action {
            "view" => {
                let method = match args.first().map(String::as_str) {
                    Some("start") => "startLiveView",
                    Some("stop") => "stopLiveView",
                    Some("status") | None => "liveViewStatus",
                    Some(other) => {
                        return Err(CoreError::Parse(format!(
                            "unknown web view action `{other}` (start, stop, status)"
                        )));
                    }
                };
                let mut params = Map::new();
                for arg in args.iter().skip(1) {
                    if let Some(expose) = arg.strip_prefix("expose=") {
                        params.insert("expose".into(), json!(expose));
                    } else if let Some(port) = arg.strip_prefix("port=") {
                        params.insert("port".into(), json!(int(port, "web view port")?));
                    } else {
                        return Err(CoreError::Parse(format!(
                            "web view: expected expose=<preset> or port=<n>, got `{arg}`"
                        )));
                    }
                }
                (method, Value::Object(params), timeout + Duration::from_secs(5))
            }
            "handoff" | "ask" => {
                let mut params = Map::new();
                let mut wait = HUMAN_WAIT;
                let mut words = Vec::new();
                for arg in args {
                    if let Some(seconds) = arg.strip_prefix("timeout=") {
                        wait = Duration::from_secs(
                            int(seconds, "web handoff timeout")?.unsigned_abs(),
                        );
                    } else if let Some(options) = arg.strip_prefix("options=") {
                        let choices: Vec<&str> = options.split('|').collect();
                        params.insert("options".into(), json!(choices));
                    } else {
                        words.push(arg.as_str());
                    }
                }
                params.insert("timeout".into(), json!(wait.as_secs()));
                if !words.is_empty() {
                    let field = if action == "ask" { "question" } else { "prompt" };
                    params.insert(field.into(), json!(words.join(" ")));
                }
                let method = if action == "ask" {
                    "waitForAsk"
                } else {
                    "waitForHandoff"
                };
                (method, Value::Object(params), wait + Duration::from_secs(15))
            }
            "chat" => match args.first().map(String::as_str) {
                Some("post") => {
                    let text = args[1..].join(" ");
                    if text.is_empty() {
                        return Err(CoreError::Parse("web chat post needs text".into()));
                    }
                    (
                        "liveViewPostChat",
                        json!({"role":"agent","text":text}),
                        timeout + Duration::from_secs(5),
                    )
                }
                None => (
                    "liveViewDrainChat",
                    json!({}),
                    timeout + Duration::from_secs(5),
                ),
                Some(other) => {
                    return Err(CoreError::Parse(format!(
                        "unknown web chat action `{other}` (post, or no argument to drain)"
                    )));
                }
            },
            "session" => match args.first().map(String::as_str) {
                Some("close") => (
                    "closeSession",
                    json!({}),
                    timeout + Duration::from_secs(5),
                ),
                _ => {
                    return Err(CoreError::Parse(
                        "web session supports only `close`".into(),
                    ));
                }
            },
            _ => unreachable!("host_action called for `{action}`"),
        };
        let envelope = self.host_call(method, params, deadline)?;
        Ok(Some(result_text(&envelope.result)))
    }
    pub fn screenshot(&mut self, timeout: Duration) -> Result<PathBuf, CoreError> {
        self.screenshot_with(&[], timeout)
    }
    fn screenshot_with(
        &mut self,
        args: &[String],
        timeout: Duration,
    ) -> Result<PathBuf, CoreError> {
        let mut options = Map::new();
        options.insert("kind".into(), json!("debug"));
        for arg in args {
            match arg.as_str() {
                "annotate" => {
                    options.insert("annotate".into(), json!(true));
                }
                "full" => {
                    options.insert("fullPage".into(), json!(true));
                }
                "jpeg" => {
                    options.insert("type".into(), json!("jpeg"));
                }
                other => {
                    if let Some(kind) = other.strip_prefix("kind=") {
                        if !matches!(kind, "proof" | "question" | "debug") {
                            return Err(CoreError::Parse(format!(
                                "web shot kind must be proof, question, or debug, got `{kind}`"
                            )));
                        }
                        options.insert("kind".into(), json!(kind));
                    } else if let Some(quality) = other.strip_prefix("quality=") {
                        options.insert("quality".into(), json!(int(quality, "web shot quality")?));
                    } else if other.contains('=') {
                        return Err(CoreError::Parse(format!(
                            "unknown web shot option `{other}`"
                        )));
                    } else {
                        options.insert("name".into(), json!(other));
                    }
                }
            }
        }
        let code = format!("return await screenshot({})", Value::Object(options));
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
        match &mut self.transport {
            Transport::Owned { child, stdin } => {
                let _ = stdin.write_all(b"{\"id\":0,\"op\":\"close\"}\n");
                let _ = stdin.flush();
                let _ = child.kill();
                let _ = child.wait();
            }
            // Disconnecting from the daemon leaves the browser — and the page
            // this run left open — alive for the next invocation.
            Transport::Daemon { .. } => {}
        }
    }
}

static TIMEOUT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)timeout|timed out").unwrap());
static MISSING_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)no element|not found|failed to find|resolved to 0|does not match any element",
    )
    .unwrap()
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

/// A scoped `web read` fails in exactly two ways, and Playwright's raw text
/// answers neither question an agent has next. Trim its element dump to the
/// first few candidates and say how to narrow or widen the scope.
fn scoped_read_help(error: CoreError) -> CoreError {
    match error {
        CoreError::SelectorAmbiguous(message) => {
            let count = message
                .split("resolved to ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or("several");
            CoreError::SelectorAmbiguous(format!(
                "{}\nhelp: the selector matched {count} elements and a snapshot needs exactly one; \
                 narrow it (`selector=#main ul`, `selector=ul.repo-list`), scope by handle with \
                 `web read ref=eN` from a plain `web read`, or drop the selector and use \
                 `web read depth=<n>` to bound the whole tree",
                clamp_dump(&message)
            ))
        }
        CoreError::SelectorNotFound(message) => CoreError::SelectorNotFound(format!(
            "{message}\nhelp: nothing matched that selector on this page; run a plain `web read` \
             to see what is actually there, then scope with `ref=eN` or a selector from it"
        )),
        other => other,
    }
}

/// Playwright lists every match on a strict-mode violation; 27 elements of raw
/// HTML buries the help line. Keep the first three.
fn clamp_dump(message: &str) -> String {
    let mut out = String::new();
    let mut listed = 0;
    for line in message.lines() {
        if line.trim_start().starts_with(|c: char| c.is_ascii_digit()) && line.contains(") <") {
            listed += 1;
            if listed > 3 {
                continue;
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if listed > 3 {
        // Playwright truncates its own list, so this counts what it showed,
        // not the total — the total is in the "resolved to N" line above.
        out.push_str(&format!("\n    … and {} more listed", listed - 3));
    }
    out
}

/// Append a `help:` line to a backend error whose message matches — the
/// Rust-error philosophy applied to known agent stumbles.
fn with_help(error: CoreError, needle: &str, help: &str) -> CoreError {
    match error {
        CoreError::Backend(message) if message.contains(needle) => {
            CoreError::Backend(format!("{message}\nhelp: {help}"))
        }
        other => other,
    }
}

fn too_old(version: &str) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter_map(|p| {
        if p.is_empty() { None } else { p.parse::<u64>().ok() }
    });
    let found = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    found < MIN_VERSION
}

/// The injection boundary: every agent-supplied string reaches the snippet
/// through here as a JSON (hence JS) string literal, and every number through
/// `int`, so nothing an agent types can become code.
fn json_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}
fn int(value: &str, name: &str) -> Result<i64, CoreError> {
    value
        .parse()
        .map_err(|_| CoreError::Parse(format!("{name} needs an integer, got `{value}`")))
}
fn ints(args: &[String], name: &str) -> Result<Vec<i64>, CoreError> {
    args.iter().map(|arg| int(arg, name)).collect()
}
/// CSS-pixel bounds for the captcha helpers: exactly four integers or nothing.
fn bounds(args: &[String], name: &str) -> Result<Option<String>, CoreError> {
    if args.is_empty() {
        return Ok(None);
    }
    let numbers = ints(args, name)?;
    let [x, y, width, height] = numbers.as_slice() else {
        return Err(CoreError::Parse(format!(
            "{name} bounds are: x y width height"
        )));
    };
    Ok(Some(format!(
        "{{x:{x},y:{y},width:{width},height:{height}}}"
    )))
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
/// `usePage`/`closePage` accept an index or a pageId.
fn page_ref(target: &str) -> String {
    match target.parse::<u64>() {
        Ok(index) => index.to_string(),
        Err(_) => json_str(target),
    }
}
/// Build the `snapshot()` options object from `web read` arguments.
fn snapshot_options(args: &[String]) -> Result<Value, CoreError> {
    let mut options = Map::new();
    let mut interactive = true;
    for arg in args {
        match arg.as_str() {
            "full" => interactive = false,
            "diff" => {
                options.insert("diff".into(), json!(true));
            }
            "urls" => {
                options.insert("urls".into(), json!(true));
            }
            other => {
                if let Some(reference) = other.strip_prefix("ref=") {
                    interactive = false;
                    options.insert("ref".into(), json!(reference));
                } else if let Some(selector) = other.strip_prefix("selector=") {
                    interactive = false;
                    options.insert("selector".into(), json!(selector));
                } else if let Some(depth) = other.strip_prefix("depth=") {
                    options.insert("depth".into(), json!(int(depth, "web read depth")?));
                } else if let Some(max) = other
                    .strip_prefix("max=")
                    .or_else(|| other.strip_prefix("maxChars="))
                {
                    options.insert("maxChars".into(), json!(int(max, "web read max")?));
                } else {
                    return Err(CoreError::Parse(format!(
                        "unknown web read option `{other}` (expected: full, diff, urls, ref=<eN>, selector=<css>, depth=<n>, max=<chars>)"
                    )));
                }
            }
        }
    }
    if interactive {
        options.insert("interactive".into(), json!(true));
    } else if !options.contains_key("maxChars") {
        // A full tree wants the ceiling betterwright allows.
        options.insert("maxChars".into(), json!(20000));
    }
    Ok(Value::Object(options))
}
fn result_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
/// One line of "where am I": `page page-1 https://… "Title"`.
fn page_line(value: &Value) -> Option<String> {
    if value["type"].as_str() != Some("Page") {
        return None;
    }
    Some(format!(
        "page {} {} {}",
        value["pageId"].as_str().unwrap_or("?"),
        value["url"].as_str().unwrap_or("?"),
        json_str(value["title"].as_str().unwrap_or(""))
    ))
}
/// Format an action result expressively: a page summary becomes a state line,
/// a `[page, value]` pair adds the field's echoed value, anything else prints
/// as-is.
fn expressive_result(value: &Value) -> String {
    if let Some(line) = page_line(value) {
        return line;
    }
    if let Some(pair) = value.as_array()
        && pair.len() == 2
        && let Some(line) = page_line(&pair[0])
    {
        return match &pair[1] {
            Value::Null => line,
            Value::String(text) => format!("{line}\nvalue {}", json_str(text)),
            other => format!("{line}\nvalue {other}"),
        };
    }
    result_text(value)
}
/// Format `[currentIndex, pages]` as one line per tab, current marked with `*`.
fn pages_listing(value: &Value) -> String {
    let Some(pair) = value.as_array().filter(|pair| pair.len() == 2) else {
        return result_text(value);
    };
    let current = pair[0].as_i64().unwrap_or(-1);
    let Some(all) = pair[1].as_array() else {
        return result_text(value);
    };
    let lines: Vec<String> = all
        .iter()
        .enumerate()
        .filter_map(|(index, page)| {
            let line = page_line(page)?;
            let marker = if index as i64 == current { "* " } else { "  " };
            Some(format!("{marker}{line}"))
        })
        .collect();
    if lines.is_empty() {
        result_text(value)
    } else {
        lines.join("\n")
    }
}
/// Build a Rust-shaped miss diagnosis: the failure, where it happened, the
/// nearest live candidates, and the way out — so the error itself replaces
/// the `web read` the agent would otherwise need.
fn compose_miss_help(original: &str, target: &str, tree: &str) -> String {
    let header = tree.lines().next().unwrap_or("").trim();
    let interactive: Vec<&str> = tree
        .lines()
        .skip(1)
        .filter(|line| line.contains("[ref="))
        .collect();
    let stale_ref = REF_RE.is_match(target);
    let needle = target.strip_prefix("text=").unwrap_or(target).to_lowercase();
    // Two-letter tokens ("in", "of") match half the tree; require three.
    let tokens: Vec<&str> = needle
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.len() >= 3)
        .collect();
    let mut scored: Vec<(usize, &str)> = interactive
        .iter()
        .map(|line| {
            let lower = line.to_lowercase();
            let score = tokens.iter().filter(|token| lower.contains(**token)).count();
            (score, *line)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let matched = !stale_ref && scored.first().is_some_and(|(score, _)| *score > 0);
    let picks: Vec<&str> = if matched {
        scored
            .iter()
            .take_while(|(score, _)| *score > 0)
            .take(5)
            .map(|(_, line)| *line)
            .collect()
    } else {
        interactive.iter().take(8).copied().collect()
    };
    let mut out = format!("{original}\non {header}");
    if !picks.is_empty() {
        out.push_str(if matched {
            "\nnearest interactive elements:"
        } else {
            "\ninteractive elements on this page:"
        });
        for line in picks {
            out.push_str("\n  ");
            out.push_str(line.trim_start());
        }
    }
    out.push_str(if stale_ref {
        "\nhelp: refs are reassigned by every `web read` and go stale when the page changes; re-read and use a fresh ref"
    } else {
        "\nhelp: act on a `[ref=eN]` from `web read`, or match visible words with `text=`"
    });
    out
}
fn trailer(envelope: &Envelope, text: Option<String>) -> Option<String> {
    let mut lines: Vec<String> = envelope
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
    lines.extend(
        envelope
            .warnings
            .iter()
            .map(|warning| format!("#warn {warning}")),
    );
    if lines.is_empty() {
        return text;
    }
    Some(match text {
        Some(text) => format!("{text}\n{}", lines.join("\n")),
        None => lines.join("\n"),
    })
}
fn stale_version(found: &str) -> String {
    format!(
        "betterwright {found} is older than the {}.{}.{} the koto engine tracks: npm i -g betterwright@1.6.3",
        MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
    )
}

/// Where the persistent sidecar listens.
pub fn bw_socket_path() -> PathBuf {
    runtime_dir().join("bw.sock")
}

fn install_sidecar() -> Result<String, CoreError> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| CoreError::Backend(format!("sidecar dir: {error}")))?;
    let path = dir.join("bw-sidecar-v2.mjs");
    std::fs::write(&path, include_str!("sidecar.mjs"))
        .map_err(|error| CoreError::Backend(format!("sidecar write: {error}")))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Start the sidecar detached, and wait for it to report `listening`.
fn start_daemon(script: &str, socket: &Path) -> Result<(), CoreError> {
    let log_path = runtime_dir().join("bw-sidecar.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| CoreError::Backend(format!("sidecar log: {error}")))?;
    let mut command = Command::new("node");
    command
        .arg(script)
        .arg("--listen")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log));
    // The daemon outlives this process, so detach it from our session.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| {
        CoreError::Backend("node not found: the betterwright engine requires node >= 22".into())
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CoreError::Backend("sidecar stdout unavailable".into()))?;
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + READY;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let value: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
                if value["error"].as_str() == Some("module-not-found") {
                    let _ = child.kill();
                    return Err(CoreError::Backend(INSTALL_HINT.into()));
                }
                if value["event"].as_str() == Some("ready") {
                    if let Some(found) = value["version"].as_str().filter(|v| too_old(v)) {
                        let _ = child.kill();
                        return Err(CoreError::Backend(stale_version(found)));
                    }
                    std::fs::write(
                        runtime_dir().join("bw-session.pid"),
                        child.id().to_string(),
                    )
                    .ok();
                    return Ok(());
                }
            }
        }
    }
    let _ = child.kill();
    Err(CoreError::Backend(format!(
        "betterwright daemon failed to start; see {}",
        log_path.display()
    )))
}

/// Stop the persistent browser. Returns whether one was running.
pub fn stop_daemon() -> Result<bool, CoreError> {
    let socket = bw_socket_path();
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        let _ = std::fs::remove_file(&socket);
        return Ok(false);
    };
    let _ = stream.write_all(b"{\"id\":0,\"op\":\"shutdown\"}\n");
    let _ = stream.flush();
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut ack = String::new();
    let _ = BufReader::new(&stream).read_line(&mut ack);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if UnixStream::connect(&socket).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Ok(pid) = std::fs::read_to_string(runtime_dir().join("bw-session.pid")) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(runtime_dir().join("bw-session.pid"));
    Ok(true)
}

/// One line describing the persistent browser's state.
pub fn daemon_status() -> String {
    let socket = bw_socket_path();
    if UnixStream::connect(&socket).is_ok() {
        format!("betterwright browser running at {}", socket.display())
    } else {
        "no betterwright browser".into()
    }
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
    use std::io::Read;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_owned()).collect()
    }

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
    fn click_snippet_resolves_refs_through_human() {
        let expected = format!(
            "await human.click(page.locator({}))",
            serde_json::to_string("aria-ref=e12").unwrap()
        );
        assert_eq!(format!("await human.click({})", locator("e12")), expected);
        assert_eq!(
            format!("await human.click({})", locator("f1e7")),
            format!(
                "await human.click(page.locator({}))",
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
    fn snapshot_options_table() {
        assert_eq!(
            snapshot_options(&[]).unwrap().to_string(),
            r#"{"interactive":true}"#
        );
        assert_eq!(
            snapshot_options(&args(&["full"])).unwrap().to_string(),
            r#"{"maxChars":20000}"#
        );
        assert_eq!(
            snapshot_options(&args(&["diff"])).unwrap().to_string(),
            r#"{"diff":true,"interactive":true}"#
        );
        assert_eq!(
            snapshot_options(&args(&["ref=e31"])).unwrap().to_string(),
            r#"{"maxChars":20000,"ref":"e31"}"#
        );
        assert_eq!(
            snapshot_options(&args(&["selector=#main", "depth=3", "urls", "max=5000"]))
                .unwrap()
                .to_string(),
            r##"{"depth":3,"maxChars":5000,"selector":"#main","urls":true}"##
        );
        assert_eq!(
            snapshot_options(&args(&["maxChars=15000"])).unwrap().to_string(),
            r#"{"interactive":true,"maxChars":15000}"#
        );
        let Err(CoreError::Parse(message)) = snapshot_options(&args(&["bogus"])) else {
            panic!("bogus option must be a parse error");
        };
        assert!(message.contains("selector=<css>"), "{message}");
    }
    #[test]
    fn a_selector_that_matches_nothing_is_exit_3_not_a_backend_error() {
        // Playwright's phrasing for an empty scoped read; it must classify as
        // SelectorNotFound (exit 3), not Backend (exit 9).
        assert!(matches!(
            map_bw_error("locator.ariaSnapshot: Selector \"list\" does not match any element"),
            CoreError::SelectorNotFound(_)
        ));
    }
    #[test]
    fn scoped_read_errors_explain_the_next_move() {
        let mut dump =
            String::from("strict mode violation: locator('ul') resolved to 27 elements:");
        for i in 1..=27 {
            dump.push_str(&format!("\n    {i}) <ul class=\"x{i}\">…</ul> aka locator('ul')"));
        }
        let CoreError::SelectorAmbiguous(message) =
            scoped_read_help(CoreError::SelectorAmbiguous(dump))
        else {
            panic!("ambiguity must stay ambiguity");
        };
        assert!(message.contains("matched 27 elements"), "{message}");
        assert!(message.contains("web read ref=eN"), "{message}");
        assert!(message.contains("… and 24 more listed"), "{message}");
        assert!(!message.contains("x27"), "the dump must be clamped: {message}");

        let CoreError::SelectorNotFound(message) = scoped_read_help(CoreError::SelectorNotFound(
            "Selector \"list\" does not match any element".into(),
        )) else {
            panic!("a miss must stay a miss");
        };
        assert!(message.contains("run a plain `web read`"), "{message}");

        // Unrelated errors pass through untouched.
        assert!(matches!(
            scoped_read_help(CoreError::Timeout("slow".into())),
            CoreError::Timeout(_)
        ));
    }
    #[test]
    fn known_stumbles_get_help_lines() {
        let helped = with_help(
            CoreError::Backend("document is not defined".into()),
            "is not defined",
            "use page.evaluate",
        );
        assert!(matches!(
            helped,
            CoreError::Backend(message) if message.contains("help: use page.evaluate")
        ));
        let untouched = with_help(
            CoreError::Timeout("slow".into()),
            "is not defined",
            "irrelevant",
        );
        assert!(matches!(untouched, CoreError::Timeout(message) if message == "slow"));
    }
    #[test]
    fn captcha_bounds_are_numbers_only() {
        assert_eq!(
            bounds(&args(&["10", "20", "300", "80"]), "test")
                .unwrap()
                .unwrap(),
            "{x:10,y:20,width:300,height:80}"
        );
        assert_eq!(bounds(&[], "test").unwrap(), None);
        assert!(matches!(
            bounds(&args(&["10", "alert(1)"]), "test"),
            Err(CoreError::Parse(_))
        ));
        assert!(matches!(
            bounds(&args(&["10", "20"]), "test"),
            Err(CoreError::Parse(_))
        ));
    }
    #[test]
    fn expressive_results_table() {
        let page = json!({"type":"Page","pageId":"page-1","url":"https://a.example/x","title":"A"});
        assert_eq!(
            expressive_result(&page),
            r#"page page-1 https://a.example/x "A""#
        );
        assert_eq!(
            expressive_result(&json!([page, "Jason Test"])),
            "page page-1 https://a.example/x \"A\"\nvalue \"Jason Test\""
        );
        assert_eq!(
            expressive_result(&json!([page, null])),
            r#"page page-1 https://a.example/x "A""#
        );
        assert_eq!(expressive_result(&json!("plain")), "plain");
        // The snippet returns the current tab's INDEX, not the page object —
        // `page` is inside `pages`, and repeating it makes betterwright's
        // serializer emit "[Circular]".
        let listing = pages_listing(&json!([
            1,
            [
                {"type":"Page","pageId":"page-1","url":"https://a.example","title":"A"},
                {"type":"Page","pageId":"page-2","url":"https://b.example","title":"B"}
            ]
        ]));
        assert_eq!(
            listing,
            "  page page-1 https://a.example \"A\"\n* page page-2 https://b.example \"B\""
        );
    }
    #[test]
    fn miss_help_names_the_nearest_candidates() {
        let tree = "page page-1 https://acme.example \"Acme – Login\"\n\
                    - link \"Sign In\" [ref=e12]\n\
                    - button \"Sign in with SSO\" [ref=e15]\n\
                    - link \"Forgot password\" [ref=e17]\n\
                    - textbox \"Email\" [ref=e19]";
        let help = compose_miss_help("resolved to 0 elements", "text=Sign in", tree);
        assert!(help.starts_with("resolved to 0 elements\non page page-1"));
        assert!(help.contains("nearest interactive elements:"));
        assert!(help.contains("link \"Sign In\" [ref=e12]"));
        assert!(help.contains("button \"Sign in with SSO\" [ref=e15]"));
        assert!(!help.contains("textbox \"Email\""));
        assert!(help.contains("help: act on a `[ref=eN]`"));

        let stale = compose_miss_help("resolved to 0 elements", "e99", tree);
        assert!(stale.contains("interactive elements on this page:"));
        assert!(stale.contains("textbox \"Email\" [ref=e19]"));
        assert!(stale.contains("help: refs are reassigned"));
    }
    #[test]
    fn page_refs_pass_indexes_and_quote_ids() {
        assert_eq!(page_ref("2"), "2");
        assert_eq!(page_ref("page-3"), "\"page-3\"");
        assert_eq!(page_ref("x\");boom//"), r#""x\");boom//""#);
    }
    #[test]
    fn version_gate_table() {
        assert!(too_old("1.6.2"));
        assert!(too_old("1.5.9"));
        assert!(too_old("0.9.12"));
        assert!(!too_old("1.6.3"));
        assert!(!too_old("1.6.10"));
        assert!(!too_old("1.7.0"));
        assert!(!too_old("2.0.0"));
        assert!(!too_old("1.6.3-beta.1"));
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
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.3"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"id":%s,"ok":true,"result":"pong","artifacts":[],"challenges":[],"error":null}\n' "$id"
done
"#,
            "echo",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None, None).unwrap();
        let out = worker
            .action("read", &[], Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.as_deref(), Some("pong"));
    }
    #[test]
    fn stale_betterwright_is_refused_at_spawn() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.2"}\n'
sleep 3600
"#,
            "stale",
        );
        let Err(error) = BwWorker::spawn_with_program("sh", &[&script], None, None, None) else {
            panic!("a stale betterwright must be refused");
        };
        assert!(matches!(
            error,
            CoreError::Backend(message) if message.contains("1.6.2") && message.contains("npm i -g betterwright@1.6.3")
        ));
    }
    #[test]
    fn versionless_ready_still_spawns() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":1}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"id":%s,"ok":true}\n' "$id"
done
"#,
            "bare",
        );
        assert!(BwWorker::spawn_with_program("sh", &[&script], None, None, None).is_ok());
    }
    #[test]
    fn warnings_trail_the_output() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.3"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"id":%s,"ok":true,"result":"body","warnings":["stealth runtime fix active"],"challenges":[]}\n' "$id"
done
"#,
            "warn",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None, None).unwrap();
        let out = worker.action("read", &[], Duration::from_secs(5)).unwrap();
        assert_eq!(
            out.as_deref(),
            Some("body\n#warn stealth runtime fix active")
        );
    }
    #[test]
    fn host_call_wraps_client_methods() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.3"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"liveViewStatus"'*)
      printf '{"id":%s,"ok":true,"result":{"ok":true,"running":false}}\n' "$id" ;;
    *)
      printf '{"id":%s,"ok":true,"result":null}\n' "$id" ;;
  esac
done
"#,
            "call",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None, None).unwrap();
        let out = worker
            .action("view", &args(&["status"]), Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.as_deref(), Some(r#"{"ok":true,"running":false}"#));
    }
    #[test]
    fn simple_actions_report_the_landing_page() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.3"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  printf '{"id":%s,"ok":true,"result":{"type":"Page","pageId":"page-1","url":"https://a.example/","title":"A"}}\n' "$id"
done
"#,
            "landing",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None, None).unwrap();
        let out = worker
            .action("goto", &args(&["https://a.example/"]), Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.as_deref(), Some(r#"page page-1 https://a.example/ "A""#));
    }
    #[test]
    fn a_missed_target_gets_candidates_in_the_error() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.3"}\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *snapshot*)
      printf '{"id":%s,"ok":true,"result":"page page-1 https://acme.example \\"Login\\"\\n- link \\"Sign In\\" [ref=e12]"}\n' "$id" ;;
    *human.click*)
      printf '{"id":%s,"ok":false,"error":"locator resolved to 0 elements"}\n' "$id" ;;
    *)
      printf '{"id":%s,"ok":true}\n' "$id" ;;
  esac
done
"#,
            "miss",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None, None).unwrap();
        let error = worker
            .action("click", &args(&["text=Sign in"]), Duration::from_secs(5))
            .unwrap_err();
        let CoreError::SelectorNotFound(message) = error else {
            panic!("expected SelectorNotFound");
        };
        assert!(message.contains("resolved to 0 elements"));
        assert!(message.contains("on page page-1 https://acme.example"));
        assert!(message.contains(r#"link "Sign In" [ref=e12]"#));
        assert!(message.contains("help:"));
    }
    #[test]
    fn dropping_a_daemon_client_leaves_the_browser_running() {
        // The browser belongs to the daemon and is shared with the next koto
        // invocation, so dropping a client must send nothing — no `close`,
        // no kill. (An owned sidecar, by contrast, is torn down; that path is
        // covered by `deadline_kills_the_child`.)
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (_sender, lines) = channel();
        let worker = BwWorker {
            transport: Transport::Daemon { socket: ours },
            lines,
            next: 1,
        };
        drop(worker);
        theirs
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut seen = Vec::new();
        // The peer sees EOF (our end closed) and nothing else — crucially not
        // a close op that would end the shared browser.
        let _ = (&theirs).read_to_end(&mut seen);
        assert!(
            seen.is_empty(),
            "daemon client wrote on drop: {}",
            String::from_utf8_lossy(&seen)
        );
    }
    #[test]
    fn deadline_kills_the_child() {
        let script = fake_sidecar(
            r#"printf '{"id":0,"event":"ready","protocol":2,"version":"1.6.3"}\n'
IFS= read -r line
printf '{"id":1,"ok":true}\n'
sleep 3600
"#,
            "stall",
        );
        let mut worker = BwWorker::spawn_with_program("sh", &[&script], None, None, None).unwrap();
        let error = worker
            .call(json!({"op":"run"}), Duration::from_millis(200))
            .unwrap_err();
        assert!(matches!(error, CoreError::Timeout(op) if op == "betterwright run"));
        let Transport::Owned { child, .. } = &mut worker.transport else {
            panic!("test worker must own its sidecar");
        };
        let mut gone = false;
        for _ in 0..50 {
            if child.try_wait().ok().flatten().is_some() {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "child should be killed after the deadline");
    }
}
