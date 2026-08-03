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

**Work inline, one step at a time.** Run a few instructions, end with `end
image` (or `end text`), look at what came back, then decide the next call.
Scripts are for flows you have already worked out and intend to repeat — not
for finding your way around a page you have never seen. A script that guesses
at six steps fails on step two and tells you nothing about why.

`end` observes and stops the program; it does not tear down the world. The
browser survives between invocations (see below), so the next call picks up
exactly where this one stopped.

## Instruction set

| Family | Mnemonics |
|---|---|
| Input | `key <chord>`, `tap <key>`, `hold <mod>...`, `release <mod>...`, `type "<text>"`, `paste "<text>"`, `click <sel>` (cap: pointer), `scroll <dir> [n]` |
| Observe | `end [text\|image\|json\|both\|silent]`, `see [$reg] [text\|image]`, `peek <field>`, `ocr` (needs a prior `see image` / `end image` in the same run — it re-reads the last capture and fails with exit 7 if there is none) |
| Wait | `wait <dur>`, `wait window <sel>`, `wait gone <sel>`, `wait title <pat>`, `wait text <pat>`, `wait idle [<dur>]`, `wait exit <pid>`; all accept `timeout <dur>` |
| Window | `focus <sel>`, `ws <n\|+1\|-1\|prev>`, `send ws <n>`, `close [sel]`, `float`, `tile`, `full`, `pin`, `swap <l\|r\|u\|d>`, `move <l\|r\|u\|d>`, `monitor <name>`, `list <windows\|workspaces\|monitors\|clients>` |
| Process | `spawn <cmd>...` (cap: spawn), `kill <sel\|scope>` |
| Terminal | `pane new [name]`, `pane send "<text>"`, `pane run "<cmd>"`, `pane read [n]`, `pane wait <pat>`, `pane kill [name]` |
| Web | `web attach [bw\|<browser>\|<sel>]` (cap: web), `web goto <url>`, `web back/forward/reload`, `web click <ref\|css\|text=> [newtab]`, `web type <target> "<text>" [append]`, `web fill <target> "<text>"`, `web scroll <n>`, `web hover <target>`, `web press <key>`, `web select <target> <value>`, `web read [full] [diff] [ref=eN] [selector=<css>] [urls]`, `web wait <target>`, `web shot [name] [annotate] [full] [kind=proof]`, `web pdf [name]`, `web open [url]` / `web use <tab>` / `web close [tab]` / `web pages`, `web overlays` / `web controls` / `web media`, `web dialog accept\|dismiss`, `web captcha solve\|inspect\|click\|drag\|text`, `web eval "<js>"` (cap: web.eval), `web login <host>` + `web creds ...` (cap: web.login), `web download <url\|target> [to=<dir>]` (cap: web.download), `web view start\|stop\|status`, `web handoff`, `web ask`, `web chat`, `web session close`. The session persists across invocations; `--web-stop` ends it. |
| Flow | `.label:`, `jmp`, `jz`, `jnz`, `je <reg> <val> .label`, `rep <n>`, `while <pred> max <n>`, `call`, `ret`, `def`/`enddef`, `include` |
| Guards | `require <cap>,...`, `assert <pred>`, `expect <pred>`, `budget ops <n>\|time <dur>` |
| Meta | `note "<text>"`, `nop`, `halt [code]` |

Capabilities you may need in `require` / `--allow`: `input`, `window`, `spawn`,
`exec`, `web`, `web.eval`, `web.login`, `web.download`, `pointer`, `fs`. Note
`pane run` requires `exec` — `spawn` alone is not enough — and `web eval`
requires `web.eval` on top of `web`.

## Web engines and the persistent session

`web attach` picks the engine, and **both engines' browsers persist between koto
invocations**. Navigate in one call, click in the next, fill a form in a third:
the page, the tabs, and the scroll position stay where you left them. Only the
first attach pays for a browser launch (~5s); the rest connect in about a
second. `koto --web-status` reports both sessions, `koto --web-stop` closes
them.

