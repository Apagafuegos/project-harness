# harness-eval hidden check: bugfix-with-test
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
expected_files='./slug.sh
./test.sh'
if [ "$files" != "$expected_files" ]; then
    printf 'file set changed:\n%s\n' "$files" >&2
    exit 1
fi

if ! sh test.sh; then
    fail 'test.sh failed'
fi

long=$(sh slug.sh 'Hello World Example Title')
if [ "$long" != "hello-world-exam" ]; then
    fail "long title slug is $long, expected hello-world-exam"
fi

length=$(printf '%s' "$long" | wc -c)
if [ "$length" != 16 ]; then
    fail "long title slug keeps $length characters, expected 16"
fi

if ! grep -q 'Hello World Example Title' test.sh; then
    fail 'test.sh has no long-title regression case'
fi

short=$(sh slug.sh 'Hello World')
if [ "$short" != "hello-world" ]; then
    fail "short title slug regressed: $short"
fi

exit 0
