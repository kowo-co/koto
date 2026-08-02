#!/usr/bin/env bash
# Integration tests against a live compositor.
#
# The unit suite exercises the parser and the VM; none of it touches the four
# backends that actually break — Wayland input, Hyprland state, tmux, and CDP.
# Every bug found by driving koto with an agent lived in that gap, so these
# tests assert observable effects (bytes arrived, window moved) rather than
# return values.
#
# usage: tests/integration.sh [name ...]      # no args runs everything
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
KOTO=${KOTO_BIN:-./target/release/koto}
[[ -x $KOTO ]] || { echo "build first: cargo build --release -p koto" >&2; exit 1; }

TAG=KOTOIT           # every window this suite makes is tagged for cleanup
pass=0; fail=0; failed_names=()

ok()   { printf '  \033[32mok\033[0m    %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n        %s\n' "$1" "$2"; fail=$((fail+1)); failed_names+=("$1"); }
have() { command -v "$1" >/dev/null 2>&1; }

# Killing the server and immediately starting koto races: koto's `start-server`
# can attach to a server that is still shutting down, which surfaces as
# "server exited unexpectedly". Wait for it to actually be gone.
reset_tmux() {
  tmux -L koto kill-server 2>/dev/null
  for _ in $(seq 1 40); do
    tmux -L koto has-session 2>/dev/null || return 0
    sleep 0.05
  done
}

cleanup_windows() {
  hyprctl -j clients 2>/dev/null | python3 -c "
import json,subprocess,sys
for w in json.load(sys.stdin):
    if '$TAG' in w.get('title','')+w.get('initialTitle',''):
        subprocess.run(['hyprctl','dispatch','closewindow','address:'+w['address']],check=False)
" >/dev/null 2>&1
}
trap 'cleanup_windows; tmux -L koto kill-server 2>/dev/null' EXIT

run_basm() { # script-text -> stdout+stderr in $OUT, status in $STATUS
  local script="$1"; shift
  printf '%s\n' "$script" > /tmp/koto-it.basm
  # stdout must be a file, never a pipe: spawned apps inherit the descriptor
  # and would hold a pipe open long after koto exits.
  timeout 60 "$KOTO" "$@" --script /tmp/koto-it.basm >/tmp/koto-it.log 2>&1 </dev/null
  STATUS=$?; OUT=$(cat /tmp/koto-it.log)
}

# ---------------------------------------------------------------- hyprland ---
test_hypr_selectors() {
  local name="hyprland: focused resolves to exactly one window"
  run_basm "require window,spawn
spawn alacritty --title ${TAG}_SEL
wait window title~\"${TAG}_SEL\" timeout 10s
focus title~\"${TAG}_SEL\"
end text" --allow window,spawn
  # Regression guard for focusHistoryID: with it unset, `focused` matched every
  # window and this exited 6 (ambiguous) whenever a second window existed.
  if [[ $STATUS -ne 0 ]]; then bad "$name" "exit=$STATUS $(tail -1 /tmp/koto-it.log)"; return; fi
  grep -q "${TAG}_SEL" <<<"$OUT" && ok "$name" || bad "$name" "focused window was not the one we opened"
}

