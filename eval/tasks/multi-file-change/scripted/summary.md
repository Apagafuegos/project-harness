## Task and constraints
Rename clamp to clamp_range in lib/util.sh and fix every call site. Keep the
output of main.sh identical, and do not rename add in lib/math.sh — only clamp
changes, so lib/math.sh stays untouched.

## Decisions
Rename the definition in lib/util.sh and the single call site in main.sh. Leave
lib/math.sh alone.

## Files and actions
Read lib/util.sh. Ran `grep -rn clamp lib main.sh`, which found the definition and
one call in main.sh. Ran `sh main.sh`, which printed 5 5.

## Outcomes
Scope confirmed: lib/util.sh and main.sh. The expected output is 5 5.

## Unresolved work
Rename clamp to clamp_range in lib/util.sh and main.sh, then re-run main.sh.
