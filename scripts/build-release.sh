#!/usr/bin/env bash
set -euo pipefail

readonly script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly repo_root=$(cd -- "$script_dir/.." && pwd -P)
readonly release_target=x86_64-unknown-linux-gnu
readonly required_rust_version=1.89.0
readonly canonical_source_path=/usr/src/cubrid-ci
readonly binary_path="$repo_root/target/$release_target/release/cubrid-ci"
readonly receipt_path="$binary_path.build-info"

git_sha=$("$script_dir/check-release-source.sh" "$repo_root")
source_date_epoch=$(git -C "$repo_root" show -s --format=%ct "$git_sha")
rust_version=$(rustc --version | awk '{print $2}')
rust_host=$(rustc --version --verbose | awk '$1 == "host:" {print $2}')

if [[ $rust_version != "$required_rust_version" ]]; then
    echo "error: release builds require rustc $required_rust_version (found $rust_version)" >&2
    exit 1
fi
if [[ $rust_host != "$release_target" ]]; then
    echo "error: release builds require host $release_target (found $rust_host)" >&2
    exit 1
fi

export CUBRID_CI_BUILD_GIT_SHA=$git_sha
export SOURCE_DATE_EPOCH=$source_date_epoch
export CARGO_INCREMENTAL=0
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$repo_root=$canonical_source_path"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=false
export CARGO_PROFILE_RELEASE_INCREMENTAL=false
export CARGO_PROFILE_RELEASE_LTO=thin
export CARGO_PROFILE_RELEASE_OPT_LEVEL=3
export CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=false
export CARGO_PROFILE_RELEASE_PANIC=unwind
export CARGO_PROFILE_RELEASE_STRIP=symbols
unset RUSTFLAGS

# A stale receipt must never attest to a build that has not passed this run's checks.
rm -f -- "$receipt_path"

cargo build \
    --manifest-path "$repo_root/Cargo.toml" \
    --release \
    --locked \
    --target "$release_target"

# Reject a source change that raced with compilation.
post_build_sha=$("$script_dir/check-release-source.sh" "$repo_root")
if [[ $post_build_sha != "$git_sha" ]]; then
    echo "error: Git HEAD changed during the release build" >&2
    exit 1
fi

binary_sha256=$(sha256sum "$binary_path" | awk '{print $1}')
binary_version=$("$binary_path" --version)
receipt_tmp=$(mktemp "$receipt_path.tmp.XXXXXX")
trap 'rm -f -- "$receipt_tmp"' EXIT

printf '%s\n' \
    "git_sha=$git_sha" \
    "source_date_epoch=$source_date_epoch" \
    "rustc_version=$(rustc --version)" \
    "cargo_version=$(cargo --version)" \
    "target=$release_target" \
    "binary_sha256=$binary_sha256" \
    "binary_version=$binary_version" \
    >"$receipt_tmp"
mv -- "$receipt_tmp" "$receipt_path"
trap - EXIT

printf 'release binary: %s\n' "$binary_path"
printf 'build receipt:  %s\n' "$receipt_path"
printf 'version:        %s\n' "$binary_version"
printf 'sha256:         %s\n' "$binary_sha256"
