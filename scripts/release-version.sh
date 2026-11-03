#!/usr/bin/env bash
# Shared by release.yml and its offline regression test. No compilation or fetch.
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
version=$(awk '
    /^\[workspace.package\]$/ { package = 1; next }
    package && /^\[/ { exit }
    package && /^version = / { gsub(/"/, "", $3); print $3; exit }
' Cargo.toml)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?(\+[A-Za-z0-9.-]+)?$ ]] || {
    echo 'could not read workspace release version' >&2
    exit 1
}
if [[ ${RELEASE_REF_TYPE:?} == tag && ${RELEASE_REF_NAME:?} != "v$version" ]]; then
    echo "tag $RELEASE_REF_NAME does not match workspace version v$version" >&2
    exit 1
fi
printf 'version=%s\nepoch=%s\n' "$version" "$(git show -s --format=%ct HEAD)" >>"${GITHUB_OUTPUT:?}"