test_hypr_workspace_move() {
  local name="hyprland: send ws moves a window and metadata agrees"
  run_basm "require window,spawn
spawn alacritty --title ${TAG}_WS
wait window title~\"${TAG}_WS\" timeout 10s
focus title~\"${TAG}_WS\"
send ws 9
end silent" --allow window,spawn
  local landed
  landed=$(hyprctl -j clients 2>/dev/null | python3 -c "
import json,sys
print(next((w['workspace']['id'] for w in json.load(sys.stdin)
            if '${TAG}_WS' in w.get('title','')+w.get('initialTitle','')), 'missing'))")
  [[ "$landed" == "9" ]] && ok "$name" || bad "$name" "expected workspace 9, found $landed"
}

# ------------------------------------------------------------------- input ---
test_input_typing() {
  local name="input: typed text arrives byte-for-byte"
  rm -f /tmp/koto-it-typed.txt
  run_basm "require input,window,spawn
spawn alacritty --title ${TAG}_TYPE -e sh -c \"cat > /tmp/koto-it-typed.txt\"
wait window title~\"${TAG}_TYPE\" timeout 10s
focus title~\"${TAG}_TYPE\"
wait idle 400ms
type \"koto integration 0123456789\"
key return
key ctrl d
wait idle 500ms
end silent" --allow input,window,spawn
  sleep 1
  # Regression guard for the write-only keymap fd, which made every keystroke
  # a no-op while koto still reported success.
  local got; got=$(head -1 /tmp/koto-it-typed.txt 2>/dev/null)
  [[ "$got" == "koto integration 0123456789" ]] && ok "$name" || bad "$name" "got [$got]"
}

test_input_sustained() {
  local name="input: 40 lines survive one session"
  rm -f /tmp/koto-it-typed.txt
  run_basm "require input,window,spawn
spawn alacritty --title ${TAG}_BULK -e sh -c \"cat > /tmp/koto-it-typed.txt\"
wait window title~\"${TAG}_BULK\" timeout 10s
focus title~\"${TAG}_BULK\"
wait idle 400ms
rep 40 {
type \"the quick brown fox jumps over the lazy dog\"
key return
}
key ctrl d
wait idle 600ms
end silent" --allow input,window,spawn --budget-ops 600
  sleep 1
  # The event queue was never drained, so the compositor eventually
  # disconnected us and input died partway through a long session.
  local lines; lines=$(wc -l < /tmp/koto-it-typed.txt 2>/dev/null || echo 0)
  [[ "$lines" == "40" ]] && ok "$name" || bad "$name" "expected 40 lines, got $lines"
}

# -------------------------------------------------------------------- tmux ---
test_tmux_many_panes() {
  local name="tmux: eight panes can coexist"
  reset_tmux
  # split-window subdivides one fixed area and fails around the fourth pane;
  # each pane must get its own window.
  run_basm "require exec
rep 8 {
pane new
}
pane run \"echo pane-ready\"
pane wait \"pane-ready\"
end silent" --allow exec --budget-ops 200
  [[ $STATUS -eq 0 ]] && ok "$name" || bad "$name" "exit=$STATUS $(tail -1 /tmp/koto-it.log)"
}

test_tmux_exact_read() {
  local name="tmux: pane read returns command output, not the echoed command"
  reset_tmux
  # The marker must not appear in the command text. `pane wait` scans the whole
  # pane, so a pattern that also occurs in the typed command matches the command
  # itself and returns before the command has produced anything.
  run_basm "require exec
pane new
pane run \"echo koto-marker | tr a-z A-Z\"
pane wait \"KOTO-MARKER\"
pane read 10
end text" --allow exec
  grep -q "KOTO-MARKER" <<<"$OUT" && ok "$name" \
    || bad "$name" "exit=$STATUS; output was: $(tr '\n' '|' <<<"$OUT" | tail -c 220)"
}

# ------------------------------------------------------------- observation ---
test_tmux_wait_waits() {
  local name="tmux: pane wait waits for output, not its own echo"
  reset_tmux
  # `pane wait` used to scan the whole pane, so it matched the command line the
  # shell had just echoed and returned before the command produced anything.
  # Sleeping for two seconds makes that visible: a premature return finishes in
  # well under a second.
  local started elapsed
  started=$(date +%s%N)
  run_basm "require exec
pane new
pane run \"sleep 2 && echo koto-late-marker\"
pane wait \"koto-late-marker\"
end silent" --allow exec --timeout 20s
  elapsed=$(( ($(date +%s%N) - started) / 1000000 ))
  if [[ $STATUS -ne 0 ]]; then bad "$name" "exit=$STATUS $(tail -1 /tmp/koto-it.log)"; return; fi
  if [[ $elapsed -lt 1800 ]]; then
    bad "$name" "returned after ${elapsed}ms — it matched the echoed command, not the output"
  else
    ok "$name"
  fi
}

test_observe_tmux_rung() {
  local name="observe: tmux rung reports exact fidelity"
  reset_tmux
  run_basm "require exec
pane new
pane run \"echo ladder-check\"
pane wait \"ladder-check\"
pane read 5
end text" --allow exec
  grep -q "source tmux fidelity=exact" <<<"$OUT" && ok "$name" || bad "$name" "expected the tmux rung, got: $(grep '^source' <<<"$OUT" | head -1)"
}

test_observe_image() {
  local name="observe: screencopy produces a real PNG"
  run_basm "require window,spawn
spawn alacritty --title ${TAG}_SHOT
wait window title~\"${TAG}_SHOT\" timeout 10s
focus title~\"${TAG}_SHOT\"
wait idle 300ms
end image" --allow window,spawn
  local path; path=$(grep -oE '/[^ ]+\.png' <<<"$OUT" | tail -1)
  if [[ -z "$path" || ! -f "$path" ]]; then bad "$name" "no image path in output"; return; fi
  # Assert it is actually a PNG, not a zero-byte placeholder.
  python3 - "$path" <<'PY' && ok "$name" || bad "$name" "not a valid non-empty PNG"
import struct,sys
data=open(sys.argv[1],'rb').read()
assert data[:8]==b'\x89PNG\r\n\x1a\n', 'bad signature'
w,h=struct.unpack('>II', data[16:24])
assert w>0 and h>0 and len(data)>1000, f'implausible image {w}x{h} {len(data)}B'
PY
}

# --------------------------------------------------------------------- run ---
ALL=(test_hypr_selectors test_hypr_workspace_move
     test_input_typing test_input_sustained
     test_tmux_many_panes test_tmux_exact_read test_tmux_wait_waits
     test_observe_tmux_rung test_observe_image)

echo "koto integration suite  (binary: $KOTO)"
echo
selected=("$@")
for t in "${ALL[@]}"; do
  if [[ ${#selected[@]} -gt 0 ]]; then
    printf '%s\n' "${selected[@]}" | grep -qF "${t#test_}" || continue
  fi
  "$t"
  cleanup_windows
done

echo
echo "passed=$pass failed=$fail"
[[ $fail -gt 0 ]] && { printf 'failed: %s\n' "${failed_names[*]}"; exit 1; }
exit 0
