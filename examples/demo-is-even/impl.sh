# is_even should return success (exit 0) when $1 is even, failure (1) when odd.
# Currently broken: it always reports "odd".
is_even() {
    if (( $1 % 2 == 0 )); then
        return 0
    else
        return 1
    fi
}
