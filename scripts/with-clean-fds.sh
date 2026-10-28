#!/bin/bash
# CI launch hygiene, not a sandbox or a product fallback. The GitHub runner
# supplies an inheritable nonblocking pipe (observed as fd 142). Close such
# descriptors in this child only, before launching Cargo and its test binaries.
# Keep the backend's independent fail-closed descriptor audit unchanged.
set -euo pipefail

for entry in /proc/$$/fdinfo/*; do
    fd=${entry##*/}
    [[ $fd =~ ^[0-9]+$ ]] || exit 1
    ((fd > 2)) || continue
    [[ -r $entry ]] || continue
    flags=
    while read -r key value _; do
        if [[ $key == flags: ]]; then
            flags=$value
            break
        fi
    done < "$entry"
    if [[ -z $flags || $flags == *[!0-7]* ]]; then
        printf 'Cannot inspect inherited descriptor %s\n' "$fd" >&2
        exit 1
    fi
    if (((8#$flags & 8#2000000) == 0)); then
        # fd is kernel-supplied decimal digits, checked above; never model text.
        # Bash supports descriptor numbers above the POSIX shell's 0..9 range.
        eval "exec ${fd}>&-"
    fi
done

exec "$@"
