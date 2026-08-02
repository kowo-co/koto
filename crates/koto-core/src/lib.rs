//! The deterministic basm parser and execution primitives.
//!
//! This crate deliberately has no compositor or input dependencies.  It is the
//! stable part of koto's public contract; platform implementations live behind
//! [`Backend`].

use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, time::Duration};
use thiserror::Error;

pub const DEFAULT_OP_BUDGET: u32 = 256;
pub const DEFAULT_TIME_BUDGET: Duration = Duration::from_secs(120);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ParseError {
    #[error("line {line}: {message}")]
    Line { line: usize, message: String },
    #[error("inline token {token}: {message}")]
    Inline { token: usize, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub instructions: Vec<Instruction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    /// Zero based index used in traces and deterministic errors.
    pub index: usize,
    pub line: usize,
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Key(Vec<String>),
    Tap(String),
    Hold(Vec<String>),
    Release(Vec<String>),
    Type(String),
    Paste(String),
    Click(String),
    Scroll {
        direction: String,
        count: u32,
    },
    End(ObserveMode),
    See {
        register: Option<String>,
        mode: ObserveMode,
    },
    Peek(String),
    Ocr,
    Wait(Wait),
    Focus(String),
    Workspace(String),
    SendWorkspace(String),
    Close(Option<String>),
    WindowAction(String),
    Swap(String),
    Move(String),
    Monitor(String),
    List(String),
    Spawn(Vec<String>),
    Kill(String),
    Pane {
        action: String,
        args: Vec<String>,
    },
    Web {
        action: String,
        args: Vec<String>,
    },
    Label(String),
    Jump {
        kind: String,
        args: Vec<String>,
    },
    Rep(u32),
    While {
        predicate: String,
        max: u32,
    },
    Call(String),
    Ret,
    Def(String),
    EndDef,
    Include(String),
    Require(Vec<String>),
    Assert(String),
    Expect(String),
    Budget {
        kind: String,
        value: String,
    },
    Checkpoint(String),
    Rollback(String),
    Note(String),
    Nop,
    Halt(i32),
    BlockEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ObserveMode {
    #[default]
    Auto,
    Text,
    Image,
    Json,
    Both,
    Silent,
}

impl ObserveMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            "json" => Some(Self::Json),
            "both" => Some(Self::Both),
            "silent" => Some(Self::Silent),
            _ => None,
        }
    }
}

/// The documented selector representation.  Backends must resolve it strictly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub terms: Vec<SelectorTerm>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectorTerm {
    pub field: String,
    pub operator: SelectorOperator,
    pub value: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectorOperator {
    Exact,
    Regex,
    Bare,
}

impl Selector {
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut terms = Vec::new();
        for term in split_unquoted(input, ',') {
            let term = term.trim();
            if term.is_empty() {
                return Err("empty selector term".into());
            }
            if term == "focused" || term == "last" {
                terms.push(SelectorTerm {
                    field: term.into(),
                    operator: SelectorOperator::Bare,
                    value: String::new(),
                });
                continue;
            }
            let (field, operator, value) = if let Some((f, v)) = term.split_once('~') {
                (f, SelectorOperator::Regex, v)
            } else if let Some((f, v)) = term.split_once('=') {
                (f, SelectorOperator::Exact, v)
            } else {
                return Err(format!("expected = or ~ in selector `{term}`"));
            };
            if field.is_empty() || value.is_empty() {
                return Err(format!("invalid selector `{term}`"));
            }
            let value = unquote(value);
            if operator == SelectorOperator::Regex {
                regex::Regex::new(value)
                    .map_err(|error| format!("invalid regex in `{term}`: {error}"))?;
            }
            terms.push(SelectorTerm {
                field: field.into(),
                operator,
                value: value.into(),
            });
        }
        Ok(Self { terms })
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, term) in self.terms.iter().enumerate() {
            if i != 0 {
                write!(f, ",")?;
            }
            match term.operator {
                SelectorOperator::Exact => write!(f, "{}={}", term.field, term.value)?,
                SelectorOperator::Regex => write!(f, "{}~{}", term.field, term.value)?,
                SelectorOperator::Bare => write!(f, "{}", term.field)?,
            }
        }
        Ok(())
    }
}

