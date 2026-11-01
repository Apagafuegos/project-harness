#!/bin/sh
# Print a slug for each argument.
slugify() {
    printf '%s' "$1" | tr 'A-Z' 'a-z' | tr -cs 'a-z0-9' '-' | cut -c1-15
}

for title in "$@"; do
    slugify "$title"
    printf '\n'
done
