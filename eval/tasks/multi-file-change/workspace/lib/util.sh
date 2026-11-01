#!/bin/sh
# Small range helpers.
clamp() {
    value=$1
    low=$2
    high=$3
    if [ "$value" -lt "$low" ]; then
        value=$low
    elif [ "$value" -gt "$high" ]; then
        value=$high
    fi
    printf '%s\n' "$value"
}
