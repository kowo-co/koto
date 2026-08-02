# koto

**A keyboard-first control and take-over library for Hyprland.**

Version 0.1 (draft specification)
Part of kowo-co. Ships as `koto` (CLI), `libkoto` (C ABI), and later `kotod` (session daemon).

---

## I. What koto is

koto gives an agent the same interface a competent keyboard user has to Omarchy: chords, window selectors, workspace moves, and a way to read back what happened. The instruction language is called **basm** (beckett assembly). It is not real assembly, but it borrows the shape: short mnemonics, explicit operands, registers, labels, branches, and no hidden control flow.

The headline case is a single line:

```sh
koto super enter end
```

That presses SUPER+Return, waits for the resulting window to settle, reads its contents, and prints them. The agent gets text if the surface can be scraped cleanly and an image reference if not.

Two halves to the product:

- **Control.** koto drives the box it runs on. This is the load-bearing half and everything in §2 through §9 is about it.
- **Take-over.** koto lends a live human session to an agent under a scoped, revocable lease. Sketched in §11, deliberately out of scope for v1.

There is no MCP server. The interface is argv, stdin, stdout, and exit codes.

---

## II. Design axioms

These are the constraints every later decision answers to.

**1. Keyboard first, pointer last.** Omarchy is operable without a mouse. koto is too. Pointer instructions exist, they are gated behind an explicit capability, and any script that uses them gets a warning in its trace. Coordinates never appear in a well-written basm file.

**2. Structured before pixels.** Every observation walks a ladder (§6) from CDP down to raw framebuffer. Screenshots are the rung you land on when the four above it fail, not the default.

**3. `end` is the only expensive instruction.** Everything else is silent. An agent that runs forty instructions and one `end` pays for one observation. This is the whole reason for a script layer: it lets the agent spend one round trip on work that would otherwise be forty.

**4. Addressable, never spatial.** Instructions reference windows by class, title, address, PID, or accessible role. A basm script written on a 1440p ultrawide runs unchanged on a laptop.

**5. Deterministic failure.** No instruction silently no-ops. Every failure produces a non-zero exit code, a machine-readable reason, and a trace entry naming the instruction index.

**6. Bounded by construction.** Every loop needs a bound. Every script has an operation budget and a wall-clock budget with defaults. An agent cannot write a basm file that runs forever.

---

## III. Invocation

```
koto [GLOBALS] <instruction>...                  # inline mode
koto [GLOBALS] --script FILE [--script FILE]...  # script mode
koto [GLOBALS] -                                 # basm on stdin
koto [GLOBALS] --script FILE -- ARG1 ARG2        # args bind to %1, %2
```

Multiple `--script` flags concatenate in order, sharing one register file and one budget. `--scripts a.basm,b.basm` is accepted as a comma-separated alias.

### 3.1 Global flags

| Flag | Default | Effect |
|---|---|---|
| `--format <agent\|json\|raw\|quiet>` | `agent` | Output contract, §7 |
| `--observe <auto\|text\|image\|both>` | `auto` | Ladder policy, §6 |
| `--timeout <dur>` | `10s` | Default per-instruction timeout |
| `--budget-ops <n>` | `256` | Hard instruction cap |
| `--budget-time <dur>` | `120s` | Hard wall-clock cap |
| `--allow <cap>[,<cap>]` | from policy | Grant capabilities, §9 |
| `--deny <cap>[,<cap>]` | none | Revoke, wins over allow |
| `--seat <host\|nested\|auto>` | `auto` | Session isolation, §9.3 |
| `--dry-run` | off | Resolve every selector, execute nothing, print the plan |
| `--explain` | off | Print the parsed instruction stream and exit |
| `--trace <path>` | none | Append a JSONL trace |
| `--session <name>` | `default` | Persist registers and seat between invocations |
| `--profile <name>` | `default` | Config file selection |

`--dry-run` matters more than it looks. It is how an agent checks that `focus class=chromium,title~"Basecamp"` resolves to exactly one window before committing to a chord that would otherwise land in the wrong place.

---

## IV. The basm instruction set

### 4.1 Lexical structure

An instruction is a mnemonic followed by operands. In script mode, one instruction per line, `;` begins a comment. In inline mode, argv is a token stream and the parser splits it by mnemonic arity.

Bare key tokens with no preceding mnemonic imply `key`. This is what makes `koto super enter end` work: `super enter` parses as `key super enter`, and `end` parses as the `end` mnemonic because it appears at an instruction boundary.

