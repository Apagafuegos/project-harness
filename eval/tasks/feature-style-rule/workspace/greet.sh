#!/bin/sh
# Print a greeting.
name=${1:-world}
printf 'Hello, %s!\n' "$name"
