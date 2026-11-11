# Contributing

Open an issue before a large change. Pull requests need `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` to pass.
CI runs all three, checks the example policy with `sw-boat-runner check`, and builds
release binaries for Linux x86_64 and macOS arm64.

A change to the decision logic in `src/decide.rs` should keep
[spec/BoatRunner.tla](spec/BoatRunner.tla) in step. The `tla` workflow runs only on
pushes to `main`, so run TLC locally first ([spec/README.md](spec/README.md#running-tlc)).

To release `vX.Y.Z`:

1. Set `version` in `Cargo.toml` and run `cargo check` so `Cargo.lock` follows.
2. Add a `CHANGELOG.md` entry for the version, dated.
3. Change every `v0.1.0`-style pin to the new tag: the `cargo install` lines in the
   README and `examples/always-on/install.sh`, the `install.sh` URL in
   `examples/always-on/README.md`, `@` and `ref` in `examples/in-workflow/README.md`, and
   the `ref` default in `.github/workflows/runners.yml`.
4. Merge, then tag the merge commit `vX.Y.Z` and push the tag.

Contributions are dual-licensed under MIT or Apache-2.0, as the project is, unless you
say otherwise.
