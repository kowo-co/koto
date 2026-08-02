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
echo "default-capability behaviour (--allow grants, never restricts):"
check "type is allowed by default"   0 $K --allow window type "" end silent

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]]
