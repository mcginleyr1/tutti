# Tutti build & install shortcuts. `just` with no args lists recipes.

default:
    @just --list

# Debug build of every crate
build:
    cargo build --workspace

# Dev cycle: kill every running daemon, then (re)install both binaries —
# so the next `tutti` always runs the code you just built.
install:
    -pkill -f tutti-server
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
