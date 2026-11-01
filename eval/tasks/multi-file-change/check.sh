# harness-eval hidden check: multi-file-change
# Exits 0 only when clamp exists exactly as clamp_range, no old call remains,
# lib/math.sh is untouched and main.sh prints the same bytes.
set -eu

out=$(sh main.sh)
if [ "$out" != "5 5" ]; then
    printf 'main.sh printed %s\n' "$out"
    exit 1
fi

if ! grep -q '^clamp_range() {' lib/util.sh; then
    printf 'lib/util.sh has no clamp_range definition\n'
    exit 1
fi

if grep -rEn '(^|[^_[:alnum:]])clamp([^_[:alnum:]]|$)' lib main.sh >/dev/null 2>&1; then
    printf 'a bare clamp reference remains\n'
    exit 1
fi

if ! grep -q '^add() {' lib/math.sh; then
    printf 'lib/math.sh no longer defines add\n'
    exit 1
fi

sum=$(sh -c '. ./lib/math.sh; add 7 8')
if [ "$sum" != "15" ]; then
    printf 'add is broken: %s\n' "$sum"
    exit 1
fi

bounded=$(sh -c '. ./lib/util.sh; clamp_range 10 0 5')
if [ "$bounded" != "5" ]; then
    printf 'clamp_range is broken: %s\n' "$bounded"
    exit 1
fi

exit 0
