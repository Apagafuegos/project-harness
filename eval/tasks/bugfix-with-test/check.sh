# harness-eval hidden check: bugfix-with-test
# Exits 0 only when the slug is cut to 16 characters, the long title is pinned by
# a test, and the whole suite passes.
set -eu

if ! sh test.sh; then
    printf 'test.sh failed\n'
    exit 1
fi

long=$(sh slug.sh 'Hello World Example Title')
if [ "$long" != "hello-world-exam" ]; then
    printf 'long title slug is %s, expected hello-world-exam\n' "$long"
    exit 1
fi

length=$(printf '%s' "$long" | wc -c)
if [ "$length" != 16 ]; then
    printf 'long title slug keeps %s characters, expected 16\n' "$length"
    exit 1
fi

if ! grep -q 'Hello World Example Title' test.sh; then
    printf 'test.sh has no long-title regression case\n'
    exit 1
fi

short=$(sh slug.sh 'Hello World')
if [ "$short" != "hello-world" ]; then
    printf 'short title slug regressed: %s\n' "$short"
    exit 1
fi

exit 0
