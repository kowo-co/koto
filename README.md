# koto

**A keyboard-first control library for Hyprland.**

koto is the argv/stdin/stdout interface for basm (beckett assembly): explicit
keyboard chords, strict window selectors, bounded control flow, and a final
observation rather than a stream of screenshots.

> This repository contains the v0.1 walking skeleton. The basm grammar,
> deterministic budgets/capabilities, CLI contracts, Hyprland state backend,
> observation-ladder interface, and C ABI validator are implemented. Direct
> virtual-keyboard injection, tmux/AT-SPI/CDP rungs, nested seats, checkpoints,
> and `kotod` are the next milestones; unsupported instructions fail explicitly
> and never silently no-op.

## Build

```sh
cargo build --release -p koto
# binary: target/release/koto
```

## Quick start

```sh
# Parsed as: key super enter; end
koto super enter end

# Inspect exactly what will run; no mutation occurs.
koto --explain super enter end
koto --dry-run --script task.basm

# Read a script from standard input.
printf 'key super enter\nend text\n' | koto -
```

A script is one instruction per line. Semicolons begin comments outside quoted
strings. The End-key collision is deliberate: bare `end` at an instruction
boundary emits and halts; `ctrl end` and `key end` press the End key.

```basm
require input,window
focus class=Alacritty,title~"^~/src"
key ctrl end
end text
```

## Invocation

```text
koto [GLOBALS] <instruction>...
koto [GLOBALS] --script FILE [--script FILE]...
koto [GLOBALS] -
koto [GLOBALS] --script FILE -- ARG1 ARG2
```

`--scripts a.basm,b.basm` is an alias for repeated `--script`. Files share a
register file and a single budget. `%1` through `%9` and `%*` bind script
arguments.

Implemented global flags include `--format`, `--observe`, `--timeout`,
`--budget-ops`, `--budget-time`, `--allow`, `--deny`, `--seat`, `--dry-run`,
`--explain`, `--trace`, `--session`, and `--profile`.

## Safety contract

- Selectors are addressable (`class=`, `title~`, `addr=`, `pid=`, `ws=`,
  `focused`, `last`), never coordinate-based by default.
- Empty and ambiguous selectors are distinct errors (exit 3 and 6).
- Capabilities are checked before a script's first instruction when declared
  with `require`; denial always wins over `--allow`.
- The default operation and wall-clock budgets are 256 and 120 seconds.
- `while` requires `max`; a program cannot contain an unbounded loop.
- Unsupported backends/instructions return an error and leave no impression of
  success.

## Workspace

| Crate | Purpose |
|---|---|
| `koto-core` | basm lexer/parser, ISA, VM, registers, budgets |
| `koto-hypr` | Hyprland JSON IPC, strict window resolution |
| `koto-observe` | ordered observation ladder contract |
| `koto-policy` | capabilities and deny-wins resolution |
| `koto` | CLI and output/exit-code contract |
| `libkoto` | C ABI (`include/koto.h`) |

Run the test suite with:

```sh
cargo test --workspace
```
