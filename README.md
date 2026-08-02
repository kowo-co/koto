# koto

**Keyboard-first computer control for Hyprland.** Your agent gets a keyboard, a
window manager, and an opinion. Not a mouse and a prayer.

```sh
koto super enter end
```

That launched a terminal, waited for it, and told you what's on screen. One
process. One observation. Done.

---

## The pitch

Most computer-use stacks work like this: screenshot, squint at it, guess some
coordinates, click, screenshot again, squint again, notice nothing happened,
click harder. Forty screenshots and eleven dollars later the agent has opened a
file manager. Maybe.

koto thinks that's a bad plan.

**Screenshots are the last resort, not the interface.** koto walks an
observation ladder — CDP, then the accessibility tree, then tmux's exact
scrollback, then compositor metadata, then and only then pixels, with OCR at the
bottom. The web rung speaks either raw CDP or BetterWright, whichever you
attached. When your agent asks what's on screen, it gets this:

```
window co.kowo.KotoEvalSettings ws=4 title="Koto Eval Settings"
source atspi fidelity=structured
---
window "Koto Eval Settings"
  tab list
    tab "General" [selected]
    tab "Accessibility"
  switch "Enable Telemetry" [checked,focused]
  switch "Start Minimised"
```

Roles. Names. States. Hierarchy. Exact, cheap, unambiguous — a switch is a
switch, and you can *see* it's already on. Compare and contrast with a JPEG of a
toggle that might be blue.

**Coordinates are a code smell.** Windows are addressed by class, title, PID,
address, workspace, accessible role, or accessible name. A script written on a
1440p ultrawide runs unchanged on a laptop, because it never knew about pixels in
the first place.

```basm
focus class=chromium,title~"Basecamp"
```

Zero matches is exit 3. Two matches is exit 6. Ambiguity is an error, never a
coin flip.

**One `end` per program, not forty screenshots.** `end` is the only expensive
instruction; everything else is silent. An agent that runs forty instructions and
one `end` pays for one observation. That is the entire reason a script layer
exists — it buys back the round trips that make computer use cost more than the
work being done.

**Bounded by construction.** Every loop needs `max`. Every script has an
operation budget and a wall-clock budget, both on by default. There is no flag to
disable them, because there is no version of "let the agent loop forever on my
desktop" that ends well.

**Failure is loud.** Ten exit codes, each meaning exactly one thing, and nothing
silently no-ops. Typo a key name and you get exit 8 — parse error, fix your
syntax — not exit 9 pretending the machine is broken. That distinction is the
difference between an agent that recovers and an agent that gives up.

| `0` ok | `1` assert | `2` timeout | `3` no match | `4` budget |
|---|---|---|---|---|
| **`5`** denied | **`6`** ambiguous | **`7`** no observation | **`8`** parse | **`9`** backend |

---

## basm

The instruction set is called basm. It is a real language with a real grammar,
not a bag of prompt suggestions.

```basm
require input,window,spawn,exec
spawn alacritty --title build
wait window title~"build" timeout 10s
focus title~"build"
type "cargo test --workspace"
key return
pane wait "test result"
pane read 40
end text
```

Families: input, observe, wait, window, process, terminal, web, flow, guards,
meta. Full table in [`docs/isa.md`](docs/isa.md); full spec in
[`spec.md`](spec.md).

Two lexical rules that will bite you exactly once:

- Inline mode is an argv token stream, so a bare token implies `key`.
  `koto super enter` is one chord; `koto enter enter` is two presses.
- `end` at an instruction boundary halts and observes. `ctrl end` and `key end`
  are the End key. Yes, this is deliberate.

## Capabilities

`input`, `window`, `spawn`, `exec`, `web`, `web.eval`, `web.login`,
`web.download`, `pointer`, `fs` — configured in
`~/.config/koto/policy.toml`, overridable per invocation.
**Deny always wins.** `require` at the top of a script validates before
instruction zero, so a policy violation fails immediately instead of halfway
through a mutation.

