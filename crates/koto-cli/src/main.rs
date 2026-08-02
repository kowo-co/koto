use clap::{Parser, ValueEnum};
use koto_core::{
    Backend, CoreError, DEFAULT_OP_BUDGET, Execution, Observation, ObserveMode, Program, Vm, Wait,
    parse_inline, parse_script,
};
use koto_hypr::HyprBackend;
use koto_policy::{Policy, default_path, split_capabilities};
use koto_tmux::Tmux;
use serde::Serialize;
use std::{fs, process::ExitCode, time::Duration};

#[derive(Parser, Debug)]
#[command(
    name = "koto",
    version,
    about = "Keyboard-first control for Hyprland",
    trailing_var_arg = true
)]
struct Cli {
    #[arg(long, value_enum, default_value_t = Format::Agent)]
    format: Format,
    #[arg(long, value_enum, default_value_t = Observe::Auto)]
    observe: Observe,
    #[arg(long, default_value = "10s", value_parser = duration)]
    timeout: Duration,
    #[arg(long, default_value_t = DEFAULT_OP_BUDGET)]
    budget_ops: u32,
    #[arg(long, default_value = "120s", value_parser = duration)]
    budget_time: Duration,
    #[arg(long, value_delimiter = ',')]
    allow: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    deny: Vec<String>,
    #[arg(long, value_enum, default_value_t = Seat::Auto)]
    seat: Seat,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    explain: bool,
    #[arg(long)]
    trace: Option<std::path::PathBuf>,
    #[arg(long, default_value = "default")]
    session: String,
    #[arg(long, default_value = "default")]
    profile: String,
    #[arg(long = "script", short = 's')]
    scripts: Vec<std::path::PathBuf>,
    #[arg(long = "scripts", value_delimiter = ',')]
    scripts_alias: Vec<std::path::PathBuf>,
    #[arg(allow_hyphen_values = true)]
    instruction: Vec<String>,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Agent,
    Json,
    Raw,
    Quiet,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Observe {
    Auto,
    Text,
    Image,
    Both,
}
impl From<Observe> for ObserveMode {
    fn from(value: Observe) -> Self {
        match value {
            Observe::Auto => Self::Auto,
            Observe::Text => Self::Text,
            Observe::Image => Self::Image,
            Observe::Both => Self::Both,
        }
    }
}
#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Seat {
    Host,
    Nested,
    Auto,
}

