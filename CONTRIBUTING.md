# Contributing

Open an issue before a large change. Pull requests need `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` to pass;
CI runs all three.

A change to the decision logic in `src/decide.rs` should keep
[spec/BoatRunner.tla](spec/BoatRunner.tla) in step. The TLA+ workflow runs only on
`main`, so run TLC locally first ([spec/README.md](spec/README.md#running-tlc)).

Contributions are dual-licensed under MIT or Apache-2.0, as the project is, unless you
say otherwise.