The gate is on anything that *reaches* a shell, not just the obvious one:
`pane new`, `pane send`, `pane run`, and `pane kill` all need `exec`, because
`pane send "rm -rf /\n"` is every bit as much execution as `pane run` is.
`pane read` will not start a shell to satisfy an observation.

Pointer use is legal, gated, and always warning-bearing in the trace.
Coordinates never appear in a well-written basm file. We're not your dad. We are
judging you.

---

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/kowo-co/koto/main/install.sh | sh
```

Builds from source (needs cargo), installs `koto` to `~/.local/bin`, and links
the agent skill into `~/.claude/skills/koto` and `~/.agent/skills/koto` — one
canonical copy under `~/.local/share/koto`, symlinked everywhere agents look.
Read it first if piping curl into sh makes you itch; it should, and it's short.

## Build

```sh
cargo build --release -p koto   # target/release/koto
```

```sh
koto --explain super enter end        # parse it, run nothing
koto --dry-run --script task.basm     # validate, mutate nothing
printf 'key super enter\nend text\n' | koto -
```

## Testing

```sh
cargo test --workspace     # parser, VM, registers, budgets
tests/exit-contract.sh     # every documented exit code
tests/integration.sh       # the four backends, against a live compositor
tests/seat.sh              # persistent nested seats
```

That last one earns its keep. A unit test cannot tell you the virtual keyboard's
keymap descriptor was opened write-only, so the compositor could never map it,
so every keystroke koto ever sent went nowhere — while koto cheerfully reported
success. Ask us how we know. The integration suite types a string and asserts the
bytes arrived, opens eight tmux panes and reads them back, moves a window and
checks the compositor agrees, and captures a screenshot and validates the PNG.

Effects, not return values. Return values lie.

## Workspace

| Crate | Purpose |
|---|---|
| `koto-core` | basm lexer, parser, ISA, VM, registers, budgets |
| `koto-hypr` | Hyprland IPC, strict window resolution |
| `koto-input` | Wayland virtual keyboard and pointer |
| `koto-observe` | the observation ladder |
| `koto-policy` | capabilities, deny-wins resolution |
| `koto-web` | Chromium over `--remote-debugging-pipe`; BetterWright over a node sidecar |
| `koto` | CLI, output formats, exit codes |
| `libkoto` | C ABI — see [`include/koto.h`](include/koto.h) |

## Safety

`SUPER+CTRL+SHIFT+ESC` aborts every running koto program, releases held
modifiers, and revokes leases. Installed at setup; not disableable from a script.
Modifiers are also released on abnormal exit, because a stuck `hold super` makes
a machine unusable and we'd like you to keep using yours.

---

## Status

v0.1. We'd rather tell you than have you find out.

**Works:** basm end to end, Wayland virtual keyboard and pointer, Hyprland state
and events, tmux control-mode panes, Chromium over CDP, the BetterWright engine
(pruned accessibility snapshots and page screenshots for sites AT-SPI cannot
see), the AT-SPI rung, wlroots screencopy, OCR via Tesseract, capabilities,
budgets, btrfs checkpoints, and the C ABI.

**Doesn't yet:** `kotod`, leases, network take-over.

**Nested seats** work and persist: `--seat nested` starts a second Hyprland the
first time and attaches to it thereafter, so windows survive between
invocations. Your desktop never sees them and they never see your keystrokes —
which is also what makes `wait idle` meaningful again, since a private seat
actually goes quiet. `--seat-status` and `--seat-stop` manage it.

```sh
koto --seat nested --allow window,spawn spawn alacritty end silent
koto --seat nested --allow window list windows end text   # still there
koto --seat-stop
```

Hyprland only, for now. The backends sit behind four traits, so another
compositor is an implementation, not a rewrite.

---

*Named after Beckett. Sparse, deliberate, and refuses to waste your time.*
