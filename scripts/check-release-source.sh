#!/usr/bin/env bash
set -euo pipefail

source_dir=${1:-.}

if ! repo_root=$(git -C "$source_dir" rev-parse --show-toplevel 2>/dev/null); then
    echo "error: release builds require a Git working tree" >&2
    exit 1
fi

if ! git_sha=$(git -C "$repo_root" rev-parse --verify 'HEAD^{commit}' 2>/dev/null); then
    echo "error: release builds require a valid Git HEAD commit" >&2
    exit 1
fi

if [[ -n ${CUBRID_CI_BUILD_GIT_SHA:-} && ${CUBRID_CI_BUILD_GIT_SHA,,} != ${git_sha,,} ]]; then
    echo "error: CUBRID_CI_BUILD_GIT_SHA ($CUBRID_CI_BUILD_GIT_SHA) must exactly match release HEAD ($git_sha)" >&2
    exit 1
fi

if ! status=$(git -C "$repo_root" status \
    --porcelain=v1 \
    --untracked-files=all \
    --ignore-submodules=none); then
    echo "error: failed to inspect the Git working tree" >&2
    exit 1
fi

if [[ -n $status ]]; then
    echo "error: release builds require a clean Git working tree:" >&2
    printf '%s\n' "$status" >&2
    exit 1
fi

printf '%s\n' "$git_sha"
