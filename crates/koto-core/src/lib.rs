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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registers {
    pub status: i32,
    pub n: u32,
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
    /// Executes a pane action. A read action returns exact scrollback text.
    fn pane(
        &mut self,
        _action: &str,
        _args: &[String],
        _timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        Err(CoreError::Unsupported("pane".into()))
    }
    fn spawn(&mut self, _command: &[String]) -> Result<String, CoreError> {
        Err(CoreError::Unsupported("spawn".into()))
    }
    fn kill(&mut self, _selector: &str) -> Result<(), CoreError> {
        Err(CoreError::Unsupported("kill".into()))
    }
    fn window(&mut self, _action: &str, _args: &[String]) -> Result<(), CoreError> {
        Err(CoreError::Unsupported("window operation".into()))
    }
    fn checkpoint(&mut self, _name: &str, _rollback: bool) -> Result<(), CoreError> {
        Err(CoreError::Unsupported("checkpoint".into()))
    }
    fn pointer(&mut self, _action: &str, _args: &[String]) -> Result<(), CoreError> {
        Err(CoreError::Backend("virtual pointer unavailable".into()))
    }
    fn key_state(&mut self, _keys: &[String], _pressed: bool) -> Result<(), CoreError> {
        Err(CoreError::Backend(
            "virtual keyboard hold unavailable".into(),
        ))
    }
    fn web(
        &mut self,
        _action: &str,
        _args: &[String],
        _timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        Err(CoreError::Backend("CDP target unavailable".into()))
    }
    fn metadata(&mut self, _field: &str) -> Result<String, CoreError> {
        Err(CoreError::Backend("window metadata unavailable".into()))
    }
}

