#!/usr/bin/env bash
# Build-time only: locked registry sources must already be fetched. No network.
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
[[ $# == 1 ]] || { echo 'usage: third-party-notices.sh OUTDIR' >&2; exit 2; }
out=$1
[[ ! -e $out ]] || { echo 'notice output must be new' >&2; exit 2; }
mkdir -p "$(dirname -- "$out")"
mkdir "$out"
trap 'rm -rf -- "$out"' ERR
# Include all locked platform/optional dependencies, a conservative superset of
# either shipped binary. No dependency source or metadata path is published.
cargo metadata --locked --offline --format-version=1 | jq -r '
  .packages | sort_by(.name,.version)[] | select(.source != null) |
  [.name,.version,.license,(.manifest_path | sub("/Cargo.toml$";""))] | @tsv' |
while IFS=$'\t' read -r name version license root; do
    dest="$out/$name-$version"
    mkdir -p "$dest"
    printf '%s %s — %s\nSource (unmodified): https://crates.io/api/v1/crates/%s/%s/download\n\n' \
        "$name" "$version" "$license" "$name" "$version" >> "$out/INDEX.txt"
    if [[ $license == MPL-2.0* ]]; then
        cp -- docs/security/licenses/MPL-2.0.txt "$dest/LICENSE-MPL-2.0"
    fi
    found=0
    while IFS= read -r -d '' file; do
        rel=${file#"$root/"}
        mkdir -p "$dest/$(dirname -- "$rel")"
        cp -- "$file" "$dest/$rel"
        found=1
    done < <(find "$root" -maxdepth 2 -type f \( -iname 'license*' -o -iname 'copying*' -o -iname 'notice*' -o -iname 'copyright*' \) -print0 | sort -z)
    # Some crates put their complete notice only in README. Preserve it too.
    if [[ $found == 0 ]]; then
        while IFS= read -r -d '' file; do cp -- "$file" "$dest/"; found=1; done \
            < <(find "$root" -maxdepth 1 -type f -iname 'readme*' -print0 | sort -z)
    fi
    # jsonschema-regex 0.46.10 omits its repository licence in the crate
    # archive; its same-version sibling publishes that shared MIT notice.
    if [[ $found == 0 && $name == jsonschema-regex && $version == 0.46.10 ]]; then
        cp -- "$(dirname -- "$root")/jsonschema-$version/LICENSE" "$dest/LICENSE"
        found=1
    fi
    if [[ $found == 0 && $version == 0.4.0 && ( $name == winapi-i686-pc-windows-gnu || $name == winapi-x86_64-pc-windows-gnu ) ]]; then
        cp -- "$(dirname -- "$root")/winapi-0.3.9/LICENSE-APACHE" "$dest/"
        cp -- "$(dirname -- "$root")/winapi-0.3.9/LICENSE-MIT" "$dest/"
        found=1
    fi
    [[ $found == 1 ]] || { echo "no licence/notice text: $name $version" >&2; exit 1; }
done
[[ -s $out/INDEX.txt ]] || { echo 'empty dependency notices' >&2; exit 1; }
printf '%s\n' 'Dependency copyrights and licences are retained in the named subdirectories.' \
    'MPL-2.0 components imbl and imbl-sized-chunks are unmodified; corresponding' \
    'source is available at the version-specific download links above. Their' \
    'file-level licence does not relicense project-harness under MPL.' >> "$out/INDEX.txt"
