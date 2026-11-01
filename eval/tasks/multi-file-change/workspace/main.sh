#!/bin/sh
# Print a sum and a clamped value.
. ./lib/math.sh
. ./lib/util.sh
sum=$(add 2 3)
bounded=$(clamp 10 0 5)
printf '%s %s\n' "$sum" "$bounded"