**Modifier binding rule.** Modifiers (`super`, `ctrl`, `alt`, `shift`) are prefix operators binding rightward to the first non-modifier key. `super enter` is one chord. `enter enter` is two presses. To press a modifier alone, use `tap super`.

**The `end` collision.** `end` is both a mnemonic and the name of the End key. Resolution: inside a chord, meaning after at least one modifier or after an explicit `key` mnemonic, `end` is the End key. At an instruction boundary with no chord pending, `end` is the mnemonic. `ctrl end` presses CTRL+End. `key end` presses End. A bare `end` terminates and observes. This is the one place the grammar is context-sensitive and it is documented here because it will bite someone.

### 4.2 Registers

| Register | Contents |
|---|---|
| `$0`–`$9` | General purpose, string-typed |
| `$?` | Status of the last instruction, 0 on success |
| `$w` | Focused window record (JSON object, dotted access: `$w.class`) |
| `$out` | Last observation text |
| `$n` | Loop counter, valid inside `rep` and `while` |
| `%1`–`%9`, `%*` | Script arguments |
| `$env.NAME` | Environment passthrough |

Registers persist across `--script` files in one invocation, and across invocations sharing a `--session`.

### 4.3 Input

```
key    <chord>              ; default mnemonic, e.g. `key super shift return`
tap    <key>                ; single key, no modifier binding
hold   <mod>...             ; press and hold until `release`
release <mod>...
type   "<text>"             ; literal text via virtual keyboard, keymap-aware
paste  "<text>"             ; via wl-clipboard + paste chord, for long or unicode text
click  <selector>           ; requires cap: pointer
scroll <up|down|left|right> [n]
```

`type` synthesises keysyms and handles layout. `paste` is faster and safer above roughly 200 characters or for anything outside the active keymap. A script that types a 4KB blob character by character is doing it wrong.

### 4.4 Observation

```
end    [text|image|json|both|silent]   ; emit observation, halt
see    [$reg] [text|image]             ; capture into register, continue
peek   <field>                         ; read one metadata field, no capture
ocr                                    ; force OCR of the last captured image
```

`end` halts the program. `see` is its non-terminating sibling and is what you use inside loops. `peek title` costs nothing and is how you test state without paying for a scrape.

`end silent` halts with an exit code and no output, for scripts whose result is the exit status.

### 4.5 Waiting

Every wait accepts `timeout <dur>`. Without it, `--timeout` applies.

```
wait <dur>                  ; raw sleep, discouraged, warns in trace
wait window <selector>
wait gone <selector>
wait title <pattern>
wait text <pattern>         ; polls the observation ladder
wait idle [<dur>]           ; no compositor events for the duration
wait exit <pid>
```

`wait idle 200ms` is the settle primitive and should follow most window-opening chords. Hyprland's event socket makes this exact rather than a guess, which is the single largest reliability win over sleep-based automation.

### 4.6 Windows and workspaces

```
focus  <selector>
ws     <n|+1|-1|prev>
send   ws <n>               ; move focused window to workspace
close  [selector]
float | tile | full | pin
swap   <l|r|u|d>
move   <l|r|u|d>
monitor <name>
list   <windows|workspaces|monitors|devices|clients>
```

All of these map to `hyprctl dispatch` or `hyprctl -j` reads. `list` writes JSON to `$out` and is the cheapest way for an agent to build a world model.

### 4.7 Processes

```
spawn <cmd>...              ; requires cap: spawn
kill  <selector|scope>
```

`spawn` launches through `uwsm app --` where available, so the process lands in its own systemd scope. `$out` receives the scope name and PID. This gives you clean teardown: `kill scope=app-koto-chromium-3f2.scope` removes everything the agent started without touching what the human started.

### 4.8 Terminal

Backed by tmux control mode. koto maintains a dedicated tmux server socket so agent panes never collide with human ones.

```
pane new  [name]
pane send "<text>"          ; keystrokes into the pane, no shell interpretation
pane run  "<cmd>"           ; send + Enter, convenience
pane read [n]               ; last n lines, default whole scrollback of the visible pane
pane wait <pattern>
pane kill [name]
```

This is the highest-value layer on Omarchy. Neovim, lazygit, btop, and every coding CLI live here, and `pane read` returns exact text with no vision tokens and no OCR error.

### 4.9 Web

Speaks CDP over `--remote-debugging-pipe`, either to a betterwright-managed browser or to an attached session (§11).

