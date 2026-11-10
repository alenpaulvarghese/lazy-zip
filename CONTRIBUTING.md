# Contributing

## Setup

You need Rust, Node.js 22 (see `.nvmrc`), the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your platform, and optionally [`just`](https://github.com/casey/just).

```bash
cd desktop && npm ci
```

## Checks

`just ci` runs everything below:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p lazy-zip-core
RUSTDOCFLAGS="-D warnings" cargo doc -p lazy-zip-core --no-deps
cd desktop && npm run check && npm test
```

`cargo test -p desktop` needs the frontend built first with `npm run build`. In PowerShell, set
the docs flag with `$env:RUSTDOCFLAGS="-D warnings"` first.

## Documentation

- [`core/README.md`](core/README.md) is the guide for developers using the crate. Its Rust
  examples are compiled by `cargo test -p lazy-zip-core`, so keep them building, and keep the
  `no_run` marker on any that touch the network.
- Every public item in `core` needs a doc comment (`#![warn(missing_docs)]`), and the docs check
  above fails on broken links.
- [`docs/architecture.md`](docs/architecture.md) describes the internals. Update it when the
  request flow or a defence moves.

## Python bindings

The bindings in `python/` wrap `lazy-zip-core` with [maturin](https://www.maturin.rs/). To build
and test them in a virtual environment (`python/.venv` is ignored by Git):

```bash
cd python
python -m venv .venv
source .venv/bin/activate      # Windows: .venv\Scripts\activate
pip install maturin pytest
maturin develop
pytest tests
```

Run `maturin develop` again after changing Rust code. `cargo clippy -p lazy-zip-py` lints just the
bindings; `just lint` covers them too.

## Tests

- `core/tests/support/` has a local HTTP server that misbehaves on purpose. Add a `Behavior` to it when fixing how the reader handles a server.
- Tests count requests and bytes instead of measuring time, so they stay reliable.
- `core/tests/data/make_methods_zip.py` regenerates the compression test archive.

## Dependencies

The macOS build must only link system libraries, or the app won't launch on other machines. CI checks this when building a release. If a new dependency wraps a C library, make sure it links statically, and check with `otool -L` on the built app.

## Releases

See [Releasing](README.md#releasing). The version lives only in the root `Cargo.toml`.
