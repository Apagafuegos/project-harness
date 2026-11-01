# harness-eval hidden check: refactor-constraint
# Run from the workspace root inside the sandbox. Exits 0 only for a correct
# refactor: one format string, one emit helper, three calls, same output.
set -eu

actual=$(sh report.sh)
expected='name         value
alpha        1
beta         2'
if [ "$actual" != "$expected" ]; then
    printf 'output changed:\n%s\n' "$actual"
    exit 1
fi

format_lines=$(grep -c "printf '%-12s %s" report.sh)
if [ "$format_lines" != 1 ]; then
    printf 'expected exactly one copy of the row format, found %s\n' "$format_lines"
    exit 1
fi

if ! grep -q '^emit() {' report.sh; then
    printf 'missing emit() helper\n'
    exit 1
fi

calls=$(grep -c '^emit ' report.sh)
if [ "$calls" != 3 ]; then
    printf 'expected three emit calls, found %s\n' "$calls"
    exit 1
fi

if [ -e report2.sh ] || [ -e emit.sh ]; then
    printf 'the refactor must not add files\n'
    exit 1
fi

exit 0
