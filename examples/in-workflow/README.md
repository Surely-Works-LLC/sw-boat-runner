# The daemon inside the workflow

No host to run: the reusable [`runners` workflow](../../.github/workflows/runners.yml)
builds the daemon and runs it for the length of the calling workflow run. Add one job and
change `runs-on`:

```yaml
jobs:
  runners:
    uses: Surely-Works-LLC/sw-boat-runner/.github/workflows/runners.yml@main
    with:
      snapshot: snap-1
    secrets:
      BOAT_API_KEY: ${{ secrets.BOAT_API_KEY }}
      RUNNER_ADMIN_TOKEN: ${{ secrets.RUNNER_ADMIN_TOKEN }}

  test:
    runs-on: [self-hosted, "boat-${{ github.run_id }}-${{ github.run_attempt }}"]
    steps:
      - uses: actions/checkout@v4
      - run: make test
```

`snapshot` is a boat snapshot as the [README](../../README.md#connect-a-repository)
describes. `RUNNER_ADMIN_TOKEN` is the fine-grained token from the same section; the
`GITHUB_TOKEN` Actions creates for the run cannot register runners. Optional inputs:
`machine_type` (`default`), `max_boxes` (4) and `ref`, the sw-boat-runner commit to build
(`main`; pin a commit for anything you rely on).

The label names the run and the attempt, so each run gets its own pool and two runs never
share a daemon. The `runners` job ends once every other job of the run is done and their
machines are deleted.

What this costs compared with a daemon that stays up:

- The pool uses the `fork` preset, so every job is one start, as with `runs-on: boat`.
  It gives each job a fresh machine from your snapshot with none of the account's
  secrets. It does not save starts; a [daemon that stays up](../always-on/) does.
- Jobs wait for the daemon to build and for its 90 s restart wait, about 3 minutes in
  all, and the `runners` job uses GitHub-hosted minutes for the whole run.
- It makes only starts the plan includes. A job past them waits.

This repository uses it on itself:
[self-hosted.yml](../../.github/workflows/self-hosted.yml) runs `cargo test` on a machine
the daemon forks. Both of its jobs are skipped until the repository variable
`BOAT_SNAPSHOT` (a snapshot with a Rust toolchain) and the two secrets are set.
