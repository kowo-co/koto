#!/usr/bin/env bash
# Does koto's exit code match its documented contract? A blind agent can only
# trust the tool if failure is always non-zero and always the right code.
#   0 ok | 1 assert | 2 timeout | 3 no match | 4 budget
#   5 denied | 6 ambiguous | 7 no observation | 8 parse | 9 backend
cd /home/jason/projects/koto || exit 1
K=./target/release/koto
pass=0; fail=0

check() { # name expected command...
  local name="$1" expected="$2"; shift 2
  local out; out=$("$@" 2>&1 </dev/null); local got=$?
  if [[ "$got" == "$expected" ]]; then
    printf '  ok    %-34s exit=%s\n' "$name" "$got"; pass=$((pass+1))
  else
    printf '  FAIL  %-34s expected=%s got=%s\n' "$name" "$expected" "$got"
    printf '        %s\n' "$(echo "$out" | tail -1)"; fail=$((fail+1))
  fi
}

echo "exit-code contract:"
check "success"            0 $K --allow window list windows end silent
check "selector matches nothing" 3 $K --allow window focus 'title~ZZ_NO_SUCH_WINDOW_ZZ' end silent
check "capability denied"  5 $K --allow window pane run "echo hi" end silent
check "parse error"        8 $K --allow window this-is-not-a-mnemonic
check "budget exceeded"    4 $K --allow window --budget-ops 2 list windows list windows list windows end silent
check "assertion failed"   1 $K --allow window assert text contains ZZ_NEVER_PRESENT_ZZ

echo
echo "exec cannot be bypassed (deny wins over an explicit allow):"
# `pane send "cmd\n"` executes exactly what `pane run "cmd"` does, and `pane new`
# starts a shell. Gating only `run` left an agent denied `exec` able to run
# anything it liked, which an eval found by heredoc'ing a whole file through it.
printf 'require input\npane send "echo pwned\\n"\nend silent\n' > /tmp/koto-bypass.basm
printf 'require input\npane new\nend silent\n' > /tmp/koto-bypass-new.basm
check "pane send needs exec"  5 $K --deny exec --allow exec,input --script /tmp/koto-bypass.basm
check "pane new needs exec"   5 $K --deny exec --allow exec,input --script /tmp/koto-bypass-new.basm
check "pane read cannot start a shell" 9 $K --deny exec --allow exec,input pane read 5

echo
echo "web verbs answer from the gate, not from a browser (none of these attach):"
# koto's own stdout/stderr must land in a file, never a pipe: `web attach` can
# spawn a browser or a node sidecar that inherits the descriptor and holds it
# open long after koto exits. `3>&- 4>&-` is the point of the selector case —
# with no CDP pipe prepared koto must refuse, not grab whatever it finds there.
checkw() { # name expected timeout command...
  local name="$1" expected="$2" secs="$3"; shift 3
  timeout "$secs" "$@" >/tmp/koto-exit-web.log 2>&1 </dev/null 3>&- 4>&-
  local got=$?
  if [[ "$got" == "$expected" ]]; then
    printf '  ok    %-34s exit=%s\n' "$name" "$got"; pass=$((pass+1))
  else
    printf '  FAIL  %-34s expected=%s got=%s\n' "$name" "$expected" "$got"
    printf '        %s\n' "$(tail -1 /tmp/koto-exit-web.log)"; fail=$((fail+1))
  fi
}
# `web read` would exit 9 on its own; 5 proves require ran before instruction 0.
printf 'require web.download\nweb read\nend silent\n' > /tmp/koto-web-require.basm
checkw "web login needs web.login"      5 10 $K --allow web web login example.com
checkw "require web.download preflight" 5 10 $K --allow web --script /tmp/koto-web-require.basm
checkw "web shot without an engine"     9 10 $K --allow web web shot
checkw "web attach selector, no fds"    9 10 $K --allow web web attach 'title~ZZ_NO_SUCH_WINDOW_ZZ'
# This one boots node to discover the module is missing, so give it room.
checkw "web attach bw, no betterwright" 9 60 $K --allow web web attach bw
checkw "web attach bw rejects junk"     8 10 $K --allow web web attach bw bogus=1

echo
echo "default-capability behaviour (--allow grants, never restricts):"
check "type is allowed by default"   0 $K --allow window type "" end silent

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]]
