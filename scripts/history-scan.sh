#!/usr/bin/env bash
# Full reachable history, including removed files and commit metadata.
# Output: representative commit, blob, path, line, category. NEVER matched values.
# Exit 0: completed (all candidates still need review); nonzero: incomplete/error.
set -euo pipefail
export LC_ALL=C
if [[ $(git rev-parse --is-shallow-repository) != false ]]; then
    echo 'history-scan: shallow history; fetch complete history first' >&2
    exit 2
fi
# A complete snapshot walk, deduplicated by blob. No diff/merge/rename omissions.
# Disable pathname quoting so tabs delimit the mode/object prefix and full path.
commits=$(git rev-list --all)
[[ -n $commits ]] || {
    echo 'history-scan: no commits' >&2
    exit 2
}
scan_text() {
    awk -v commit="$1" -v blob="$2" -v path="$3" '
    function emit(kind) { printf "%s\t%s\t%s\t%d\t%s\n",commit,blob,path,NR,kind }
    function entropy(s,   i,n,c,p,e,f) {
        e=0; n=length(s); for(i=1;i<=n;i++) f[substr(s,i,1)]++
        for(c in f) { p=f[c]/n; e-=p*log(p)/log(2) }; return e
    }
    {
        text=$0; lower=tolower(text)
        if (text ~ /sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,}|BEGIN [A-Z ]*PRIVATE KEY/)
            emit("credential-shape")
        if (lower ~ /(access_token|refresh_token|id_token|account_id|chatgpt-account-id|auth\.json)/)
            emit("auth-context")
        if (text ~ /\/home\/[A-Za-z0-9_.-]+\//) emit("local-path")
        # Remove only the permitted address, not the entire line (which could
        # contain a second, non-noreply email).
        email=text
        gsub(/[A-Za-z0-9._%+-]+@users\.noreply\.github\.com|noreply@github\.com/, "", email)
        if (email ~ /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/) emit("email")
        if (lower ~ /(api[_-]?key|password|secret|token)["[:space:]]*[:=]/) {
            emit("credential-context")
            # Candidate literals in key/value context. Include hex secrets:
            # hashes are triaged, not silently excluded. Shannon >=3.5 bits.
            n=split(text,parts,/[^A-Za-z0-9_+=\/.\-]/)
            for(i=1;i<=n;i++) if(length(parts[i])>=24 && entropy(parts[i])>=3.5) {
                emit("high-entropy-context"); break
            }
        }
        if (text ~ /"(response|conversation|chatcmpl)[._-][A-Za-z0-9]+"/ ||
            (path ~ /\.(sse|jsonl)$/ && lower ~ /"(messages|choices|output|content|delta)"/))
            emit("recording-context")
    }'
}
# Pipe failure propagates (pipefail). No raw matches are stored on disk or printed.
{
    while read -r commit; do
        printf 'COMMIT %s\n' "$commit"
        git -c core.quotePath=false ls-tree -r --full-tree "$commit"
    done <<<"$commits"
} | awk '
    /^COMMIT / {c=$2; next}
    {tab=index($0,"\t"); prefix=substr($0,1,tab-1); split(prefix,a," ");
     if(a[2]=="blob" && !seen[a[3]]++) print c "\t" a[3] "\t" substr($0,tab+1)}
' | while IFS=$'\t' read -r commit blob path; do
    git cat-file blob "$blob" | scan_text "$commit" "$blob" "$path"
done
while read -r commit; do
    git show -s --format='%an%n%ae%n%cn%n%ce%n%B' "$commit" |
        scan_text "$commit" - '<commit-metadata>'
done <<<"$commits"
printf 'history-scan: commits=%s; unique blobs scanned; candidate values withheld\n' \
    "$(printf '%s\n' "$commits" | wc -l | tr -d ' ')" >&2
# Successful completion means scan coverage, NOT credential clearance. All
# output locations need human triage. Exit 0 allows pipelines to collect them.
