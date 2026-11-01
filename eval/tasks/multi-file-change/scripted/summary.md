## Task and constraints
Rename clamp to clamp_range in lib/util.sh and fix every call site. Keep the
output of main.sh identical, and do not rename add in lib/math.sh — only clamp
changes, so lib/math.sh stays untouched.

## Decisions
Rename the definition in lib/util.sh and the call sites in main.sh. Leave
lib/math.sh alone.

## Files and actions
Read lib/util.sh, then wrote lib/util.sh with clamp_range and main.sh with the
first call site renamed. Captured `sh main.sh`: status 127 and stdout only `5 5 `,
because the second call site still calls clamp.

## Outcomes
lib/util.sh and main.sh are partially renamed. main.sh is broken until the
remaining clamp call in main.sh is updated.

## Unresolved work
Rename the last clamp call site in main.sh and re-run main.sh.
