default:
    @just --list

# Build the release binary.
build:
    cargo build --release

# Run the tool with the given arguments.
run *args:
    cargo run --release -- {{args}}

# Run the test suite.
test:
    cargo test

# Built-in end-to-end self-test (default target: current directory).
smoke target=".":
    cargo run --release -- --path {{target}} selftest

# One command: lint, unit tests and end-to-end smoke.
verify: lint test smoke

# Run formatting and lint checks.
lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings

# Format source files.
fmt:
    nix fmt
    cargo fmt

# Enter the Nix development shell.
develop:
    nix develop

# Check the Nix flake.
check:
    nix flake check

# Update flake inputs.
update:
    nix flake update

# Show flake outputs.
show:
    nix flake show
