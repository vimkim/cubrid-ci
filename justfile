set shell := ["bash", "-cu"]

# List the available recipes.
default:
    @just --list

# Check the project without producing a runnable binary.
check:
    cargo check --all-targets

# Build the debug binary.
build:
    cargo build

# Build the optimized release binary.
release:
    ./scripts/build-release.sh

# Build the release twice in independent clones and compare binary hashes.
verify-release-reproducible:
    ./scripts/verify-release-reproducibility.sh

# Run cubrid-ci, passing any remaining arguments to the CLI.
run *args:
    cargo run -- {{args}}

# Run all tests.
test:
    cargo test --all-targets

# Format Rust source files.
fmt:
    cargo fmt --all

# Run Clippy with warnings treated as errors.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the full local verification suite.
verify:
    cargo fmt --all -- --check
    cargo check --all-targets
    cargo test --all-targets
    cargo clippy --all-targets --all-features -- -D warnings

alias sync := install

# Install cubrid-ci and bundled skills from this checkout.
install:
    cargo install --path . --locked
    "{{just_executable()}}" install-skills

# Install bundled skills (copy to ~/.agents/skills and link for Claude, Codex, and Antigravity).
install-skills:
    #!/usr/bin/env bash
    set -euo pipefail
    skill_root="{{justfile_directory()}}/skills"
    agents_dir="$HOME/.agents/skills"
    claude_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills"
    codex_dir="${CODEX_HOME:-$HOME/.codex}/skills"
    agy_dir="$HOME/.gemini/config/skills"
    mkdir -p "$agents_dir" "$claude_dir" "$codex_dir" "$agy_dir"

    link_agent() {
        local target="$1"
        local dir="$2"
        local name="$3"
        local link_path="$dir/$name"
        if [ -L "$link_path" ] || [ -e "$link_path" ]; then
            rm -rf "${dir:?}/${name:?}"
        fi
        ln -s "$target" "$link_path"
    }

    for src in "$skill_root"/*; do
        [ -d "$src" ] && [ -f "$src/SKILL.md" ] || continue
        name="$(basename "$src")"
        staging="$(mktemp -d "${agents_dir}/.${name}.XXXXXX")"
        cp -R "$src/." "$staging/"
        find "$staging" -type f -name '*.pyc' -delete
        find "$staging" -type d -name '__pycache__' -empty -delete
        if [ -L "$agents_dir/$name" ] || [ -e "$agents_dir/$name" ]; then
            rm -rf "${agents_dir:?}/${name:?}"
        fi
        mv "$staging" "$agents_dir/$name"
        if [ "$claude_dir" = "$HOME/.claude/skills" ]; then
            link_agent "../../.agents/skills/$name" "$claude_dir" "$name"
        else
            link_agent "$agents_dir/$name" "$claude_dir" "$name"
        fi
        link_agent "$agents_dir/$name" "$codex_dir" "$name"
        link_agent "$agents_dir/$name" "$agy_dir" "$name"
        printf 'Installed skill %s -> %s\n' "$name" "$agents_dir/$name"
    done

# Remove Cargo build artifacts.
clean:
    cargo clean