/// Parses semicolon-commented basm.  A script instruction is one physical line;
/// braces and labels are instructions so later compilation can build control flow
/// without reparsing source.
pub fn parse_script(source: &str) -> Result<Program, ParseError> {
    let mut instructions = Vec::new();
    for (line_number, source_line) in source.lines().enumerate() {
        let line = strip_comment(source_line).trim();
        if line.is_empty() {
            continue;
        }
        let tokens = lex(line).map_err(|message| ParseError::Line {
            line: line_number + 1,
            message,
        })?;
        let op = parse_tokens(&tokens).map_err(|message| ParseError::Line {
            line: line_number + 1,
            message,
        })?;
        instructions.push(Instruction {
            index: instructions.len(),
            line: line_number + 1,
            op,
        });
    }
    validate_structure(&instructions).map_err(|message| ParseError::Line { line: 0, message })?;
    Ok(Program { instructions })
}

/// Parses argv's instruction stream. Bare key tokens imply `key`; the End-key
/// collision is resolved exactly as specified: bare boundary `end` terminates,
/// while `key end` and `ctrl end` are chords.
pub fn parse_inline(tokens: &[String]) -> Result<Program, ParseError> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let start = i;
        let token = tokens[i].as_str();
        let (op, consumed) = if is_mnemonic(token) {
            parse_inline_mnemonic(&tokens[i..]).map_err(|message| ParseError::Inline {
                token: i + 1,
                message,
            })?
        } else {
            let mut keys = Vec::new();
            while i + keys.len() < tokens.len() && is_modifier(&tokens[i + keys.len()]) {
                keys.push(tokens[i + keys.len()].clone());
            }
            if i + keys.len() >= tokens.len() || is_mnemonic(&tokens[i + keys.len()]) {
                return Err(ParseError::Inline {
                    token: i + 1,
                    message: "expected a non-modifier key".into(),
                });
            }
            keys.push(tokens[i + keys.len()].clone());
            (Op::Key(keys.clone()), keys.len())
        };
        result.push(Instruction {
            index: result.len(),
            line: start + 1,
            op,
        });
        i += consumed;
    }
    Ok(Program {
        instructions: result,
    })
}

fn parse_inline_mnemonic(tokens: &[String]) -> Result<(Op, usize), String> {
    let word = tokens[0].as_str();
    let arg = |n: usize| {
        tokens
            .get(n)
            .map(String::as_str)
            .ok_or_else(|| format!("`{word}` needs an operand"))
    };
    match word {
        "end" => Ok((
            Op::End(
                tokens
                    .get(1)
                    .and_then(|s| ObserveMode::parse(s))
                    .unwrap_or(ObserveMode::Auto),
            ),
            if tokens.get(1).and_then(|s| ObserveMode::parse(s)).is_some() {
                2
            } else {
                1
            },
        )),
        "key" => {
            let key = arg(1)?;
            let mut keys = Vec::new();
            let mut n = 1;
            while let Some(value) = tokens.get(n) {
                keys.push(value.clone());
                n += 1;
                if !is_modifier(value) {
                    break;
                }
            }
            if key.is_empty() {
                Err("`key` needs a key".into())
            } else {
                Ok((Op::Key(keys), n))
            }
        }
        "tap" => Ok((Op::Tap(arg(1)?.into()), 2)),
        "type" => Ok((Op::Type(arg(1)?.into()), 2)),
        "paste" => Ok((Op::Paste(arg(1)?.into()), 2)),
        "focus" => Ok((Op::Focus(arg(1)?.into()), 2)),
        "wait" => Ok((Op::Wait(parse_wait(&tokens[1..])?), tokens.len())),
        "see" => {
            let first = tokens.get(1).map(String::as_str);
            let (register, mode, used) = match first {
                Some(value) if value.starts_with('$') => (
                    Some(value.into()),
                    tokens
                        .get(2)
                        .and_then(|s| ObserveMode::parse(s))
                        .unwrap_or(ObserveMode::Auto),
                    if tokens.get(2).and_then(|s| ObserveMode::parse(s)).is_some() {
                        3
                    } else {
                        2
                    },
                ),
                Some(value) if ObserveMode::parse(value).is_some() => {
                    (None, ObserveMode::parse(value).unwrap(), 2)
                }
                _ => (None, ObserveMode::Auto, 1),
            };
            Ok((Op::See { register, mode }, used))
        }
        _ => Err(format!(
            "inline `{word}` is not yet supported; use --script for multi-operand instructions"
        )),
    }
}

