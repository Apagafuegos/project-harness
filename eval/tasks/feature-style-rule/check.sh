# harness-eval hidden check: feature-style-rule
# Exits 0 only when the flag behaves, the plain path is unchanged and the style
# rule holds: no tabs, 80 columns, two-space indentation.
set -eu

json=$(sh greet.sh --json Alice)
if [ "$json" != '{"greeting":"Hello, Alice!"}' ]; then
    printf 'json form is %s\n' "$json"
    exit 1
fi

plain=$(sh greet.sh Alice)
if [ "$plain" != "Hello, Alice!" ]; then
    printf 'plain form is %s\n' "$plain"
    exit 1
fi

default=$(sh greet.sh)
if [ "$default" != "Hello, world!" ]; then
    printf 'default form is %s\n' "$default"
    exit 1
fi

if grep -n '	' greet.sh >/dev/null 2>&1; then
    printf 'greet.sh contains a tab\n'
    exit 1
fi

if ! awk 'length > 80 { exit 1 }' greet.sh; then
    printf 'greet.sh has a line longer than 80 columns\n'
    exit 1
fi

if ! grep -q '^  [^ ]' greet.sh; then
    printf 'greet.sh has no two-space indentation\n'
    exit 1
fi

if grep -n '^   [^ ]' greet.sh >/dev/null 2>&1; then
    printf 'greet.sh indents with three or more spaces somewhere\n'
    exit 1
fi

exit 0
