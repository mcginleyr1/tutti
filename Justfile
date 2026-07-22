# Tutti build & install shortcuts. `just` with no args lists recipes.

default:
    @just --list

# Debug build of every crate
build:
    cargo build --workspace

# Build and (re)install both binaries into ~/.cargo/bin
install:
    cargo install --path crates/tutti --force
    cargo install --path crates/tutti-server --force

# Run every test in the workspace
test:
    cargo test --workspace

# The full merge gate: check, test, clippy -D warnings, fmt --check
verify:
    cargo check --workspace --all-targets
    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --check

fmt:
    cargo fmt
