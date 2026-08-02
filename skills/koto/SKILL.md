---
name: koto
description: Drive a Linux/Hyprland desktop keyboard-first through the koto CLI, which executes basm (beckett assembly). Use for any GUI computer-use task on this machine — launching and arranging windows, typing into applications, operating web pages through a real browser, or reading structured observations instead of screenshots.
---

# Using koto

You drive a Linux/Hyprland desktop by invoking the `koto` CLI. koto executes
**basm** (beckett assembly): explicit keyboard chords, strict window selectors,
bounded control flow, and one final observation instead of a screenshot stream.

## Invocation

```
koto <instruction>...                  # inline mode, argv token stream
koto --script FILE                     # script mode, one instruction per line
koto --script FILE -- ARG1 ARG2        # args bind to %1, %2
```

Useful globals: `--allow a,b` / `--deny a,b` (capabilities), `--timeout 10s`,
`--budget-ops 256`, `--budget-time 120s`, `--format agent|json|raw|quiet`,
`--observe auto|text|image|both`, `--dry-run`, `--explain`.

Prefer **one script per tool call** over many inline calls. `end` is the only
expensive instruction: forty instructions plus one `end` costs one observation.

## Instruction set

| Family | Mnemonics |
|---|---|
| Input | `key <chord>`, `tap <key>`, `hold <mod>...`, `release <mod>...`, `type "<text>"`, `paste "<text>"`, `click <sel>` (cap: pointer), `scroll <dir> [n]` |
| Observe | `end [text\|image\|json\|both\|silent]`, `see [$reg] [text\|image]`, `peek <field>`, `ocr` (needs a prior `see image` / `end image` in the same run — it re-reads the last capture and fails with exit 7 if there is none) |
| Wait | `wait <dur>`, `wait window <sel>`, `wait gone <sel>`, `wait title <pat>`, `wait text <pat>`, `wait idle [<dur>]`, `wait exit <pid>`; all accept `timeout <dur>` |
| Window | `focus <sel>`, `ws <n\|+1\|-1\|prev>`, `send ws <n>`, `close [sel]`, `float`, `tile`, `full`, `pin`, `swap <l\|r\|u\|d>`, `move <l\|r\|u\|d>`, `monitor <name>`, `list <windows\|workspaces\|monitors\|clients>` |
| Process | `spawn <cmd>...` (cap: spawn), `kill <sel\|scope>` |
| Terminal | `pane new [name]`, `pane send "<text>"`, `pane run "<cmd>"`, `pane read [n]`, `pane wait <pat>`, `pane kill [name]` |
| Web | `web attach [bw\|<browser>\|<sel>]` (cap: web), `web goto <url>`, `web click <ref\|css\|text=>`, `web fill <target> "<text>"`, `web read [full]`, `web wait <target>`, `web shot [name]`, `web eval "<js>"` (cap: web.eval), `web login <host>` (cap: web.login), `web download <url\|target> [to=<dir>]` (cap: web.download) |
| Flow | `.label:`, `jmp`, `jz`, `jnz`, `je <reg> <val> .label`, `rep <n>`, `while <pred> max <n>`, `call`, `ret`, `def`/`enddef`, `include` |
| Guards | `require <cap>,...`, `assert <pred>`, `expect <pred>`, `budget ops <n>\|time <dur>` |
| Meta | `note "<text>"`, `nop`, `halt [code]` |

Capabilities you may need in `require` / `--allow`: `input`, `window`, `spawn`,
`exec`, `web`, `web.eval`, `web.login`, `web.download`, `pointer`, `fs`. Note
`pane run` requires `exec` — `spawn` alone is not enough — and `web eval`
requires `web.eval` on top of `web`.

## Web engines

`web attach` picks the engine. Bare (or a browser name) launches a managed
Chromium over CDP in a koto-owned profile; a `title~` selector attaches an
inherited `--remote-debugging-pipe`. **`web attach bw`** uses BetterWright
(requires node + `npm i -g betterwright`): `web read` returns a pruned
accessibility snapshot with `[ref=eN]` markers, and those refs work directly in
`web click`/`web fill`/`web wait` — no CSS reverse-engineering. Refs are
reassigned on every read and go stale when the page changes; re-read before
acting. `web shot`, `web login`, and `web download` exist only on the bw
engine. Prefer bw for real websites; it is the rung that works when a site
ignores accessibility.

## Selectors

Comma is AND. `=` is exact, `~` is a regex.

```
class=chromium      title~"^nvim"     addr=0x55f1a2    pid=4821
scope=app-foo.scope ws=3              role=button      name="Save"
web=#login          focused           last
```

Zero matches is exit 3; more than one is exit 6 (except `close`, `kill`,
`list`, which accept sets). Ambiguity is an error, never a coin flip.

## Lexical rules that bite

- Script mode: one instruction per line; `;` starts a comment outside quotes.
- Inline mode is an argv token stream. A bare non-mnemonic token implies `key`,
  so `koto super enter` is one chord and `koto enter enter` is two presses.
- **`end` collides with the End key.** At a bare instruction boundary `end`
  halts and observes. After `key` or a modifier (`ctrl end`, `key end`) it is
  the End key.
- `while` **must** carry `max <n>`. There is no unbounded loop and no flag to
  enable one.
- `type` synthesises keysyms and is layout-aware; use `paste` above ~200 chars
  or for anything outside the active keymap.

## Registers

`$0`–`$9` general purpose, `$?` last status, `$w` focused window record
(dotted: `$w.class`), `$out` last observation text, `$n` loop counter,
`%1`–`%9`/`%*` script args, `$env.NAME` environment passthrough. Registers
persist across `--script` files in one invocation.

## Exit codes

| 0 | success | 1 | assertion failed | 2 | timeout | 3 | selector matched nothing |
|---|---|---|---|---|---|---|---|
| 4 | budget exceeded | 5 | capability denied | 6 | selector ambiguous | 7 | observation unavailable |
| 8 | basm parse error | 9 | backend unavailable | 64 | internal error | | |

## Working style

- **Keyboard first.** `click` needs the `pointer` capability and always warns in
  the trace. Coordinates never appear in a well-written script.
- **Screenshots are available.** `end image` (or `end both`) captures the screen
  and returns it to you as an image. On the bw engine it captures the page
  itself, even occluded.
- **Structured before pixels.** The observation ladder runs CDP/BetterWright →
  AT-SPI → tmux → Hyprland metadata → screencopy → OCR. `end text` and
  `web read` cost far less than an image. Reach for `end image` when the rungs
  above fail.
- **Settle explicitly.** `wait idle 200ms` after a window-opening chord is exact
  (it reads Hyprland's event socket), unlike a sleep.
- `list clients` writes JSON to `$out` and is the cheapest way to build a world
  model.
- Start every script with `require`, so a policy violation fails before
  instruction zero rather than halfway through a mutation.

## Worked example

```basm
require input,window,spawn
spawn alacritty --title KOTO_EVAL_WIN_1
wait window title~"KOTO_EVAL_WIN_1" timeout 10s
send ws 2
focus title~"KOTO_EVAL_WIN_1"
float
end text
```
