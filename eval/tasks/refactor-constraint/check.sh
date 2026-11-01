# harness-eval hidden check: refactor-constraint
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
expected_files='./report.sh'
if [ "$files" != "$expected_files" ]; then
    printf 'file set changed:\n%s\n' "$files" >&2
    exit 1
fi

# Byte-exact output, including the final newline: command substitution would eat
# it, so a sentinel is appended and stripped instead.
actual=$(sh report.sh; printf '\037')
actual=${actual%"$sentinel"}
expected='name         value
alpha        1
beta         2
'
if [ "$actual" != "$expected" ]; then
    printf 'output is not byte-identical:\n%s\n' "$actual" >&2
    exit 1
fi

format_lines=$(grep -c "printf '%-12s %s" report.sh)
if [ "$format_lines" != 1 ]; then
    fail "expected exactly one copy of the row format, found $format_lines"
fi

if ! grep -q '^emit() {' report.sh; then
    fail 'missing emit() helper'
fi

calls=$(grep -c '^emit ' report.sh)
if [ "$calls" != 3 ]; then
    fail "expected three emit calls, found $calls"
fi

exit 0
