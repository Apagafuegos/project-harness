#!/usr/bin/env bash
# Pure git/awk regression; fake markers only. No credentials or network.
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
scanner=$PWD/scripts/history-scan.sh
mkdir -p target
scratch=$(mktemp -d "$PWD/target/history-test.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
repo=$scratch/repo
mkdir "$repo"
cd "$repo"
git init -q --initial-branch=main
git config user.name 'Audit fixture'
git config user.email audit-fixture@users.noreply.github.com
# Build a recognizable fake marker, never a real credential.
marker=$(printf '%s%s' sk- SENTINEL-CREDENTIAL-NOT-A-REAL-KEY)
printf 'api_key = "%s"\n' "$marker" > removed.txt
printf '/home/example-user/work\naccess_token: fixture\n' > context.txt
git add .; git commit -qm 'synthetic initial fixture'
first=$(git rev-parse HEAD)
blob=$(git rev-parse HEAD:removed.txt)
git rm -q removed.txt; git commit -qm 'remove synthetic marker'
# A secret on another branch must not disappear from the scan of main.
git checkout -qb side "$first"
printf 'token = "SENTINEL-AbCd0123456789-FAKE-ONLY"\n' > side.txt
git add .; git commit -qm 'synthetic side fixture'
git checkout -q main
"$scanner" > "$scratch/hits" 2> "$scratch/summary"
grep -F "$blob" "$scratch/hits" | grep -F removed.txt | grep -q credential-shape
grep -F side.txt "$scratch/hits" | grep -q high-entropy-context
grep -F context.txt "$scratch/hits" | grep -q auth-context
grep -F context.txt "$scratch/hits" | grep -q local-path
! grep -Fq "$marker" "$scratch/hits"
! grep -Fq SENTINEL "$scratch/hits"
! grep -q $'\temail$' "$scratch/hits"
git clone -q --depth 1 "file://$repo" "$scratch/shallow"
cd "$scratch/shallow"
if "$scanner" > "$scratch/shallow-hits" 2>/dev/null; then
    echo 'shallow history was accepted' >&2; exit 1
fi
printf 'history scanner tests passed (deleted blobs, all refs, redacted output, shallow refusal)\n'
