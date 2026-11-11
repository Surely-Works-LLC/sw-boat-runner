# Contributing

Open an issue before a large change. Pull requests need `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` to pass.
CI runs all three, checks the example policy with `sw-boat-runner check`, and builds
the release binaries: static Linux x86_64 and arm64 (musl), and macOS arm64.

A change to the decision logic in `src/decide.rs` should keep
[spec/BoatRunner.tla](spec/BoatRunner.tla) in step. The `tla` workflow runs only on
pushes to `main`, so run TLC locally first ([spec/README.md](spec/README.md#running-tlc)).

To release `vX.Y.Z`:

1. Set `version` in `Cargo.toml` and run `cargo check` so `Cargo.lock` follows.
2. Add a `CHANGELOG.md` entry for the version, dated.
3. The examples follow `main` (`@main` and `ref: main` in
   `examples/in-workflow/README.md` and the `ref` default in
   `.github/workflows/runners.yml`), with a note to pin a SHA. Point them at the tag if
   you like.
4. Merge, then tag the merge commit `vX.Y.Z` and push the tag. The `release` workflow
   checks that the tag matches `Cargo.toml`, builds the binaries, attests their build
   provenance and publishes them with `SHA256SUMS` on the GitHub Release, with the
   version's `CHANGELOG.md` section as its notes. `install.sh` installs the latest
   release from then on.

Contributions are dual-licensed under MIT or Apache-2.0, as the project is, unless you
say otherwise.
