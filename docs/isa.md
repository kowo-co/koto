# basm instruction set: v0.1

This table is the compatibility boundary for scripts. Mnemonics and their
operand shape are stable within 0.1; a backend may report exit 9 when its
platform facility is unavailable, but it must not silently ignore one.

| Family | Mnemonics | Operand contract |
|---|---|---|
| Input | `key`, `tap`, `hold`, `release`, `type`, `paste`, `click`, `scroll` | `key`/`hold`/`release` take a chord; modifier prefixes bind rightward. `tap` is required for a modifier alone. |
| Observe | `end`, `see`, `peek`, `ocr` | `end [text\|image\|json\|both\|silent]`; bare boundary `end` halts and observes. |
| Wait | `wait` | duration, `window`, `gone`, `title`, `text`, `idle`, or `exit`; optional `timeout <dur>`. |
| Window | `focus`, `ws`, `send ws`, `close`, `float`, `tile`, `full`, `pin`, `swap`, `move`, `monitor`, `list` | Window selectors are strict. |
| Process | `spawn`, `kill` | `spawn` accepts a command vector; `kill` accepts a selector or scope. |
| Terminal | `pane new/send/run/read/wait/kill` | Managed tmux control-mode panes. |
| Web | `web attach/goto/back/forward/reload/click/type/fill/scroll/hover/press/select/read/wait/eval/shot/pdf/open/use/close/pages/overlays/controls/media/dialog/captcha/login/creds/download/view/handoff/ask/chat/session` | Raw CDP (goto/read/click/fill/wait/eval only) or the BetterWright engine; `web attach bw` selects the latter, which maps each action onto one betterwright 1.6.3 function. |
| Flow | labels, `jmp`, `jz`, `jnz`, `je`, `rep`, `while`, `call`, `ret`, `def`, `enddef`, `include` | `while` **must** include `max <n>`. |
| Guards | `require`, `assert`, `expect`, `budget`, `checkpoint`, `rollback` | `require` validates before instruction zero. |
| Meta | `note`, `nop`, `halt` | `halt [code]` exits without implicit observation. |

## Lexical rules

- Script mode has one instruction per line. `;` starts a comment outside a
  quoted string.
- Inline mode is an argv token stream. Bare non-mnemonic key tokens imply
  `key`; thus `super enter` is a single chord and `enter enter` is two presses.
- `end` is the End key after `key` or a modifier (`ctrl end`), and is the halt
  mnemonic at a bare instruction boundary.
- Selector terms are comma-separated AND clauses. `=` is exact and `~` is a
  validated regular expression. A zero match is exit 3; multiple matches are
  exit 6 except for explicitly set-oriented operations.

## Registers and budgets

`$0`–`$9`, `$?`, `$w`, `$out`, `$n`, `%1`–`%9`, `%*`, and `$env.NAME` are the
reserved register namespace. Scripts have a default 256-op and 120-second
budget. There is no flag or instruction that permits an unbounded loop.
