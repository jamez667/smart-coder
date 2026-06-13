# Contract test for is_even. FROZEN: a solver must not modify this file.
. ./impl.sh

fail=0
is_even 4 || { echo "FAIL: 4 should be even"; fail=1; }
is_even 0 || { echo "FAIL: 0 should be even"; fail=1; }
if is_even 3; then echo "FAIL: 3 should be odd"; fail=1; fi
if is_even 7; then echo "FAIL: 7 should be odd"; fail=1; fi

[ "$fail" -eq 0 ] && echo "ok"
exit "$fail"
