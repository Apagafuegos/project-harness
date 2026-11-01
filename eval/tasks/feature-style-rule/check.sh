# harness-eval hidden check: feature-style-rule
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
expected_files='./greet.sh'
if [ "$files" != "$expected_files" ]; then
    printf 'file set changed:\n%s\n' "$files" >&2
    exit 1
fi

json=$(sh greet.sh --json Alice; printf '\037')
json=${json%"$sentinel"}
expected_json='{"greeting":"Hello, Alice!"}
'
if [ "$json" != "$expected_json" ]; then
    printf 'json form is not byte-identical:\n%s\n' "$json" >&2
    exit 1
fi

plain=$(sh greet.sh Alice; printf '\037')
plain=${plain%"$sentinel"}
expected_plain='Hello, Alice!
'
if [ "$plain" != "$expected_plain" ]; then
    printf 'plain form is not byte-identical:\n%s\n' "$plain" >&2
    exit 1
fi

default=$(sh greet.sh; printf '\037')
default=${default%"$sentinel"}
expected_default='Hello, world!
'
if [ "$default" != "$expected_default" ]; then
    printf 'default form is not byte-identical:\n%s\n' "$default" >&2
    exit 1
fi

tab=$(printf '\t')
if grep -n "$tab" greet.sh >/dev/null 2>&1; then
    fail 'greet.sh contains a tab'
fi

if ! awk 'length > 80 { exit 1 }' greet.sh; then
    fail 'greet.sh has a line longer than 80 columns'
fi

if ! grep -q '^  [^ ]' greet.sh; then
    fail 'greet.sh has no two-space indentation'
fi

if grep -n '^   [^ ]' greet.sh >/dev/null 2>&1; then
    fail 'greet.sh indents with three or more spaces somewhere'
fi

exit 0