```
web attach <target>         ; requires cap: web
web goto  <url>
web click <css|text=>
web fill  <css> "<text>"
web read  [css]             ; accessibility tree by default, not innerText
web wait  <css|url~>
web eval  "<js>"            ; requires cap: web.eval
```

`web read` returns the CDP accessibility snapshot, which is structurally closer to what a screen reader sees than to raw DOM, and is much cheaper in tokens.

### 4.10 Control flow

```
.label:
jmp  .label
jz   .label                 ; jump if $? == 0
jnz  .label
je   <reg> <value> .label
rep  <n> { ... }            ; bounded loop, $n is the counter
while <pred> max <n> { ... }; `max` is mandatory
call .label
ret
def  <name>(<params>) ... enddef
include "<file>"
```

`while` without `max` is a parse error, not a warning. There is no unbounded loop in basm and there is no flag to enable one.

### 4.11 Guards

```
require <cap>[,<cap>]       ; declare needs up front; validated before instruction 0
assert  <pred>              ; abort non-zero on failure
expect  <pred>              ; set $?, continue
budget  ops <n> | time <dur>
checkpoint <name>           ; btrfs snapshot, requires cap: fs
rollback   <name>
```

`require` at the top of a script means a policy violation fails immediately rather than halfway through a mutation. Every shipped script should start with one.

### 4.12 Meta

```
note "<text>"               ; annotate the trace
nop
halt [code]
```

---

## V. Selectors and predicates

### 5.1 Selector grammar

One uniform grammar across `focus`, `wait`, `close`, `click`, and `kill`.

```
class=chromium              ; exact match on window class
title~"^nvim"               ; regex on title
addr=0x55f1a2               ; Hyprland window address, stable for the window's life
pid=4821
scope=app-foo.scope
ws=3
role=button                 ; AT-SPI role
name="Save"                 ; AT-SPI accessible name
web=#login                  ; CDP selector, valid only after `web attach`
focused
last                        ; most recently opened window
```

Comma is AND: `focus class=chromium,title~"Basecamp"`.

Resolution is strict. A selector matching zero windows exits 3. A selector matching more than one exits 6 unless the instruction is one that accepts a set (`close`, `kill`, `list`). Ambiguity is an error, never a coin flip.

`at=<x>,<y>` exists, requires `--allow pointer`, and is the only spatial selector. Its presence in a script is a code smell and koto says so in the trace.

### 5.2 Predicates

```
text ~"regex" | text contains "literal"
window exists <selector> | window count <op> <n>
$? <op> <n> | $<reg> <op> "<value>"
title ~"regex"
```

Operators: `==` `!=` `<` `<=` `>` `>=`.

---

## VI. The observation ladder

`end` and `see` walk this ladder top down and stop at the first rung that yields content. The chosen rung is reported in the output header so the agent can calibrate how much to trust what it just read.

| Rung | Source | Mechanism | Fidelity |
|---|---|---|---|
| 1 | `cdp` | Chrome DevTools accessibility snapshot | exact, structured |
| 2 | `tmux` | `capture-pane -p` | exact text |
| 3 | `atspi` | AT-SPI2 over D-Bus | structured, coverage varies |
| 4 | `hypr` | Window metadata only (class, title, geometry) | metadata only |
| 5 | `ocr` | screencopy + OCR | lossy |
| 6 | `pixels` | screencopy PNG | image, agent interprets |

Rung 4 exists because a window's title is often the whole answer. If the agent asked "did the terminal open," the class and title settle it and no capture is needed.

`--observe text` restricts to rungs 1 through 5 and exits 7 if none produce content. `--observe image` skips to 6. `--observe both` returns text plus image for rungs 1 through 3, which is what you want while debugging a script.

Images are written to `$XDG_RUNTIME_DIR/koto/obs/<unix_ms>.png` and the path is printed. Base64 inlining happens only under `--format json --inline-images`, because blasting a megabyte of base64 into a terminal is a hostile default.

Capture on Wayland uses the `wlr-screencopy` protocol via a `grim`-equivalent in-process, not by shelling out. Per-window capture uses the Hyprland window geometry from rung 4.

---

## VII. Output contract

### 7.1 `--format agent` (default)

```
#koto ok ops=4 t=812ms seat=host
window Alacritty ws=1 addr=0x55f1a2 title="~/src/beckett"
source tmux fidelity=exact
---
jason@omarchy ~/src/beckett $ cargo test
   Compiling koto-core v0.1.0
test result: ok. 42 passed; 0 failed
```

