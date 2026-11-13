# Tutti build & install shortcuts. `just` with no args lists recipes.

default:
    @just --list

# Debug build of every crate
build:
    cargo build --workspace

# Dev cycle: (re)install both binaries, so the next `tutti` runs the code you
# just built. Every running daemon is then killed, but only when the daemon's
# own binary came out different from the installed one: a change to the
# client alone leaves the sessions and their panes running. The kill comes
# last, so run from inside a tutti pane it takes the pane only once both
# binaries are in place.
install:
    #!/usr/bin/env bash
    set -euo pipefail
    # Build the daemon to one side and compare it with the one installed.
    cargo install --path crates/tutti-server --force --no-track --root target/staged
    installed="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/tutti-server"
    if cmp -s target/staged/bin/tutti-server "$installed"; then
        cargo install --path crates/tutti --force
        echo "tutti-server is unchanged: running daemons left alone"
    else
        cargo install --path crates/tutti-server --force
        cargo install --path crates/tutti --force
        echo "tutti-server changed: stopping every running daemon"
        pkill -f tutti-server || true
    fi

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
