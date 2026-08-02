# Specification implementation tracker

Legend: `[x]` implemented, `[~]` implemented with documented gaps, `[ ]` not implemented.
This is a live engineering checklist for `spec.md` v0.1.

## Invocation and output

- [x] Inline invocation, script files, repeated `--script`, `--scripts`, and stdin.
- [x] Script argument expansion (`%1` through `%9`, `%*`) and shared register/session state.
- [x] Global parsing for format, observation policy, timeouts, budgets, policy controls, trace, session, and profile.
- [~] `--seat` is parsed and emitted, but nested seats are not available yet.
- [x] Agent, raw, quiet, and JSON output modes.
- [x] JSON image inlining via `--inline-images`.
- [x] JSON and agent output include focused-window metadata; trace entries include argument arrays.
- [x] Failed runs persist the completed and failing instruction entries to `--trace`.

## basm core

- [x] Lexer, quoted strings, comments, mnemonic arity, bare chord shorthand, and the `end` collision rule.
- [x] Registers, persistent sessions, labels, jumps, calls, returns, bounded `rep`, bounded `while`, definitions, and relative includes.
- [x] `require`, `assert`, `expect`, `budget`, `note`, `nop`, and `halt`.
- [~] Definition parameters and macro arguments are not bound.
- [~] Predicates implement text/register comparisons and window exists/count; live-title semantics remain incomplete.
- [ ] The unspecified `||` fallback example syntax is not supported.

## Control

- [x] Direct virtual-keyboard and virtual-pointer Wayland input.
- [x] XKB active-layout Unicode type synthesis, direct chords, held keys, pointer, and clipboard paste.
- [x] Hyprland focus/workspace/window/process/list controls and strict selector resolution.
- [x] Hyprland event-socket idle waits plus window/gone/title/text/exit waits.
- [x] Dedicated isolated tmux server and pane new/send/run/read/wait/kill operations.
- [~] Managed Chromium CDP transport over `--remote-debugging-pipe`: attach/launch, goto, click, fill, read, wait, and eval. Attaching an already-running inherited pipe is pending.

## Observation

- [x] tmux exact-text rung.
- [x] Hyprland metadata rung.
- [x] In-process wlr-screencopy PNG rung, `end image`/`end both`, and JSON image inlining.
- [x] OCR instruction over the latest capture when Tesseract is installed.
- [~] CDP accessibility snapshot is returned by `web read`; it is not yet an automatic top observation-ladder rung.
- [x] AT-SPI2 bounded structured-tree rung.
- [~] OCR uses the system Tesseract executable rather than an embedded engine.

## Safety, isolation, and integration

- [x] TOML policy profiles, deny-wins capability resolution, and preflight requirements.
- [x] Emergency kill-switch installation, cancellation marker, and held-key release on runtime exit.
- [~] Cancellation is checked between instructions, not inside every blocking backend operation.
- [ ] Nested Hyprland seats.
- [~] Btrfs checkpoints and rollback for explicitly configured subvolumes (`KOTO_BTRFS_SUBVOLUME`, `KOTO_BTRFS_SNAPSHOT_DIR`); automatic Omarchy layout discovery is pending.
- [ ] `kotod`, leases, visible takeover indication, and remote take-over.
- [~] `libkoto` exports ABI version and parser validation only; executable C ABI remains.
- [~] Omarchy binding generator handles normal `bind = ..., exec, ...` lines with common target heuristics; web-app generation remains.
