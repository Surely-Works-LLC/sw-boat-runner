<!-- What changes and why. Open an issue first for a large change. -->

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --locked`
- [ ] A change to `src/decide.rs` keeps `spec/BoatRunner.tla` in step, and TLC passes
      locally (spec/README.md, Running TLC)
