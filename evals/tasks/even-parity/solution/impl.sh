# is_even N -> exit status 0 if N is even, non-zero otherwise.
is_even() {
    [ $(( $1 % 2 )) -eq 0 ]
}
