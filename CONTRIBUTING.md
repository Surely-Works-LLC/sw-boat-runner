# Contributing

Open an issue before a large change. Pull requests need `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` to pass.
CI runs all three, checks the example policy with `sw-boat-runner check`, and builds
release binaries for Linux x86_64 and macOS arm64.

A change to the decision logic in `src/decide.rs` should keep
[spec/BoatRunner.tla](spec/BoatRunner.tla) in step. The `tla` workflow runs only on
pushes to `main`, so run TLC locally first ([spec/README.md](spec/README.md#running-tlc)).

Contributions are dual-licensed under MIT or Apache-2.0, as the project is, unless you
say otherwise.