Bare `web attach` (or `web attach <browser>`) drives Chrome over CDP in a
koto-owned profile and supports only goto/read/click/fill/wait/eval. `web
attach bw` uses BetterWright 1.6.3 (needs node and `npm i -g
betterwright@1.6.3`; an older install is refused at attach) and exposes the
whole engine, one koto action per betterwright function. Attach options:
`profile=<name>` (separate cookie jar), `session=<name>` (parallel lane in one
browser), `platform=linux|macos|windows` (identity fingerprint; koto defaults
to linux so the headed browser renders at host scale instead of
betterwright's 2x-Retina macOS default).

- **Every action answers "where am I now".** Success returns a
  `page <id> <url> "<title>"` line; `type`/`fill` also echo the field's actual
  value (passwords redact). A missed target returns the page it missed on,
  the nearest live candidates with refs, and a `help:` line — read the error
  before re-reading the page; it usually already contains your next target.
- **Read before acting.** `web read` is `snapshot({interactive:true})` — a
  pruned accessibility tree with `[ref=eN]` markers that work directly as
  targets. `full` reads content wholesale, `diff` shows only what changed since
  the last same-shaped read, `ref=eN`/`selector=<css>` scope to a subtree,
  `urls` keeps link hrefs. Refs are reassigned on every read and go stale when
  the page changes, so re-read before acting. A scoped read needs to match
  exactly one element: too many is exit 6, none is exit 3, and both errors name
  the fix. Escalate `read` → `read full` →
  `shot annotate` (boxes every interactive element with its ref) only as far as
  the question needs.
- **Visible actions are human-shaped.** `web click`, `web type`, and `web
  scroll` run betterwright's `human.*` helpers (curved pointer, bounded key
  timing). `web fill` is the precise, instant `locator.fill` for when shaping
  does not matter. `web press`/`web hover`/`web select` cover keys, hovers, and
  dropdowns.
- **Tabs are a session, and the session outlives the invocation.** `web open
  [url]`, `web use <tab>`, `web close`, `web pages`; `web click <link> newtab`
  Ctrl+clicks a link into a background tab and returns the new tab's summary.
  Tabs and the current page survive between koto runs until `--web-stop`, so a
  later invocation can `web read` or `web click` without re-navigating. Refs
  (`eN`) are the exception — they go stale on any page change, so re-read.
- **Verify, don't infer.** `web overlays` dismisses cookie/promo layers,
  `web controls` reports exact form-control state, `web media` reports what is
  actually playing. `web dialog accept|dismiss` arms the next JS dialog before
  the click that triggers it.
- **Challenges are resumable.** Results carry `#challenge`/`#warn` trailer
  lines. `web captcha solve` runs the local solver; `inspect`/`click`/`drag`/
  `text` (CSS-pixel bounds) handle the vision stages. Never repeat a failed
  stage — after a rejection, switch source or take `web handoff`.
- **Credentials never surface.** `web login <host>` fills a vaulted secret;
  `web creds list/inspect/fill/generate/pending/commit/discard` manage the
  vault by metadata only. Commit a generated password only after the site
  visibly accepts it; discard on failure.
- **A human is one command away.** `web view start` prints a live-view URL,
  `web handoff "check the cart" timeout=600` blocks until the human clicks
  Done, `web ask "which color?" options=red|blue` waits for a typed answer,
  `web chat` drains their freeform guidance between steps.

The two engines keep separate profiles and separate logins.

**Targets** for `web click`, `web type`, `web fill`, `web hover`, `web select`,
and `web wait` are, in order of preference: `eN` (a ref from the last bw `web
read`), `text=save draft` (matches the visible words on a button, link, or
label — survives a class rename), or a CSS selector. A target that matches
nothing is exit 3, never a silent no-op.

`web eval` runs in betterwright's **worker**, not the page — there is no
`document`. Its globals are `page`, `pages`, `snapshot`, `human`, `credentials`;
reach the DOM through `page.evaluate(() => document…)`.

Reach for `web eval` only when no instruction covers what you need. A flow
written entirely in evals is a JavaScript program wearing koto as a hat: it
gives up the observation ladder, the exit contract, and the capability gate,
which are the reasons to use koto at all.

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
