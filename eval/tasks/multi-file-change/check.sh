# harness-eval hidden check: multi-file-change
# Run from the workspace root inside the sandbox. Exits 0 only for a correct
# solution. Trusted bytes, the allowed file set and byte-exact output are all
# compared literally; the temporary hidden-check carrier is the only entry the
# file set ignores.
set -eu

sentinel=$(printf '\037')
fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

# The full file set, excluding only this check's carrier.
files=$(find . -type f ! -name '.harness-eval-check.sh' -print | LC_ALL=C sort)
expected_files='./lib/math.sh
./lib/util.sh
./main.sh'
if [ "$files" != "$expected_files" ]; then
    printf 'file set changed:\n%s\n' "$files" >&2
    exit 1
fi

# lib/math.sh is protected: compare trusted bytes, including the final newline.
trusted_math=$(cat <<'ORIGINAL'
#!/bin/sh
# Small arithmetic helpers.
add() {
    printf '%s\n' "$(( $1 + $2 ))"
}
ORIGINAL
printf '\037')
trusted_math=${trusted_math%"$sentinel"}
current_math=$(cat lib/math.sh; printf '\037')
current_math=${current_math%"$sentinel"}
if [ "$current_math" != "$trusted_math" ]; then
    fail 'lib/math.sh was modified; only clamp may change'
fi

out=$(sh main.sh; printf '\037')
out=${out%"$sentinel"}
expected='5 5 0
'
if [ "$out" != "$expected" ]; then
    printf 'main.sh printed:\n%s\n' "$out" >&2
    exit 1
fi

if ! grep -q '^clamp_range() {' lib/util.sh; then
    fail 'lib/util.sh has no clamp_range definition'
fi

if grep -rEn '(^|[^_[:alnum:]])clamp([^_[:alnum:]]|$)' lib main.sh >/dev/null 2>&1; then
    fail 'a bare clamp reference remains'
fi

if ! grep -q '^add() {' lib/math.sh; then
    fail 'lib/math.sh no longer defines add'
fi

sum=$(sh -c '. ./lib/math.sh; add 7 8')
if [ "$sum" != "15" ]; then
    fail "add is broken: $sum"
fi

bounded=$(sh -c '. ./lib/util.sh; clamp_range 10 0 5')
if [ "$bounded" != "5" ]; then
    fail "clamp_range is broken: $bounded"
fi

exit 0