Header line, one metadata line per relevant subject, a `source` line, a `---` separator, then payload. Designed to be readable by a model without a parser and by a human without squinting.

### 7.2 `--format json`

```json
{
  "status": "ok",
  "exit": 0,
  "ops": 4,
  "elapsed_ms": 812,
  "seat": "host",
  "observation": {
    "source": "tmux",
    "fidelity": "exact",
    "text": "...",
    "image": null
  },
  "window": { "class": "Alacritty", "addr": "0x55f1a2", "ws": 1, "title": "..." },
  "trace": [ { "i": 0, "op": "key", "args": ["super","enter"], "ms": 12, "status": 0 } ]
}
```

### 7.3 Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Assertion failed |
| 2 | Timeout |
| 3 | Selector matched nothing |
| 4 | Budget exceeded (ops or time) |
| 5 | Capability denied by policy |
| 6 | Selector ambiguous |
| 7 | Observation unavailable at the requested fidelity |
| 8 | Parse error in basm |
| 9 | Backend unavailable (no compositor, no tmux server, no CDP target) |
| 64 | Internal error |

---

## VIII. Architecture

Rust, with a C ABI surface. Rust gives you the Wayland protocol bindings, `serde` for the IPC layer, and a single static binary with no runtime. The C ABI (`libkoto.so` + `koto.h`) covers anyone who wants to link it from C, and covers a future Hyprland plugin, which must be C or C++.

### 8.1 Crates

```
koto-core      ISA definition, lexer, parser, VM, register file, budget enforcement
koto-hypr      Hyprland IPC: .socket.sock (commands), .socket2.sock (events)
koto-input     wlr-virtual-pointer-unstable-v1, virtual-keyboard-unstable-v1, keymap synthesis
koto-observe   Ladder implementation: cdp, tmux, atspi, hypr, ocr, screencopy
koto-policy    Capability model, policy.toml, lease management
koto-cli       argv parsing, output formatting, exit codes
libkoto        C ABI wrapper over koto-core + backends
kotod          Session daemon and take-over transport (phase 3)
```

### 8.2 Backend traits

Platform portability lives entirely behind four traits. Adding Ubuntu means implementing them for a different compositor, not touching the VM.

```rust
trait Compositor {
    fn windows(&self) -> Result<Vec<Window>>;
    fn focus(&self, sel: &Selector) -> Result<Window>;
    fn dispatch(&self, cmd: Dispatch) -> Result<()>;
    fn events(&self) -> Result<EventStream>;   // the settle primitive depends on this
}

trait Input {
    fn chord(&self, keys: &[Key]) -> Result<()>;
    fn text(&self, s: &str) -> Result<()>;
    fn pointer(&self, act: PointerAction) -> Result<()>;
}

trait Observer {
    fn rung(&self) -> Rung;
    fn observe(&self, target: &Target) -> Result<Option<Observation>>;
}

trait Isolation {
    fn nested_seat(&self) -> Result<Seat>;
    fn checkpoint(&self, name: &str) -> Result<()>;
    fn rollback(&self, name: &str) -> Result<()>;
}
```

### 8.3 Input backend choice

Bind `wlr-virtual-pointer-unstable-v1` and `virtual-keyboard-unstable-v1` directly through `wayland-client`.

Not the RemoteDesktop portal: Hyprland's portal backend still has competing RemoteDesktop PRs open, and the third-party bridges that do work are themselves translating libei events onto these same two protocols. Going through the portal buys you a dependency on unmerged code to reach an interface you can call directly.

Not `ydotool`: uinput needs root or group surgery, injects below the compositor so you lose per-window targeting, and requires a daemon. The virtual protocols need none of that.

libei becomes the right answer for macOS and Windows parity later, and the `Input` trait is where it plugs in.

### 8.4 Platform matrix

| Capability | Arch/Hyprland | Ubuntu/GNOME | macOS | Windows |
|---|---|---|---|---|
| Window state | hyprctl IPC | GNOME Shell D-Bus eval (restricted) | Accessibility API | UIAutomation |
| Event stream | socket2, exact | limited | AXObserver | UIA events |
| Input | virtual-* protocols | libei via portal | CGEvent | SendInput |
| Terminal | tmux | tmux | tmux | ConPTY |
| Web | CDP | CDP | CDP | CDP |
| A11y tree | AT-SPI2 | AT-SPI2 | AX API | UIA |
| Capture | wlr-screencopy | portal ScreenCast | CGWindowList | DXGI |
| Nested seat | yes, nested Hyprland | no | no | no |
| Checkpoint | btrfs | btrfs if present | APFS snapshot | VSS |

