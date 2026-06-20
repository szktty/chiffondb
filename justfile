# chiffondb task runner

# Default: list the available recipes
default:
    @just --list

# Build (debug)
build:
    cargo build

# Build (release)
release:
    cargo build --release

# Run all tests
test:
    cargo test

# Test only the core library
test-core:
    cargo test -p chiffondb-core

# Lint check
lint:
    cargo clippy -- -D warnings

# Format check
fmt-check:
    cargo fmt --check

# Apply formatting
fmt:
    cargo fmt

# Run lint + fmt-check together
check: lint fmt-check

# Run an example (e.g. just example basic)
example name:
    bash examples/{{name}}/run.sh

# Run all examples
examples-all:
    @for dir in examples/*/; do \
        name=$(basename "$dir"); \
        if [ -f "$dir/run.sh" ]; then \
            echo ">>> $name"; \
            bash "$dir/run.sh"; \
            echo ""; \
        fi; \
    done

# Install the chiffon command (add it to PATH)
install:
    cargo install --path chiffon
