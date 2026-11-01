#!/bin/sh
# Test check.sh's control flow on a PATH with no bwrap. Cargo is a counted
# stand-in: the real integration tests separately prove missing-backend refusal.
set -eu
cd "$(dirname "$0")/.."
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
ln -s /usr/bin/dirname "$scratch/dirname"
ln -s /usr/bin/grep "$scratch/grep"
cat >"$scratch/cargo" <<'CARGO'
#!/bin/sh
if command -v bwrap >/dev/null 2>&1; then
    echo 'test setup accidentally exposes bwrap' >&2
    exit 78
fi
case " $* " in
    *" --test $SECURITY_TARGET "*)
        echo "required security target: $SECURITY_TARGET; missing bwrap is failure"
        exit 79
        ;;
esac
exit 0
CARGO
chmod +x "$scratch/cargo"
for SECURITY_TARGET in sandbox tool_host doctor offline_gate; do
    export SECURITY_TARGET
    status=0
    (unset HARNESS_ALLOW_NO_SANDBOX; PATH="$scratch"; export PATH; /bin/sh scripts/check.sh) >"$scratch/output" 2>&1 || status=$?
    if [ "$status" != 79 ]; then
        echo "security gate bypassed $SECURITY_TARGET on missing bwrap (exit $status)" >&2
        exit 1
    fi
    grep -q "required security target: $SECURITY_TARGET" "$scratch/output"
    if grep -q SKIPPED "$scratch/output"; then
        echo 'security gate silently skipped missing bwrap' >&2
        exit 1
    fi
done
echo 'security gate control-flow test: missing bwrap fails all required targets, never skips'