Ship order: Arch/Hyprland, then Debian family, then macOS if there is demand, then Windows. The event stream row is why Hyprland is first and why the others will feel worse: nothing else gives you an exact settle signal for free.

---

## IX. Safety and isolation

### 9.1 Capabilities

| Capability | Gates |
|---|---|
| `input` | key, type, tap, hold, paste |
| `pointer` | click, scroll, `at=` selectors |
| `window` | focus, ws, close, move, float |
| `spawn` | spawn |
| `exec` | pane run, anything reaching a shell |
| `web` | web attach, goto, click, fill, read |
| `web.eval` | web eval |
| `fs` | checkpoint, rollback |
| `takeover` | lease acquisition (§11) |

Configured in `~/.config/koto/policy.toml`, overridable per invocation by `--allow` and `--deny`. Deny always wins. A missing capability fails at `require` time, before instruction 0.

```toml
[default]
allow = ["input", "window", "spawn"]
deny  = ["web.eval"]

[profile.beckett]
allow = ["input", "window", "spawn", "exec", "web", "fs"]
budget_ops = 512
seat = "nested"
```

### 9.2 Kill switch

koto installs a Hyprland binding, `SUPER+CTRL+SHIFT+ESC`, that aborts every running koto program, releases held modifiers, and revokes all leases. It is bound at install and cannot be disabled from a basm script. Held modifiers are also released on any abnormal exit, because a stuck `hold super` makes the machine unusable.

### 9.3 Seats

`--seat host` drives the live session. There is one pointer and one keyboard focus, so a host-seat koto program and a human typing will fight, and the human will lose.

`--seat nested` launches a nested Hyprland instance with `WLR_BACKENDS=headless`, which gets its own instance signature, IPC socket, seat, and focus. The agent works there, the human never sees it, and `list windows` inside the nested seat sees only agent windows. This is the correct default for any long-running Beckett task.

`--seat auto` picks nested unless the program declares `require host` or uses an instruction that only makes sense against the live session.

Note that rungs 1 through 3 of the observation ladder do not contend for focus at all, so a well-written script is mostly seat-agnostic. Contention is a property of chords and pointer events specifically.

### 9.4 Checkpoints

On Omarchy's btrfs layout, `checkpoint work` snapshots the subvolume and `rollback work` restores it. Combined with `assert`, this gives a script the shape that makes full machine access survivable:

```
checkpoint pre
; ... mutating work ...
pane run "cargo test"
pane wait ~"test result:" timeout 300s
expect text ~"test result: ok"
jz .done
rollback pre
halt 1
.done:
end text
```

---

## X. The Omarchy standard library

Omarchy's whole interaction model is bindings, so koto ships a macro library that names them. Hardcoding them would rot the moment DHH changes a keybinding, so the library is **generated**:

```sh
koto stdlib sync
```

parses `~/.config/hypr/bindings.conf`, extracts every `bind = MODS, KEY, exec, ...` line, and emits `~/.local/share/koto/stdlib/omarchy.basm` with one `def` per binding, named from the dispatched command. It also emits a settle wait derived from the target's window class.

Generated output looks like:

```
; auto-generated by `koto stdlib sync` from ~/.config/hypr/bindings.conf
def omarchy.terminal()
  key super return
  wait window class=Alacritty timeout 3s
  wait idle 150ms
enddef

def omarchy.browser()
  key super shift return
  wait window class=chromium timeout 5s
  wait idle 250ms
enddef

def omarchy.launcher()
  key super space
  wait window class=walker timeout 2s
enddef

def omarchy.menu()
  key super alt space
  wait window class=walker timeout 2s
enddef
```

An agent then writes `call omarchy.terminal` and never has to know the chord. When a binding changes, `koto stdlib sync` runs from the Omarchy post-update hook directory and every script keeps working.

Web apps get the same treatment. Omarchy's web apps are frameless Chromium windows launched with `--app`, which means each one is a CDP target. The generator emits a `def` per web app that focuses the window and attaches CDP in one step:

```
def omarchy.hey()
  focus class=chromium,title~"HEY" || spawn omarchy-launch-webapp "https://app.hey.com"
  wait window title~"HEY" timeout 10s
  web attach title~"HEY"
enddef
```

---

