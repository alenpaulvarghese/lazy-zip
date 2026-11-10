# Run everything CI runs for core and desktop.
ci: fmt lint test docs frontend

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test -p lazy-zip-core

# Fails on broken links and missing docs in the API reference.
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc -p lazy-zip-core --no-deps

frontend:
    cd desktop && npm run check && npm test

# Raises the version. Committing this to master is what triggers a release.
bump version:
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm Cargo.toml.bak
    cargo update --workspace

dev:
    cd desktop && npm run tauri dev
