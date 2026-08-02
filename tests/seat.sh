#!/usr/bin/env bash
# Persistent nested seat: does work done by one invocation survive for the next,
# and does it stay out of the user's session?
#
# This is the property the old ephemeral seat could not provide — it tore the
# compositor down on drop, so every window died with the process that opened it.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
KOTO=${KOTO_BIN:-./target/release/koto}
[[ -x $KOTO ]] || { echo "build first: cargo build --release -p koto" >&2; exit 1; }
HOST_SIG="${HYPRLAND_INSTANCE_SIGNATURE:-}"
pass=0; fail=0

ok()  { printf '  \033[32mok\033[0m    %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n        %s\n' "$1" "$2"; fail=$((fail+1)); }

cleanup() { "$KOTO" --seat-stop >/dev/null 2>&1; }
trap cleanup EXIT

echo "koto nested seat  (binary: $KOTO)"
echo
cleanup

# --- starts and reports itself -----------------------------------------------
"$KOTO" --seat nested --allow window list windows end silent >/tmp/koto-seat.log 2>&1 </dev/null
status=$?
if [[ $status -ne 0 ]]; then
  bad "seat starts on first use" "exit=$status $(tail -1 /tmp/koto-seat.log)"
else
  ok "seat starts on first use"
fi

STATUS_LINE=$("$KOTO" --seat-status 2>/dev/null)
case "$STATUS_LINE" in
  "seat running"*) ok "seat is recorded and running" ;;
  *) bad "seat is recorded and running" "status said: $STATUS_LINE" ;;
esac

SEAT_SIG=$(sed -n 's/.*signature=\([^ ]*\).*/\1/p' <<<"$STATUS_LINE")
[[ -n "$SEAT_SIG" && "$SEAT_SIG" != "$HOST_SIG" ]] \
  && ok "seat is a different compositor from the host session" \
  || bad "seat is a different compositor from the host session" "seat=$SEAT_SIG host=$HOST_SIG"

# --- windows survive between invocations -------------------------------------
"$KOTO" --seat nested --allow window,spawn \
  spawn alacritty --title KOTOSEAT_PERSIST end silent >/dev/null 2>&1 </dev/null
sleep 3

INSIDE=$(HYPRLAND_INSTANCE_SIGNATURE="$SEAT_SIG" hyprctl -j clients 2>/dev/null | python3 -c '
import json,sys
print(sum(1 for w in json.load(sys.stdin)
          if "KOTOSEAT_PERSIST" in w.get("title","")+w.get("initialTitle","")))' 2>/dev/null)
[[ "$INSIDE" == "1" ]] \
  && ok "a window opened in one invocation is visible to the next" \
  || bad "a window opened in one invocation is visible to the next" "found $INSIDE in the seat"

# --- and never leak to the host ----------------------------------------------
OUTSIDE=$(HYPRLAND_INSTANCE_SIGNATURE="$HOST_SIG" hyprctl -j clients 2>/dev/null | python3 -c '
import json,sys
print(sum(1 for w in json.load(sys.stdin)
          if "KOTOSEAT_PERSIST" in w.get("title","")+w.get("initialTitle","")))' 2>/dev/null)
[[ "$OUTSIDE" == "0" ]] \
  && ok "the host session never sees the seat's windows" \
  || bad "the host session never sees the seat's windows" "found $OUTSIDE on the host"

# --- teardown is real --------------------------------------------------------
"$KOTO" --seat-stop >/dev/null 2>&1
sleep 1
[[ "$("$KOTO" --seat-status 2>/dev/null)" == "no seat" ]] \
  && ok "stop tears the seat down" \
  || bad "stop tears the seat down" "status still reports a seat"

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]]
