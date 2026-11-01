#!/bin/sh
# Print a sum and two clamped values.
. ./lib/math.sh
. ./lib/util.sh
sum=$(add 2 3)
bounded=$(clamp 10 0 5)
floor=$(clamp -3 0 5)
printf '%s %s %s\n' "$sum" "$bounded" "$floor"
