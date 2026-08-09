# rsshogi-csa workspace tasks

set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

default:
    @just --list

# Build all workspace crates
build:
    cargo build --workspace

# Build release binaries
release:
    cargo build --workspace --release

# Run all tests
test:
    cargo test --workspace

# Run tests for the protocol codec only
test-csa:
    cargo test -p rsshogi-csa

# Check without building
check:
    cargo check --workspace

# Format only
fmt:
    cargo fmt --all

# Clippy only
clippy:
    cargo clippy --workspace --all-targets --all-features -- \
        -W clippy::pedantic -W clippy::nursery -W clippy::cargo \
        -A clippy::module_name_repetitions -A clippy::missing_panics_doc \
        -A clippy::missing_errors_doc \
        -D warnings

# Lint and format
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- \
        -W clippy::pedantic -W clippy::nursery -W clippy::cargo \
        -A clippy::module_name_repetitions \
        -A clippy::missing_panics_doc \
        -A clippy::missing_errors_doc \
        -D warnings

# Build Rustdoc
docs:
    cargo doc --no-deps --workspace

