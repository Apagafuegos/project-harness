#!/bin/sh
# Small arithmetic helpers.
add() {
    printf '%s\n' "$(( $1 + $2 ))"
}
