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
    cargo build --release --locked

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

# Install cubrid-ci from this checkout.
install:
    cargo install --path . --locked

# Remove Cargo build artifacts.
clean:
    cargo clean
