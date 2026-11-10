# This repository on its own runners

[.github/workflows/self-hosted.yml](../../.github/workflows/self-hosted.yml) runs this
repository's tests on a boat machine that sw-boat-runner forks for the job. Its `daemon`
job builds the daemon and runs it with [policy.toml](policy.toml); its `test` job asks for
`runs-on: [self-hosted, sw-boat-runner]`. The daemon sees the queued job, forks a machine,
registers a runner on it, and deletes both when the job ends. The `daemon` job's log shows
each step.

Running the daemon inside the workflow keeps the example self-contained. A real setup
runs it on a host that stays up, as the [README](../../README.md#connect-a-repository)
describes.

The workflow runs on pushes to `main` and on manual dispatch, and skips both jobs until
it is set up:

- Repository variable `BOAT_SNAPSHOT`: a boat snapshot as the
  [README](../../README.md#connect-a-repository) describes, with a Rust toolchain
  installed.
- Secret `BOAT_API_KEY`.
- Secret `RUNNER_ADMIN_TOKEN`: a fine-grained personal access token for this repository
  with Administration read and write and Actions read. The `GITHUB_TOKEN` that Actions
  creates for each run cannot be given Administration, so it cannot register runners.

CI validates the policy on every pull request with `sw-boat-runner check`.

## Pull requests

The runner is registered on the repository, so while the daemon runs it serves any queued
job that asks for `[self-hosted, sw-boat-runner]`, not only this workflow's `test` job. A
pull request can add such a job to any workflow, a fork's included once its run is
approved. The machine is forked for that one job and deleted after it, and the job gets
none of the daemon's secrets, but it does run that pull request's code on a boat machine.
Approve fork pull request runs only after reading their workflow changes.
