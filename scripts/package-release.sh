#!/usr/bin/env bash
# GNU tar + gzip: reproducible packaging, with an independent archive self-check.
set -euo pipefail
export LC_ALL=C
if [[ $# != 4 ]]; then
    echo "usage: $0 VERSION TARGET BINARY OUTDIR" >&2
    exit 2
fi
version=$1 target=$2 binary=$3 outdir=$4
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?(\+[A-Za-z0-9.-]+)?$ ]] || {
    echo 'invalid version' >&2
    exit 2
}
case $target in
x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
*)
    echo 'unsupported release target' >&2
    exit 2
    ;;
esac
[[ ${SOURCE_DATE_EPOCH:-} =~ ^[0-9]+$ ]] || {
    echo 'SOURCE_DATE_EPOCH must be the commit timestamp (integer seconds)' >&2
    exit 2
}
[[ -f $binary && -x $binary ]] || {
    echo 'binary must be executable' >&2
    exit 2
}
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p -- "$outdir"
stage=$(mktemp -d "$outdir/.package.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
name=harness-$version-$target
mkdir -- "$stage/$name"
install -m 755 -- "$binary" "$stage/$name/harness"
for doc in LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md; do
    install -m 644 -- "$root/$doc" "$stage/$name/$doc"
done
# Dependency notices are generated once from locked sources by the release job.
notices=$root/target/third-party-notices
[[ -s $notices/INDEX.txt ]] || { echo 'generate target/third-party-notices first' >&2; exit 2; }
cp -R -- "$notices" "$stage/$name/third-party-notices"
find "$stage/$name/third-party-notices" -type f -exec chmod 644 {} +
if [[ -f $root/docs/user-guide.md ]]; then
    mkdir -- "$stage/$name/docs"
    install -m 644 -- "$root/docs/user-guide.md" "$stage/$name/docs/user-guide.md"
else
    echo 'note: docs/user-guide.md not present; skipping' >&2
fi
# Normalize all directory modes as well as file modes, independent of umask.
find "$stage/$name" -type d -exec chmod 755 {} +
tar --sort=name --format=gnu --mtime="@$SOURCE_DATE_EPOCH" \
    --owner=0 --group=0 --numeric-owner -C "$stage" -cf - "$name" |
    gzip -n -9 >"$stage/archive.tar.gz"
# Independent required-content manifest: omitting a copy above must fail here.
{
    find "$notices" -printf '%P\n' | while IFS= read -r rel; do
        if [[ -d $notices/$rel ]]; then
            printf '%s/\n' "$name/third-party-notices${rel:+/$rel}"
        else
            printf '%s\n' "$name/third-party-notices/$rel"
        fi
    done
    printf '%s\n' "$name/" "$name/harness" "$name/LICENSE-MIT" \
        "$name/LICENSE-APACHE" "$name/README.md" "$name/CHANGELOG.md"
    if [[ -f $root/docs/user-guide.md ]]; then
        printf '%s\n' "$name/docs/" "$name/docs/user-guide.md"
    fi
} | sort >"$stage/expected"
tar -tzf "$stage/archive.tar.gz" | sort >"$stage/actual"
if ! diff -u "$stage/expected" "$stage/actual"; then
    echo 'archive content self-check failed' >&2
    exit 1
fi
tar -xOf "$stage/archive.tar.gz" "$name/third-party-notices/INDEX.txt" | cmp - "$notices/INDEX.txt"
for doc in LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md; do
    tar -xOf "$stage/archive.tar.gz" "$name/$doc" | cmp - "$root/$doc"
done
tar -xOf "$stage/archive.tar.gz" "$name/harness" | cmp - "$binary"
if [[ -f $root/docs/user-guide.md ]]; then
    tar -xOf "$stage/archive.tar.gz" "$name/docs/user-guide.md" | cmp - "$root/docs/user-guide.md"
fi
mv -- "$stage/archive.tar.gz" "$outdir/$name.tar.gz"
printf '%s\n' "$outdir/$name.tar.gz"