## XI. Take-over (design sketch, not v1)

The problem: signing Beckett into your accounts through a fresh browser profile is friction, and the credentials end up somewhere you did not intend. Letting it use the browser you are already signed into removes both problems.

Three tiers, increasing in ambition.

**Tier 1: profile attach (buildable now, roughly 2 days).** Change the Omarchy `$browser` binding to launch Chromium with `--remote-debugging-pipe`. Unlike `--remote-debugging-port`, this speaks CDP over inherited file descriptors, so nothing listens on localhost and no other process on the box can hijack the session. `koto web attach` connects through the pipe and gets your live session with every cookie and login intact. No re-authentication, no credential copy, and the browser is the one you are already using.

**Tier 2: leased local take-over (roughly 1 week).** `kotod` runs as a user service and exposes a unix socket. An agent acquires a **lease**: a TTL, an explicit capability set, a target selector allowlist, and an audit sink. While a lease is live, koto changes `general:col.active_border` and posts a persistent waybar element, so the machine visibly announces that something else is driving. Lease expiry releases every held key and restores the border. The lease, the indicator, and the kill switch are the whole safety story, and none of them are optional.

**Tier 3: remote take-over (later).** The transport is uninteresting: forward the `kotod` unix socket over SSH or WireGuard. The tech-support analogy is right about the shape and wrong about the mechanism, since there is no reason to stream a framebuffer when the observation ladder returns text. What actually needs designing is lease negotiation across a network boundary and what happens when the link drops mid-instruction. The answer is probably that `kotod` holds a dead-man timer and rolls back to the last checkpoint on disconnect.

Tier 1 is worth building alongside v1 because it is small and it removes the betterwright login problem immediately.

---

## XII. Example programs

**Open a repo and run tests.**

```
; test-repo.basm  --  usage: koto --script test-repo.basm -- ~/src/beckett
require input, exec
call omarchy.terminal
pane run "cd %1 && cargo test"
pane wait ~"test result:" timeout 300s
expect text ~"test result: ok"
end text
```

**Retry a flaky launch, bounded.**

```
require input, spawn
rep 5 {
  call omarchy.browser
  expect window exists class=chromium
  jz .up
  wait 1s
}
halt 2
.up:
note "browser up after $n attempts"
end silent
```

**Read a web app without a mouse.**

```
require web
call omarchy.hey
web wait ".imbox"
web read main
end text
```

**Poll until a build finishes, observing once.**

```
require exec
pane run "cargo build --release"
while text ~"Compiling" max 60 {
  wait 5s
  see $0
}
end text
```

Note the shape of that last one. Sixty polls, one observation. That ratio is the point of the whole language.

---

## XIII. Milestones

**M1, walking skeleton (2 weeks).** `koto-core` VM with `key`, `type`, `wait`, `focus`, `end`. Hyprland IPC for state and events. Virtual keyboard input. Ladder rungs 4 and 6 only. Inline mode, `--format agent`, exit codes. Ship `koto super enter end` working end to end.

**M2, the useful tool (3 weeks).** Script mode, `--scripts`, registers, labels, `rep`, `while`, macros, `include`. Ladder rungs 2 (tmux) and 3 (atspi). `spawn` through uwsm. Capability model and policy.toml. Kill switch binding.

**M3, agent-grade (3 weeks).** Rung 1 (CDP) and the `web` instruction family. Nested seat. Checkpoint and rollback. `--dry-run`, `--trace`, `--format json`. `koto stdlib sync`.

**M4, take-over tier 1 and 2 (3 weeks).** `--remote-debugging-pipe` attach. `kotod`, leases, the visible indicator.

**M5, portability.** `libkoto` C ABI. Debian backend behind the four traits.

---

## Open questions

1. Does `see` inside a `while` loop accumulate observations or overwrite? Overwrite is simpler; accumulate is what a debugging agent wants. Suggest overwrite by default with `see +$0` for append.
2. Should `focus` fall back to `spawn` when a selector matches nothing? The `||` operator used in the generated `omarchy.hey` macro above implies yes, but that operator is not otherwise in the ISA. Either specify it properly or drop it and use `jz`.
3. Does a nested seat share the clipboard with the host? It should not by default, but `paste` becomes awkward if it cannot.

---

**Next step:** lock the instruction set table in §4 before writing the parser. The `end` collision in §4.1 and the ambiguity-is-an-error rule in §5.1 are the two decisions that are painful to reverse once scripts exist in the wild.