fn parse_tokens(tokens: &[String]) -> Result<Op, String> {
    if tokens.len() == 1 && tokens[0] == "}" {
        return Ok(Op::BlockEnd);
    }
    let word = tokens.first().ok_or("empty instruction")?.as_str();
    let rest = &tokens[1..];
    let one = || {
        rest.first()
            .cloned()
            .ok_or_else(|| format!("`{word}` needs an operand"))
    };
    match word {
        label if label.starts_with('.') && label.ends_with(':') => {
            Ok(Op::Label(label.trim_end_matches(':').into()))
        }
        "key" => chord(rest).map(Op::Key),
        "tap" => Ok(Op::Tap(one()?)),
        "hold" => chord(rest).map(Op::Hold),
        "release" => chord(rest).map(Op::Release),
        "type" => Ok(Op::Type(one()?)),
        "paste" => Ok(Op::Paste(one()?)),
        "click" => Ok(Op::Click(one()?)),
        "scroll" => Ok(Op::Scroll {
            direction: one()?,
            count: rest
                .get(1)
                .map(|n| n.parse())
                .transpose()
                .map_err(|_| "scroll count must be an integer")?
                .unwrap_or(1),
        }),
        "end" => Ok(Op::End(
            rest.first()
                .and_then(|v| ObserveMode::parse(v))
                .unwrap_or(ObserveMode::Auto),
        )),
        "see" => {
            let register = rest.first().filter(|v| v.starts_with('$')).cloned();
            let mode_at = usize::from(register.is_some());
            Ok(Op::See {
                register,
                mode: rest
                    .get(mode_at)
                    .and_then(|v| ObserveMode::parse(v))
                    .unwrap_or(ObserveMode::Auto),
            })
        }
        "peek" => Ok(Op::Peek(one()?)),
        "ocr" => Ok(Op::Ocr),
        "wait" => Ok(Op::Wait(parse_wait(rest)?)),
        "focus" => Ok(Op::Focus(one()?)),
        "ws" => Ok(Op::Workspace(one()?)),
        "send" if rest.first().is_some_and(|v| v == "ws") => Ok(Op::SendWorkspace(
            rest.get(1).cloned().ok_or("`send ws` needs a workspace")?,
        )),
        "close" => Ok(Op::Close(rest.first().cloned())),
        "float" | "tile" | "full" | "pin" => Ok(Op::WindowAction(word.into())),
        "swap" => Ok(Op::Swap(one()?)),
        "move" => Ok(Op::Move(one()?)),
        "monitor" => Ok(Op::Monitor(one()?)),
        "list" => Ok(Op::List(one()?)),
        "spawn" => {
            if rest.is_empty() {
                Err("`spawn` needs a command".into())
            } else {
                Ok(Op::Spawn(rest.into()))
            }
        }
        "kill" => Ok(Op::Kill(one()?)),
        "pane" | "web" => {
            let action = one()?;
            if word == "pane" {
                Ok(Op::Pane {
                    action,
                    args: rest[1..].into(),
                })
            } else {
                Ok(Op::Web {
                    action,
                    args: rest[1..].into(),
                })
            }
        }
        "jmp" | "jz" | "jnz" | "je" => Ok(Op::Jump {
            kind: word.into(),
            args: rest.into(),
        }),
        "rep" => Ok(Op::Rep(
            one()?.parse().map_err(|_| "rep count must be an integer")?,
        )),
        "while" => {
            let max = rest
                .iter()
                .position(|v| v == "max")
                .ok_or("while requires `max <n>`")?;
            let count = rest
                .get(max + 1)
                .ok_or("while requires a bound")?
                .parse()
                .map_err(|_| "while max must be an integer")?;
            Ok(Op::While {
                predicate: rest[..max].join(" "),
                max: count,
            })
        }
        "call" => Ok(Op::Call(one()?)),
        "ret" => Ok(Op::Ret),
        "def" => Ok(Op::Def(one()?)),
        "enddef" => Ok(Op::EndDef),
        "include" => Ok(Op::Include(one()?)),
        "require" => Ok(Op::Require(split_caps(rest)?)),
        "assert" => Ok(Op::Assert(rest.join(" "))),
        "expect" => Ok(Op::Expect(rest.join(" "))),
        "budget" => Ok(Op::Budget {
            kind: one()?,
            value: rest.get(1).cloned().ok_or("budget needs a value")?,
        }),
        "checkpoint" => Ok(Op::Checkpoint(one()?)),
        "rollback" => Ok(Op::Rollback(one()?)),
        "note" => Ok(Op::Note(rest.join(" "))),
        "nop" => Ok(Op::Nop),
        "halt" => Ok(Op::Halt(
            rest.first()
                .map(|v| v.parse())
                .transpose()
                .map_err(|_| "halt code must be an integer")?
                .unwrap_or(0),
        )),
        _ => Err(format!("unknown mnemonic `{word}`")),
    }
}

