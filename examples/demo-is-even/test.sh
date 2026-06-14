. ./impl.sh
is_even 4 || { echo "FAIL: 4 should be even"; exit 1; }
if is_even 3; then echo "FAIL: 3 should be odd"; exit 1; fi
is_even 10 || { echo "FAIL: 10 should be even"; exit 1; }
echo "all tests passed"
