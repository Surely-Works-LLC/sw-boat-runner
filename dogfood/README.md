# Dogfood

[.github/workflows/dogfood.yml](../.github/workflows/dogfood.yml) runs this repository's
tests on a boat machine that sw-boat-runner forks for the job. Its `daemon` job builds
the daemon and runs it with [policy.toml](policy.toml); its `test` job asks for
`runs-on: [self-hosted, sw-boat-runner-dogfood]`. The daemon sees the queued job, forks a
machine, registers a runner on it, and deletes both when the job ends. The `daemon` job's
log shows each step.

The workflow runs on pushes to `main` and on manual dispatch, and skips both jobs until
it is set up:

- Repository variable `DOGFOOD_SNAPSHOT`: a boat snapshot as the
  [README](../README.md#snapshot) describes, with a Rust toolchain installed.
- Secret `BOAT_API_KEY`.
- Secret `DOGFOOD_GITHUB_TOKEN`: a fine-grained token for this repository with
  Administration read and write and Actions read. The workflow's own `GITHUB_TOKEN`
  cannot register runners.

CI validates the policy on every pull request with `sw-boat-runner check`.