fn parse_wait(args: &[String]) -> Result<Wait, String> {
    if args.is_empty() {
        return Err("`wait` needs an operand".into());
    }
    let timeout_at = args.iter().position(|v| v == "timeout");
    let values = &args[..timeout_at.unwrap_or(args.len())];
    let timeout = timeout_at
        .map(|i| args.get(i + 1).cloned().ok_or("timeout needs a duration"))
        .transpose()?;
    let (kind, value) = match values {
        [duration] => ("duration".into(), duration.clone()),
        [kind, value @ ..] if !value.is_empty() => (kind.clone(), value.join(" ")),
        _ => return Err("invalid wait instruction".into()),
    };
    Ok(Wait {
        kind,
        value,
        timeout,
    })
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    pub kind: String,
    pub value: String,
    pub timeout: Option<String>,
}
fn chord(values: &[String]) -> Result<Vec<String>, String> {
    if values.is_empty() {
        Err("key instruction needs a chord".into())
    } else if !values.last().is_some_and(|v| !is_modifier(v)) {
        Err("a chord must end in a non-modifier key; use tap for a modifier".into())
    } else {
        Ok(values.into())
    }
}
fn split_caps(values: &[String]) -> Result<Vec<String>, String> {
    let caps: Vec<_> = values
        .iter()
        .flat_map(|v| v.split(','))
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect();
    if caps.is_empty() {
        Err("require needs at least one capability".into())
    } else {
        Ok(caps)
    }
}
fn is_modifier(word: &str) -> bool {
    matches!(word, "super" | "ctrl" | "alt" | "shift")
}
fn is_mnemonic(word: &str) -> bool {
    matches!(
        word,
        "key"
            | "tap"
            | "hold"
            | "release"
            | "type"
            | "paste"
            | "click"
            | "scroll"
            | "end"
            | "see"
            | "peek"
            | "ocr"
            | "wait"
            | "focus"
            | "ws"
            | "send"
            | "close"
            | "float"
            | "tile"
            | "full"
            | "pin"
            | "swap"
            | "move"
            | "monitor"
            | "list"
            | "spawn"
            | "kill"
            | "pane"
            | "web"
            | "jmp"
            | "jz"
            | "jnz"
            | "je"
            | "rep"
            | "while"
            | "call"
            | "ret"
            | "def"
            | "enddef"
            | "include"
            | "require"
            | "assert"
            | "expect"
            | "budget"
            | "checkpoint"
            | "rollback"
            | "note"
            | "nop"
            | "halt"
    )
}
fn validate_structure(instructions: &[Instruction]) -> Result<(), String> {
    let mut blocks = 0;
    let mut definitions = 0;
    for instruction in instructions {
        match instruction.op {
            Op::Rep(_) | Op::While { .. } => blocks += 1,
            Op::BlockEnd => {
                if blocks == 0 {
                    return Err(format!("instruction {} closes no block", instruction.index));
                }
                blocks -= 1
            }
            Op::Def(_) => definitions += 1,
            Op::EndDef => {
                if definitions == 0 {
                    return Err(format!(
                        "instruction {} closes no definition",
                        instruction.index
                    ));
                }
                definitions -= 1
            }
            _ => {}
        }
    }
    if blocks != 0 {
        Err("unclosed control-flow block".into())
    } else if definitions != 0 {
        Err("unclosed definition".into())
    } else {
        Ok(())
    }
}
fn strip_comment(line: &str) -> &str {
    let mut quote = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if c == '"' && !escaped {
            quote = !quote;
        }
        if c == ';' && !quote {
            return &line[..i];
        }
        escaped = c == '\\' && !escaped;
        if c != '\\' {
            escaped = false;
        }
    }
    line
}
fn lex(input: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote = false;
    let mut escaped = false;
    for c in input.chars() {
        if escaped {
            current.push(match c {
                'n' => '\n',
                't' => '\t',
                other => other,
            });
            escaped = false;
            continue;
        }
        if c == '\\' && quote {
            escaped = true;
            continue;
        }
        if c == '"' {
            quote = !quote;
            continue;
        }
        if c.is_whitespace() && !quote {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if quote || escaped {
        return Err("unterminated quoted string".into());
    }
    if !current.is_empty() {
        result.push(current);
    }
    Ok(result)
}
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}
fn split_unquoted(input: &str, needle: char) -> Vec<&str> {
    input.split(needle).collect()
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("basm parse error: {0}")]
    Parse(String),
    #[error("capability denied: {0}")]
    Capability(String),
    #[error("budget exceeded: {0}")]
    Budget(&'static str),
    #[error("instruction timed out: {0}")]
    Timeout(String),
    #[error("selector matched nothing: {0}")]
    SelectorNotFound(String),
    #[error("selector is ambiguous: {0}")]
    SelectorAmbiguous(String),
    #[error("observation unavailable: {0}")]
    ObservationUnavailable(String),
    #[error("backend unavailable: {0}")]
    Backend(String),
    #[error("unsupported in this milestone: {0}")]
    Unsupported(String),
    #[error("assertion failed: {0}")]
    Assertion(String),
}
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub source: String,
    pub fidelity: String,
    pub text: Option<String>,
    pub image: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct Registers {
    pub status: i32,
    pub out: String,
    pub values: std::collections::BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TraceEntry {
    pub i: usize,
    pub op: String,
    pub ms: u128,
    pub status: i32,
    pub warning: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Execution {
    pub observation: Option<Observation>,
    pub registers: Registers,
    pub trace: Vec<TraceEntry>,
    pub halted: bool,
}

pub trait Backend {
    fn key(&mut self, keys: &[String]) -> Result<(), CoreError>;
    fn text(&mut self, text: &str, paste: bool) -> Result<(), CoreError>;
    fn wait(&mut self, wait: &Wait, default_timeout: Duration) -> Result<(), CoreError>;
    fn focus(&mut self, selector: &str) -> Result<(), CoreError>;
    fn observe(&mut self, mode: ObserveMode) -> Result<Observation, CoreError>;
    fn list(&mut self, subject: &str) -> Result<String, CoreError>;
}

pub struct Vm<'a, B: Backend> {
    pub backend: &'a mut B,
    pub capabilities: BTreeSet<String>,
    pub op_budget: u32,
    pub time_budget: Duration,
    pub default_timeout: Duration,
}
impl<'a, B: Backend> Vm<'a, B> {
    pub fn run(&mut self, program: &Program) -> Result<Execution, CoreError> {
        let started = std::time::Instant::now();
        let mut execution = Execution {
            observation: None,
            registers: Registers::default(),
            trace: Vec::new(),
            halted: false,
        };
        for instruction in &program.instructions {
            if instruction.index as u32 >= self.op_budget {
                return Err(CoreError::Budget("operations"));
            }
            if started.elapsed() > self.time_budget {
                return Err(CoreError::Budget("wall clock"));
            }
            let tick = std::time::Instant::now();
            let result = self.execute(&instruction.op, &mut execution);
            let status = if result.is_ok() { 0 } else { 1 };
            execution.registers.status = status;
            execution.trace.push(TraceEntry {
                i: instruction.index,
                op: op_name(&instruction.op).into(),
                ms: tick.elapsed().as_millis(),
                status,
                warning:
                    matches!(&instruction.op, Op::Wait(Wait { kind, .. }) if kind == "duration")
                        .then(|| "raw sleep is discouraged".into()),
            });
            result?;
            if execution.halted {
                break;
            }
        }
        Ok(execution)
    }
    fn execute(&mut self, op: &Op, execution: &mut Execution) -> Result<(), CoreError> {
        match op {
            Op::Key(keys) => {
                self.require("input")?;
                self.backend.key(keys)
            }
            Op::Tap(key) => {
                self.require("input")?;
                self.backend.key(std::slice::from_ref(key))
            }
            Op::Type(text) => {
                self.require("input")?;
                self.backend.text(text, false)
            }
            Op::Paste(text) => {
                self.require("input")?;
                self.backend.text(text, true)
            }
            Op::Wait(wait) => self.backend.wait(wait, self.default_timeout),
            Op::Focus(selector) => {
                self.require("window")?;
                self.backend.focus(selector)
            }
            Op::End(mode) => {
                if *mode != ObserveMode::Silent {
                    let observation = self.backend.observe(*mode)?;
                    execution.registers.out = observation.text.clone().unwrap_or_default();
                    execution.observation = Some(observation);
                }
                execution.halted = true;
                Ok(())
            }
            Op::See { register, mode } => {
                let observation = self.backend.observe(*mode)?;
                let text = observation.text.clone().unwrap_or_default();
                execution.registers.out = text.clone();
                if let Some(register) = register {
                    execution.registers.values.insert(register.clone(), text);
                }
                execution.observation = Some(observation);
                Ok(())
            }
            Op::List(subject) => {
                let output = self.backend.list(subject)?;
                execution.registers.out = output;
                Ok(())
            }
            Op::Require(caps) => {
                for cap in caps {
                    self.require(cap)?;
                }
                Ok(())
            }
            Op::Nop | Op::Label(_) | Op::BlockEnd | Op::EndDef => Ok(()),
            Op::Halt(code) => {
                execution.registers.status = *code;
                execution.halted = true;
                Ok(())
            }
            _ => Err(CoreError::Unsupported(op_name(op).into())),
        }
    }
    fn require(&self, cap: &str) -> Result<(), CoreError> {
        if self.capabilities.contains(cap) {
            Ok(())
        } else {
            Err(CoreError::Capability(cap.into()))
        }
    }
}
fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Key(_) => "key",
        Op::Tap(_) => "tap",
        Op::Hold(_) => "hold",
        Op::Release(_) => "release",
        Op::Type(_) => "type",
        Op::Paste(_) => "paste",
        Op::Click(_) => "click",
        Op::Scroll { .. } => "scroll",
        Op::End(_) => "end",
        Op::See { .. } => "see",
        Op::Peek(_) => "peek",
        Op::Ocr => "ocr",
        Op::Wait(_) => "wait",
        Op::Focus(_) => "focus",
        Op::Workspace(_) => "ws",
        Op::SendWorkspace(_) => "send",
        Op::Close(_) => "close",
        Op::WindowAction(_) => "window",
        Op::Swap(_) => "swap",
        Op::Move(_) => "move",
        Op::Monitor(_) => "monitor",
        Op::List(_) => "list",
        Op::Spawn(_) => "spawn",
        Op::Kill(_) => "kill",
        Op::Pane { .. } => "pane",
        Op::Web { .. } => "web",
        Op::Label(_) => "label",
        Op::Jump { .. } => "jump",
        Op::Rep(_) => "rep",
        Op::While { .. } => "while",
        Op::Call(_) => "call",
        Op::Ret => "ret",
        Op::Def(_) => "def",
        Op::EndDef => "enddef",
        Op::Include(_) => "include",
        Op::Require(_) => "require",
        Op::Assert(_) => "assert",
        Op::Expect(_) => "expect",
        Op::Budget { .. } => "budget",
        Op::Checkpoint(_) => "checkpoint",
        Op::Rollback(_) => "rollback",
        Op::Note(_) => "note",
        Op::Nop => "nop",
        Op::Halt(_) => "halt",
        Op::BlockEnd => "}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn end_collision_is_unambiguous() {
        let program = parse_inline(&["super".into(), "enter".into(), "end".into()]).unwrap();
        assert_eq!(
            program.instructions[0].op,
            Op::Key(vec!["super".into(), "enter".into()])
        );
        assert_eq!(program.instructions[1].op, Op::End(ObserveMode::Auto));
        let key_end = parse_inline(&["key".into(), "end".into()]).unwrap();
        assert_eq!(key_end.instructions[0].op, Op::Key(vec!["end".into()]));
    }
    #[test]
    fn loops_are_bounded() {
        assert!(parse_script("while text ~\"wait\" {\n}\n").is_err());
        assert!(parse_script("while text ~\"wait\" max 2 {\n}\n").is_ok());
    }
    #[test]
    fn comments_and_quotes_work() {
        let p = parse_script("type \"hello; world\" ; ignored\nend text\n").unwrap();
        assert_eq!(p.instructions.len(), 2);
        assert_eq!(p.instructions[0].op, Op::Type("hello; world".into()));
    }
    #[test]
    fn selectors_are_uniform() {
        let selector = Selector::parse("class=chromium,title~\"Basecamp\"").unwrap();
        assert_eq!(selector.terms.len(), 2);
        assert_eq!(selector.terms[1].value, "Basecamp");
    }
}