struct Runtime {
    hypr: HyprBackend,
    tmux: Tmux,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            hypr: HyprBackend::default(),
            tmux: Tmux::default(),
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.hypr.release_held();
    }
}
impl Backend for Runtime {
    fn key(&mut self, keys: &[String]) -> Result<(), CoreError> {
        self.hypr.key(keys)
    }
    fn key_state(&mut self, keys: &[String], pressed: bool) -> Result<(), CoreError> {
        self.hypr.key_state(keys, pressed)
    }
    fn pointer(&mut self, action: &str, args: &[String]) -> Result<(), CoreError> {
        self.hypr.pointer(action, args)
    }
    fn text(&mut self, text: &str, paste: bool) -> Result<(), CoreError> {
        self.hypr.text(text, paste)
    }
    fn wait(&mut self, wait: &Wait, timeout: Duration) -> Result<(), CoreError> {
        if wait.kind != "text" {
            return self.hypr.wait(wait, timeout);
        }
        let limit = wait
            .timeout
            .as_deref()
            .map(duration)
            .transpose()
            .map_err(CoreError::Parse)?
            .unwrap_or(timeout);
        let pattern = regex::Regex::new(&wait.value)
            .map_err(|error| CoreError::Parse(format!("invalid text regex: {error}")))?;
        let started = std::time::Instant::now();
        loop {
            if let Ok(observation) = self.observe(ObserveMode::Text) {
                if observation
                    .text
                    .as_deref()
                    .is_some_and(|text| pattern.is_match(text))
                {
                    return Ok(());
                }
            }
            if started.elapsed() >= limit {
                return Err(CoreError::Timeout(format!("wait text {}", wait.value)));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn focus(&mut self, selector: &str) -> Result<(), CoreError> {
        self.hypr.focus(selector)
    }
    fn observe(&mut self, mode: ObserveMode) -> Result<Observation, CoreError> {
        let image = if matches!(mode, ObserveMode::Image | ObserveMode::Both) {
            Some(
                koto_observe::screencopy::capture_png(&observation_image_path())?
                    .display()
                    .to_string(),
            )
        } else {
            None
        };
        if mode == ObserveMode::Image {
            return Ok(Observation {
                source: "pixels".into(),
                fidelity: "image".into(),
                text: None,
                image,
            });
        }
        let mut observation = if let Ok(text) = self.tmux.read_active(None) {
            if !text.is_empty() {
                Observation {
                    source: "tmux".into(),
                    fidelity: "exact".into(),
                    text: Some(text),
                    image: None,
                }
            } else {
                self.hypr.observe(mode)?
            }
        } else {
            self.hypr.observe(mode)?
        };
        observation.image = image;
        Ok(observation)
    }
    fn list(&mut self, subject: &str) -> Result<String, CoreError> {
        self.hypr.list(subject)
    }
    fn pane(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        match action {
            "new" => Ok(Some(self.tmux.new_pane(args.first().map(String::as_str))?)),
            "send" => {
                self.tmux.send(&args.join(" "), false)?;
                Ok(None)
            }
            "run" => {
                self.tmux.send(&args.join(" "), true)?;
                Ok(None)
            }
            "read" => Ok(Some(
                self.tmux.read(
                    args.first()
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(|_| CoreError::Parse("pane read needs an integer".into()))?,
                )?,
            )),
            "wait" => {
                self.tmux.wait(
                    args.first()
                        .ok_or_else(|| CoreError::Parse("pane wait needs a pattern".into()))?,
                    timeout,
                )?;
                Ok(None)
            }
            "kill" => {
                self.tmux.kill(args.first().map(String::as_str))?;
                Ok(None)
            }
            _ => Err(CoreError::Parse(format!("unknown pane action `{action}`"))),
        }
    }
    fn spawn(&mut self, command: &[String]) -> Result<String, CoreError> {
        self.hypr.spawn(command)
    }
    fn kill(&mut self, selector: &str) -> Result<(), CoreError> {
        self.hypr.kill(selector)
    }
    fn metadata(&mut self, field: &str) -> Result<String, CoreError> {
        self.hypr.metadata(field)
    }
    fn selector_count(&mut self, selector: &str) -> Result<usize, CoreError> {
        Ok(self.hypr.resolve_all(selector)?.len())
    }
    fn window(&mut self, action: &str, args: &[String]) -> Result<(), CoreError> {
        self.hypr.window(action, args)
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("#koto error exit={} reason={}", exit_code(&error), error);
            ExitCode::from(exit_code(&error) as u8)
        }
    }
}
fn run(cli: Cli) -> Result<i32, CoreError> {
    if cli.instruction.as_slice() == ["abort"] {
        request_abort()?;
        return Ok(0);
    }
    if cli.instruction.as_slice() == ["install-kill-switch"] {
        install_kill_switch()?;
        return Ok(0);
    }
    let marker = abort_path();
    let _ = fs::remove_file(&marker);
    if cli.instruction.len() == 2 && cli.instruction[0] == "stdlib" && cli.instruction[1] == "sync"
    {
        sync_stdlib()?;
        return Ok(0);
    }
    let mut program = load_program(&cli)?;
    apply_observe_policy(&mut program, cli.observe.into());
    if cli.explain {
        print_program(&program, cli.format);
        return Ok(0);
    }
    if cli.dry_run {
        resolve_dry_run(&program)?;
        print_plan(&program, cli.format);
        return Ok(0);
    }
    let (policy, profile) = Policy::load(&default_path(), &cli.profile)?;
    let capabilities = policy.effective(
        &split_capabilities(&cli.allow),
        &split_capabilities(&cli.deny),
    );
    // Require declarations fail before instruction zero, including those in later concatenated files.
    let required: Vec<String> = program
        .instructions
        .iter()
        .filter_map(|instruction| match &instruction.op {
            koto_core::Op::Require(caps) => Some(caps.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    policy.require_all(&capabilities, &required)?;
    let registers = load_session(&cli.session)?;
    let mut backend = Runtime::default();
    let mut vm = Vm {
        backend: &mut backend,
        capabilities,
        op_budget: profile.budget_ops.unwrap_or(cli.budget_ops),
        time_budget: profile
            .budget_time
            .as_deref()
            .map(duration)
            .transpose()
            .map_err(CoreError::Parse)?
            .unwrap_or(cli.budget_time),
        default_timeout: cli.timeout,
        registers,
        cancel_file: Some(abort_path()),
    };
    let execution = vm.run(&program)?;
    save_session(&cli.session, &execution.registers)?;
    if let Some(path) = cli.trace {
        append_trace(&path, &execution)?;
    }
    print_execution(&execution, cli.format, cli.seat);
    Ok(execution.registers.status)
}
fn observation_image_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("koto/obs");
    base.join(format!(
        "{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ))
}
fn abort_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir())
        .join("koto/abort")
}
fn request_abort() -> Result<(), CoreError> {
    let path = abort_path();
    fs::create_dir_all(path.parent().unwrap())
        .map_err(|error| CoreError::Backend(error.to_string()))?;
    fs::write(path, b"abort\n").map_err(|error| CoreError::Backend(error.to_string()))
}
fn install_kill_switch() -> Result<(), CoreError> {
    let binding = "SUPER CTRL SHIFT, ESCAPE, exec, koto abort";
    let status = std::process::Command::new("hyprctl")
        .args(["keyword", "bind", binding])
        .status()
        .map_err(|error| CoreError::Backend(format!("hyprctl unavailable: {error}")))?;
    if !status.success() {
        return Err(CoreError::Backend(
            "could not install Hyprland kill switch".into(),
        ));
    }
    let config = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join(".config/hypr/koto.conf");
    if let Some(parent) = config.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::Backend(error.to_string()))?;
    }
    fs::write(&config, format!("# managed by koto\nbind = {binding}\n"))
        .map_err(|error| CoreError::Backend(error.to_string()))?;
    println!("installed kill switch: SUPER+CTRL+SHIFT+ESC");
    Ok(())
}
fn sync_stdlib() -> Result<(), CoreError> {
    let config = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join(".config/hypr/bindings.conf");
    let source = fs::read_to_string(&config)
        .map_err(|error| CoreError::Backend(format!("{}: {error}", config.display())))?;
    let mut output = format!(
        "; auto-generated by koto stdlib sync from {}\n",
        config.display()
    );
    for line in source
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("bind") && line.contains(", exec,"))
    {
        // Hyprland's form is `bind = MODS, KEY, exec, COMMAND`.
        let Some((_, binding)) = line.split_once('=') else {
            continue;
        };
        let values: Vec<_> = binding.splitn(4, ',').map(str::trim).collect();
        if values.len() != 4 || values[2] != "exec" {
            continue;
        }
        let modifiers = values[0]
            .replace('$', "")
            .replace("mainMod", "super")
            .replace(' ', "")
            .to_lowercase();
        let key = values[1].to_lowercase();
        let command = values[3];
        let name = command
            .split_whitespace()
            .next()
            .unwrap_or("binding")
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
            .to_lowercase();
        if name.is_empty() {
            continue;
        }
        let target = if command.contains("alacritty") || command.contains("terminal") {
            Some(("Alacritty", "3s", "150ms"))
        } else if command.contains("chrom") || command.contains("browser") {
            Some(("chromium", "5s", "250ms"))
        } else if command.contains("walker") || command.contains("launcher") {
            Some(("walker", "2s", "150ms"))
        } else {
            None
        };
        output.push_str(&format!(
            "\ndef omarchy.{name}()\n  key {modifiers} {key}\n"
        ));
        if let Some((class, timeout, idle)) = target {
            output.push_str(&format!(
                "  wait window class={class} timeout {timeout}\n  wait idle {idle}\n"
            ));
        } else {
            output.push_str("  wait idle 150ms\n");
        }
        output.push_str("enddef\n");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| ".".into())
        .join("koto/stdlib");
    fs::create_dir_all(&base).map_err(|error| CoreError::Backend(error.to_string()))?;
    let destination = base.join("omarchy.basm");
    fs::write(&destination, output).map_err(|error| CoreError::Backend(error.to_string()))?;
    println!("{}", destination.display());
    Ok(())
}
fn load_program(cli: &Cli) -> Result<Program, CoreError> {
    let mut paths = cli.scripts.clone();
    paths.extend(cli.scripts_alias.iter().cloned());
    if paths.is_empty() && cli.instruction.first().is_some_and(|value| value == "-") {
        let mut source = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut source)
            .map_err(|e| CoreError::Backend(e.to_string()))?;
        return parse_script(&source).map_err(|e| CoreError::Parse(e.to_string()));
    }
    if paths.is_empty() {
        return parse_inline(&cli.instruction).map_err(|e| CoreError::Parse(e.to_string()));
    }
    let arguments = &cli.instruction;
    let mut all = Vec::new();
    for path in paths {
        let source = fs::read_to_string(&path)
            .map_err(|e| CoreError::Backend(format!("{}: {e}", path.display())))?;
        let source = expand_includes(
            &source,
            path.parent().unwrap_or_else(|| std::path::Path::new(".")),
            0,
        )?;
        let bound = bind_arguments(&source, arguments);
        let mut program = parse_script(&bound)
            .map_err(|e| CoreError::Parse(format!("{}: {e}", path.display())))?;
        for (offset, instruction) in program.instructions.iter_mut().enumerate() {
            instruction.index = all.len() + offset;
        }
        all.extend(program.instructions);
    }
    Ok(Program { instructions: all })
}
fn expand_includes(source: &str, base: &std::path::Path, depth: u8) -> Result<String, CoreError> {
    if depth >= 16 {
        return Err(CoreError::Parse("include nesting exceeds 16 files".into()));
    }
    let mut expanded = String::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(argument) = trimmed.strip_prefix("include ") {
            let file = argument.trim().trim_matches('"');
            if file.is_empty() || std::path::Path::new(file).is_absolute() || file.contains("..") {
                return Err(CoreError::Parse(format!("unsafe include `{file}`")));
            }
            let path = base.join(file);
            let child = fs::read_to_string(&path)
                .map_err(|error| CoreError::Backend(format!("{}: {error}", path.display())))?;
            expanded.push_str(&expand_includes(
                &child,
                path.parent().unwrap_or(base),
                depth + 1,
            )?);
            expanded.push('\n');
        } else {
            expanded.push_str(line);
            expanded.push('\n');
        }
    }
    Ok(expanded)
}
fn session_path(name: &str) -> Result<std::path::PathBuf, CoreError> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return Err(CoreError::Parse("invalid session name".into()));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })
        .unwrap_or_else(|| ".".into());
    Ok(base.join("koto/sessions").join(format!("{name}.json")))
}
fn load_session(name: &str) -> Result<koto_core::Registers, CoreError> {
    let path = session_path(name)?;
    if !path.exists() {
        return Ok(koto_core::Registers::default());
    }
    let source = fs::read_to_string(&path)
        .map_err(|error| CoreError::Backend(format!("read {}: {error}", path.display())))?;
    serde_json::from_str(&source)
        .map_err(|error| CoreError::Parse(format!("{}: {error}", path.display())))
}
fn save_session(name: &str, registers: &koto_core::Registers) -> Result<(), CoreError> {
    let path = session_path(name)?;
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)
        .map_err(|error| CoreError::Backend(format!("create {}: {error}", parent.display())))?;
    let encoded =
        serde_json::to_vec(registers).map_err(|error| CoreError::Backend(error.to_string()))?;
    fs::write(path, encoded).map_err(|error| CoreError::Backend(error.to_string()))
}
fn apply_observe_policy(program: &mut Program, requested: ObserveMode) {
    if requested == ObserveMode::Auto {
        return;
    }
    for instruction in &mut program.instructions {
        match &mut instruction.op {
            koto_core::Op::End(mode) if *mode == ObserveMode::Auto => *mode = requested,
            koto_core::Op::See { mode, .. } if *mode == ObserveMode::Auto => *mode = requested,
            _ => {}
        }
    }
}
fn bind_arguments(source: &str, arguments: &[String]) -> String {
    let mut result = source.replace("%*", &arguments.join(" "));
    for (index, argument) in arguments.iter().take(9).enumerate() {
        result = result.replace(&format!("%{}", index + 1), argument);
    }
    result
}
fn resolve_dry_run(program: &Program) -> Result<(), CoreError> {
    let backend = HyprBackend::default();
    for instruction in &program.instructions {
        let selector = match &instruction.op {
            koto_core::Op::Focus(selector)
            | koto_core::Op::Click(selector)
            | koto_core::Op::Kill(selector) => Some(selector),
            koto_core::Op::Close(Some(selector)) => Some(selector),
            koto_core::Op::Wait(wait) if matches!(wait.kind.as_str(), "window" | "gone") => {
                Some(&wait.value)
            }
            _ => None,
        };
        if let Some(selector) = selector {
            backend.resolve(selector)?;
        }
    }
    Ok(())
}
fn print_program(program: &Program, format: Format) {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(program).unwrap()),
        Format::Quiet => {}
        _ => {
            for instruction in &program.instructions {
                println!("{:03} {:?}", instruction.index, instruction.op);
            }
        }
    }
}
fn print_plan(program: &Program, format: Format) {
    match format {
        Format::Json => println!(
            "{}",
            serde_json::json!({"status":"dry-run", "instructions":program.instructions})
        ),
        Format::Quiet => {}
        _ => {
            println!("#koto dry-run ops={}", program.instructions.len());
            for instruction in &program.instructions {
                println!("{:03} {:?}", instruction.index, instruction.op);
            }
        }
    }
}
fn print_execution(execution: &Execution, format: Format, seat: Seat) {
    match format {
        Format::Quiet => {}
        Format::Raw => {
            if let Some(observation) = &execution.observation {
                print!("{}", observation.text.as_deref().unwrap_or(""));
            } else {
                print!("{}", execution.registers.out);
            }
        }
        Format::Json => println!(
            "{}",
            serde_json::json!({"status": if execution.registers.status == 0 { "ok" } else { "halted" }, "exit":execution.registers.status, "ops":execution.trace.len(), "elapsed_ms":execution.elapsed_ms, "seat":seat, "observation":execution.observation, "trace":execution.trace})
        ),
        Format::Agent => {
            println!(
                "#koto {} ops={} t={}ms seat={:?}",
                if execution.registers.status == 0 {
                    "ok"
                } else {
                    "halted"
                },
                execution.trace.len(),
                execution.elapsed_ms,
                seat
            );
            if let Some(observation) = &execution.observation {
                println!(
                    "source {} fidelity={}",
                    observation.source, observation.fidelity
                );
                println!("---");
                if let Some(text) = &observation.text {
                    println!("{text}");
                }
                if let Some(image) = &observation.image {
                    println!("image {image}");
                }
            } else if !execution.registers.out.is_empty() {
                println!("---\n{}", execution.registers.out);
            }
        }
    }
}
fn append_trace(path: &std::path::Path, execution: &Execution) -> Result<(), CoreError> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    for entry in &execution.trace {
        serde_json::to_writer(&mut file, entry).map_err(|e| CoreError::Backend(e.to_string()))?;
        writeln!(file).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    Ok(())
}
fn duration(input: &str) -> Result<Duration, String> {
    let (number, unit) = if let Some(value) = input.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = input.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = input.strip_suffix('m') {
        (value, 60_000)
    } else {
        return Err("duration must end in ms, s, or m".into());
    };
    Ok(Duration::from_millis(
        number
            .parse::<u64>()
            .map_err(|_| "invalid duration")?
            .checked_mul(unit)
            .ok_or("duration is too large")?,
    ))
}
fn exit_code(error: &CoreError) -> i32 {
    match error {
        CoreError::Parse(_) => 8,
        CoreError::Assertion(_) => 1,
        CoreError::Timeout(_) => 2,
        CoreError::SelectorNotFound(_) => 3,
        CoreError::Budget(_) => 4,
        CoreError::Capability(_) => 5,
        CoreError::SelectorAmbiguous(_) => 6,
        CoreError::ObservationUnavailable(_) => 7,
        CoreError::Backend(_) => 9,
        CoreError::Unsupported(_) | CoreError::Aborted => 9,
    }
}
