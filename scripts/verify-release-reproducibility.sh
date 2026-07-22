#!/usr/bin/env bash
set -euo pipefail

readonly script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly repo_root=$(cd -- "$script_dir/.." && pwd -P)
readonly release_target=x86_64-unknown-linux-gnu

git_sha=$("$script_dir/check-release-source.sh" "$repo_root")
temp_root=$(mktemp -d)
trap 'rm -rf -- "$temp_root"' EXIT

for clone_name in first second; do
    git clone --quiet --no-hardlinks "$repo_root" "$temp_root/$clone_name"
    git -C "$temp_root/$clone_name" checkout --quiet --detach "$git_sha"
    "$temp_root/$clone_name/scripts/build-release.sh"
done

first_binary="$temp_root/first/target/$release_target/release/cubrid-ci"
second_binary="$temp_root/second/target/$release_target/release/cubrid-ci"
first_hash=$(sha256sum "$first_binary" | awk '{print $1}')
second_hash=$(sha256sum "$second_binary" | awk '{print $1}')

if [[ $first_hash != "$second_hash" ]]; then
    echo "error: release binaries are not reproducible" >&2
    echo "first:  $first_hash" >&2
    echo "second: $second_hash" >&2
    exit 1
fi

printf 'reproducible release sha256: %s\n' "$first_hash"
