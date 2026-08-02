#!/usr/bin/env bash
# BetterWright engine, end to end: the pruned [ref=eN] snapshot, ref targeting,
# page screenshots, and the session daemon that outlives a sidecar.
#
# BetterWright is a runtime-optional dependency (node + `npm i -g betterwright`).
# When it is absent this suite SKIPS — exit 0 with a message. A machine without
# it must not report a failure it cannot fix.
#
# usage: tests/web-bw.sh
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
KOTO=${KOTO_BIN:-./target/release/koto}
[[ -x $KOTO ]] || KOTO=./target/debug/koto
[[ -x $KOTO ]] || { echo "build first: cargo build --release -p koto" >&2; exit 1; }

pass=0; fail=0; failed_names=()
ok()  { printf '  \033[32mok\033[0m    %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n        %s\n' "$1" "$2"; fail=$((fail+1)); failed_names+=("$1"); }

# --- probe -------------------------------------------------------------------
# Same resolution order the sidecar uses: plain import, then
# $KOTO_BETTERWRIGHT_DIR, then `npm root -g`.
command -v node >/dev/null 2>&1 || { echo "skip: betterwright not installed"; exit 0; }
NPM_G=$(npm root -g 2>/dev/null)
NPM_G="$NPM_G" node --input-type=module -e '
import {createRequire} from "node:module";
import {pathToFileURL} from "node:url";
const tries = ["betterwright"];
for (const d of [process.env.KOTO_BETTERWRIGHT_DIR, process.env.NPM_G].filter(Boolean)) {
  try { tries.push(pathToFileURL(createRequire(d + "/koto.js").resolve("betterwright")).href); } catch {}
}
for (const t of tries) { try { await import(t); process.exit(0); } catch {} }
process.exit(1);
' >/dev/null 2>&1 || { echo "skip: betterwright not installed"; exit 0; }

OBS="${XDG_RUNTIME_DIR:-/tmp}/koto/obs"
BTN='data:text/html,<button>koto-press</button>'
CLICKY='data:text/html,<button onclick=this.textContent=%22koto-clicked%22>koto-press</button>'
FIELD='data:text/html,<input id=q aria-label=field>'

run_basm() { # script-text [koto args...] -> $OUT, $STATUS
  local script="$1"; shift
  printf '%s\n' "$script" > /tmp/koto-bw.basm
  # stdout must be a file, never a pipe: the node sidecar and the browser it
  # starts inherit the descriptor and would hold a pipe open past koto's exit.
  timeout 120 "$KOTO" "$@" --script /tmp/koto-bw.basm >/tmp/koto-bw.log 2>&1 </dev/null
  STATUS=$?; OUT=$(cat /tmp/koto-bw.log)
}

echo "koto betterwright suite  (binary: $KOTO)"
echo

# --- the snapshot is pruned and ref-tagged -----------------------------------
name="bw: read returns a [ref=eN] snapshot"
run_basm "require web
web attach bw
web goto \"$BTN\"
web read
end text" --allow web
grep -qF '[ref=' <<<"$OUT" && ok "$name" \
  || bad "$name" "exit=$STATUS; no refs in: $(tr '\n' '|' <<<"$OUT" | tail -c 220)"

# --- a ref taken from one snapshot drives a click ----------------------------
name="bw: click by ref changes the page"
REF=$(grep -oE 'ref=(f[0-9]+)?e[0-9]+' <<<"$OUT" | head -1 | cut -d= -f2)
if [[ -z "$REF" ]]; then bad "$name" "no ref to click"; else
run_basm "require web
web attach bw
web goto \"$CLICKY\"
web read
web click $REF
web read
end text" --allow web
grep -qF 'koto-clicked' <<<"$OUT" && ok "$name" \
  || bad "$name" "exit=$STATUS; ref=$REF; $(tr '\n' '|' <<<"$OUT" | tail -c 220)"
fi

# --- fill takes a CSS selector, not just a ref -------------------------------
name="bw: fill by css selector, read shows the value"
run_basm "require web
web attach bw
web goto \"$FIELD\"
web fill \"#q\" \"koto-typed\"
web read
end text" --allow web
grep -qF 'koto-typed' <<<"$OUT" && ok "$name" \
  || bad "$name" "exit=$STATUS; $(tr '\n' '|' <<<"$OUT" | tail -c 220)"

# --- web shot lands a real PNG in the observation directory ------------------
name="bw: web shot writes a PNG into the obs dir"
run_basm "require web
web attach bw
web goto \"$BTN\"
web shot
end silent" --allow web
SHOT=$(grep -oE '/[^ ]+\.png' <<<"$OUT" | tail -1)
if [[ -z "$SHOT" || ! -f "$SHOT" ]]; then bad "$name" "exit=$STATUS; \$out was not a path: $OUT"
elif [[ "$(head -c 8 "$SHOT" | od -An -tx1 | tr -d ' \n')" != "89504e470d0a1a0a" ]]; then
  bad "$name" "$SHOT is not a PNG: $(file -b "$SHOT" 2>/dev/null)"
elif [[ "$SHOT" != "$OBS"/* ]]; then bad "$name" "$SHOT is outside $OBS"
else ok "$name"; fi

# --- the page image reaches --inline-images ----------------------------------
name="bw: end both + --inline-images carries base64 PNG"
run_basm "require web
web attach bw
web goto \"$BTN\"
end both" --allow web --format json --inline-images
grep -qF '"image":"iVBOR' /tmp/koto-bw.log && ok "$name" \
  || bad "$name" "exit=$STATUS; no inline image in the json envelope"

# --- the browser survives the sidecar ----------------------------------------
name="bw: a killed sidecar does not lose the session"
# Sidecar death is cheap by design: the tab lives in BetterWright's per-profile
# session daemon, so the next koto process re-attaches to it.
pkill -f bw-sidecar >/dev/null 2>&1
run_basm "require web
web attach bw
web read
end text" --allow web
[[ $STATUS -eq 0 ]] && grep -qF '[ref=' <<<"$OUT" && ok "$name" \
  || bad "$name" "exit=$STATUS after the sidecar was killed"

echo
echo "passed=$pass failed=$fail"
[[ $fail -gt 0 ]] && { printf 'failed: %s\n' "${failed_names[*]}"; exit 1; }
exit 0
