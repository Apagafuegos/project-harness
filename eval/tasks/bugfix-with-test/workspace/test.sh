#!/bin/sh
# Exercise slug.sh on two titles.
fail=0
check() {
    got=$(sh slug.sh "$1")
    if [ "$got" = "$2" ]; then
        printf 'ok: %s\n' "$1"
    else
        printf 'not ok: %s (got %s, want %s)\n' "$1" "$got" "$2"
        fail=1
    fi
}
check "Hello World" "hello-world"
check "Hello   World" "hello-world"
exit $fail