pub struct Vm<'a, B: Backend> {
    pub backend: &'a mut B,
    pub capabilities: BTreeSet<String>,
    pub op_budget: u32,
    pub time_budget: Duration,
    pub default_timeout: Duration,
    pub registers: Registers,
}
impl<'a, B: Backend> Vm<'a, B> {
    pub fn run(&mut self, program: &Program) -> Result<Execution, CoreError> {
        let started = std::time::Instant::now();
        let mut execution = Execution {
            observation: None,
            registers: self.registers.clone(),
            trace: Vec::new(),
            halted: false,
        };
        // Require is preflight, even when it appears after a label or a macro.
        for instruction in &program.instructions {
            if let Op::Require(caps) = &instruction.op {
                for cap in caps {
                    self.require(cap)?;
                }
            }
        }
        let mut labels = std::collections::BTreeMap::new();
        let mut definitions = std::collections::BTreeMap::new();
        let mut definition_ends = std::collections::BTreeMap::new();
        let mut definition_stack = Vec::new();
        let mut block_ends = std::collections::BTreeMap::new();
        let mut block_stack = Vec::new();
        for (pc, instruction) in program.instructions.iter().enumerate() {
            match &instruction.op {
                Op::Label(name) => {
                    labels.insert(name.clone(), pc);
                }
                Op::Def(name) => {
                    definitions.insert(name.trim_end_matches("()").to_owned(), pc + 1);
                    definition_stack.push(pc);
                }
                Op::EndDef => {
                    let start = definition_stack
                        .pop()
                        .ok_or_else(|| CoreError::Parse("enddef without def".into()))?;
                    definition_ends.insert(start, pc);
                }
                Op::Rep(_) | Op::While { .. } => block_stack.push(pc),
                Op::BlockEnd => {
                    let start = block_stack
                        .pop()
                        .ok_or_else(|| CoreError::Parse("block end without loop".into()))?;
                    block_ends.insert(start, pc);
                }
                _ => {}
            }
        }
        let mut pc = 0usize;
        let mut loops: Vec<(usize, usize, u32, u32)> = Vec::new();
        let mut calls = Vec::new();
        let mut operations = 0u32;
        while pc < program.instructions.len() {
            if operations >= self.op_budget {
                return Err(CoreError::Budget("operations"));
            }
            if started.elapsed() > self.time_budget {
                return Err(CoreError::Budget("wall clock"));
            }
            let instruction = &program.instructions[pc];
            let tick = std::time::Instant::now();
            let mut next = pc + 1;
            let result = match &instruction.op {
                Op::Jump { kind, args } => {
                    let should_jump = match kind.as_str() {
                        "jmp" => true,
                        "jz" => execution.registers.status == 0,
                        "jnz" => execution.registers.status != 0,
                        "je" => {
                            args.get(0)
                                .map(|register| self.register_value(register, &execution.registers))
                                == Some(args.get(1).cloned().unwrap_or_default())
                        }
                        _ => false,
                    };
                    let label = if kind == "je" {
                        args.get(2)
                    } else {
                        args.first()
                    }
                    .ok_or_else(|| CoreError::Parse(format!("{kind} needs a label")));
                    match label {
                        Ok(label) if should_jump => {
                            next = labels.get(label).copied().ok_or_else(|| {
                                CoreError::Parse(format!("unknown label `{label}`"))
                            })?;
                            Ok(())
                        }
                        Ok(_) => Ok(()),
                        Err(error) => Err(error),
                    }
                }
                Op::Label(_) => Ok(()),
                Op::Rep(count) => {
                    let end = block_ends
                        .get(&pc)
                        .copied()
                        .ok_or_else(|| CoreError::Parse("rep has no block".into()))?;
                    if *count == 0 {
                        next = end + 1;
                    } else {
                        execution.registers.n = 1;
                        loops.push((pc, end, *count, 1));
                    }
                    Ok(())
                }
                Op::While { predicate, max } => {
                    let end = block_ends
                        .get(&pc)
                        .copied()
                        .ok_or_else(|| CoreError::Parse("while has no block".into()))?;
                    if !self.predicate(predicate, &execution.registers) {
                        next = end + 1;
                    } else {
                        execution.registers.n = 1;
                        loops.push((pc, end, *max, 1));
                    }
                    Ok(())
                }
                Op::BlockEnd => {
                    let (start, end, max, current) = loops
                        .last()
                        .copied()
                        .ok_or_else(|| CoreError::Parse("block end outside loop".into()))?;
                    if end != pc {
                        Err(CoreError::Parse("mismatched loop block".into()))
                    } else if current >= max {
                        loops.pop();
                        next = pc + 1;
                        Ok(())
                    } else if matches!(&program.instructions[start].op, Op::While { predicate, .. } if !self.predicate(predicate, &execution.registers))
                    {
                        loops.pop();
                        next = pc + 1;
                        Ok(())
                    } else {
                        let next_count = current + 1;
                        loops.last_mut().unwrap().3 = next_count;
                        execution.registers.n = next_count;
                        next = start + 1;
                        Ok(())
                    }
                }
                Op::EndDef => {
                    next = calls
                        .pop()
                        .ok_or_else(|| CoreError::Parse("enddef outside call".into()))?;
                    Ok(())
                }
                Op::Def(_) => {
                    next = definition_ends
                        .get(&pc)
                        .copied()
                        .ok_or_else(|| CoreError::Parse("unclosed def".into()))?
                        + 1;
                    Ok(())
                }
                Op::Call(label) => {
                    calls.push(pc + 1);
                    next = labels
                        .get(label)
                        .copied()
                        .or_else(|| definitions.get(label).copied())
                        .ok_or_else(|| {
                            CoreError::Parse(format!("unknown label or definition `{label}`"))
                        })?;
                    Ok(())
                }
                Op::Ret => {
                    next = calls
                        .pop()
                        .ok_or_else(|| CoreError::Parse("ret without call".into()))?;
                    Ok(())
                }
                _ => self.execute(&instruction.op, &mut execution),
            };
            let status = match &instruction.op {
                Op::Expect(predicate) if result.is_ok() => {
                    i32::from(!self.predicate(predicate, &execution.registers))
                }
                Op::Halt(code) if result.is_ok() => *code,
                _ if result.is_ok() => 0,
                _ => 1,
            };
            execution.registers.status = status;
            execution.trace.push(TraceEntry {
                i: instruction.index,
                op: op_name(&instruction.op).into(),
                ms: tick.elapsed().as_millis(),
                status,
                warning:
                    matches!(&instruction.op, Op::Wait(Wait { kind, .. }) if kind == "duration")
                        .then(|| "raw sleep is discouraged".into())
                        .or_else(|| {
                            matches!(&instruction.op, Op::Click(_) | Op::Scroll { .. })
                                .then(|| "pointer instruction used".into())
                        }),
            });
            operations += 1;
            result?;
            if execution.halted {
                break;
            }
            pc = next;
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
            Op::Hold(keys) => {
                self.require("input")?;
                self.backend.key_state(keys, true)
            }
            Op::Release(keys) => {
                self.require("input")?;
                self.backend.key_state(keys, false)
            }
            Op::Click(selector) => {
                self.require("pointer")?;
                self.backend
                    .pointer("click", std::slice::from_ref(selector))
            }
            Op::Scroll { direction, count } => {
                self.require("pointer")?;
                self.backend
                    .pointer("scroll", &[direction.clone(), count.to_string()])
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
            Op::Peek(field) => {
                execution.registers.out = self.backend.metadata(field)?;
                Ok(())
            }
            Op::Ocr => Err(CoreError::Backend("OCR backend unavailable".into())),
            Op::Web { action, args } => {
                if action == "eval" {
                    self.require("web.eval")?;
                } else {
                    self.require("web")?;
                }
                if let Some(output) = self.backend.web(action, args, self.default_timeout)? {
                    execution.registers.out = output;
                }
                Ok(())
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
            Op::Pane { action, args } => {
                if action == "run" {
                    self.require("exec")?;
                }
                if let Some(output) = self.backend.pane(action, args, self.default_timeout)? {
                    execution.registers.out = output;
                }
                Ok(())
            }
            Op::Spawn(command) => {
                self.require("spawn")?;
                execution.registers.out = self.backend.spawn(command)?;
                Ok(())
            }
            Op::Kill(selector) => {
                self.require("spawn")?;
                self.backend.kill(selector)
            }
            Op::Workspace(workspace) => {
                self.require("window")?;
                self.backend.window("ws", std::slice::from_ref(workspace))
            }
            Op::SendWorkspace(workspace) => {
                self.require("window")?;
                self.backend.window("send", std::slice::from_ref(workspace))
            }
            Op::Close(selector) => {
                self.require("window")?;
                self.backend.window(
                    "close",
                    selector.as_ref().map(std::slice::from_ref).unwrap_or(&[]),
                )
            }
            Op::WindowAction(action) => {
                self.require("window")?;
                self.backend.window(action, &[])
            }
            Op::Swap(direction) => {
                self.require("window")?;
                self.backend.window("swap", std::slice::from_ref(direction))
            }
            Op::Move(direction) => {
                self.require("window")?;
                self.backend.window("move", std::slice::from_ref(direction))
            }
            Op::Monitor(name) => {
                self.require("window")?;
                self.backend.window("monitor", std::slice::from_ref(name))
            }
            Op::Checkpoint(name) => {
                self.require("fs")?;
                self.backend.checkpoint(name, false)
            }
            Op::Rollback(name) => {
                self.require("fs")?;
                self.backend.checkpoint(name, true)
            }
            Op::Assert(predicate) => {
                if self.predicate(predicate, &execution.registers) {
                    Ok(())
                } else {
                    Err(CoreError::Assertion(predicate.clone()))
                }
            }
            // `expect` is deliberately non-fatal. The run loop preserves its 1 in `$?`.
            Op::Expect(_) => Ok(()),
            Op::Note(_) => Ok(()),
            Op::Require(caps) => {
                for cap in caps {
                    self.require(cap)?;
                }
                Ok(())
            }
            Op::Budget { kind, value } => match kind.as_str() {
                "ops" => {
                    let value = value
                        .parse::<u32>()
                        .map_err(|_| CoreError::Parse("budget ops needs an integer".into()))?;
                    self.op_budget = self.op_budget.min(value);
                    Ok(())
                }
                "time" => {
                    let value = parse_duration(value)?;
                    self.time_budget = self.time_budget.min(value);
                    Ok(())
                }
                _ => Err(CoreError::Parse(format!("unknown budget {kind}"))),
            },
            Op::Nop | Op::Label(_) | Op::BlockEnd | Op::EndDef => Ok(()),
            Op::Halt(code) => {
                execution.registers.status = *code;
                execution.halted = true;
                Ok(())
            }
            _ => Err(CoreError::Unsupported(op_name(op).into())),
        }
    }
    fn predicate(&self, predicate: &str, registers: &Registers) -> bool {
        let predicate = predicate.trim();
        if let Some(pattern) = predicate.strip_prefix("text ~") {
            return regex::Regex::new(pattern.trim_matches('"'))
                .is_ok_and(|regex| regex.is_match(&registers.out));
        }
        if let Some(text) = predicate.strip_prefix("text contains ") {
            return registers.out.contains(text.trim_matches('"'));
        }
        if let Some(pattern) = predicate.strip_prefix("title ~") {
            return regex::Regex::new(pattern.trim_matches('"'))
                .is_ok_and(|regex| regex.is_match(&registers.out));
        }
        for operator in ["==", "!=", "<=", ">=", "<", ">"] {
            if let Some((left, right)) = predicate.split_once(operator) {
                let left = self.register_value(left.trim(), registers);
                let right = right.trim().trim_matches('"');
                return match operator {
                    "==" => left == right,
                    "!=" => left != right,
                    "<" => {
                        left.parse::<f64>().unwrap_or(f64::NAN) < right.parse().unwrap_or(f64::NAN)
                    }
                    "<=" => {
                        left.parse::<f64>().unwrap_or(f64::NAN) <= right.parse().unwrap_or(f64::NAN)
                    }
                    ">" => {
                        left.parse::<f64>().unwrap_or(f64::NAN) > right.parse().unwrap_or(f64::NAN)
                    }
                    ">=" => {
                        left.parse::<f64>().unwrap_or(f64::NAN) >= right.parse().unwrap_or(f64::NAN)
                    }
                    _ => false,
                };
            }
        }
        false
    }
    fn register_value(&self, register: &str, registers: &Registers) -> String {
        match register {
            "$?" => registers.status.to_string(),
            "$out" => registers.out.clone(),
            value if value.starts_with("$env.") => std::env::var(&value[5..]).unwrap_or_default(),
            value => registers.values.get(value).cloned().unwrap_or_default(),
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
fn parse_duration(value: &str) -> Result<Duration, CoreError> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else {
        return Err(CoreError::Parse(format!("invalid duration `{value}`")));
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
        .map(Duration::from_millis)
        .ok_or_else(|| CoreError::Parse(format!("invalid duration `{value}`")))
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
    struct Null;
    impl Backend for Null {
        fn key(&mut self, _: &[String]) -> Result<(), CoreError> {
            Ok(())
        }
        fn text(&mut self, _: &str, _: bool) -> Result<(), CoreError> {
            Ok(())
        }
        fn wait(&mut self, _: &Wait, _: Duration) -> Result<(), CoreError> {
            Ok(())
        }
        fn focus(&mut self, _: &str) -> Result<(), CoreError> {
            Ok(())
        }
        fn observe(&mut self, _: ObserveMode) -> Result<Observation, CoreError> {
            Ok(Observation {
                source: "test".into(),
                fidelity: "exact".into(),
                text: None,
                image: None,
            })
        }
        fn list(&mut self, _: &str) -> Result<String, CoreError> {
            Ok(String::new())
        }
    }
    fn vm(backend: &mut Null) -> Vm<'_, Null> {
        Vm {
            backend,
            capabilities: BTreeSet::new(),
            op_budget: 32,
            time_budget: Duration::from_secs(1),
            default_timeout: Duration::from_secs(1),
            registers: Registers::default(),
        }
    }
    #[test]
    fn repetitions_execute_a_bounded_body() {
        let program = parse_script("rep 3 {\nnop\n}\nend silent\n").unwrap();
        let execution = vm(&mut Null).run(&program).unwrap();
        assert!(execution.halted);
        assert_eq!(
            execution
                .trace
                .iter()
                .filter(|entry| entry.op == "nop")
                .count(),
            3
        );
    }
    #[test]
    fn definitions_are_skipped_until_called() {
        let program = parse_script("def f()\nnop\nenddef\ncall f\nend silent\n").unwrap();
        let execution = vm(&mut Null).run(&program).unwrap();
        assert!(execution.halted);
        assert_eq!(
            execution
                .trace
                .iter()
                .filter(|entry| entry.op == "nop")
                .count(),
            1
        );
    }
}
