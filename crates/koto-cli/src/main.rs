use clap::{Parser, ValueEnum};
use koto_core::{
    CoreError, DEFAULT_OP_BUDGET, Execution, ObserveMode, Program, Vm, parse_inline, parse_script,
};
use koto_hypr::HyprBackend;
use koto_policy::{Policy, split_capabilities};
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

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("#koto error exit={} reason={}", exit_code(&error), error);
            ExitCode::from(exit_code(&error) as u8)
        }
    }
}
fn run(cli: Cli) -> Result<(), CoreError> {
    let mut program = load_program(&cli)?;
    apply_observe_policy(&mut program, cli.observe.into());
    if cli.explain {
        print_program(&program, cli.format);
        return Ok(());
    }
    if cli.dry_run {
        resolve_dry_run(&program)?;
        print_plan(&program, cli.format);
        return Ok(());
    }
    let policy = Policy::default_profile();
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
    let mut backend = HyprBackend;
    let mut vm = Vm {
        backend: &mut backend,
        capabilities,
        op_budget: cli.budget_ops,
        time_budget: cli.budget_time,
        default_timeout: cli.timeout,
    };
    let execution = vm.run(&program)?;
    if let Some(path) = cli.trace {
        append_trace(&path, &execution)?;
    }
    print_execution(&execution, cli.format, cli.seat);
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
    let backend = HyprBackend;
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
            serde_json::json!({"status":"ok", "exit":0, "ops":execution.trace.len(), "seat":seat, "observation":execution.observation, "trace":execution.trace})
        ),
        Format::Agent => {
            println!("#koto ok ops={} seat={:?}", execution.trace.len(), seat);
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
        CoreError::Unsupported(_) => 9,
    }
}
