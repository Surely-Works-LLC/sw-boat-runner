# The daemon inside the workflow

No host to run: the reusable [`runners` workflow](../../.github/workflows/runners.yml)
builds the daemon and runs it for the length of the calling workflow run. Add one job and
change `runs-on`:

```yaml
permissions:
  contents: read
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
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
        with:
          persist-credentials: false
      - run: make test
```

The `runners` job needs only `contents: read` from the run's token. `@main` follows this
repository; for anything you rely on, put a commit SHA there and in `ref`.

`snapshot` is a boat snapshot as the [README](../../README.md#connect-a-repository)
describes. `RUNNER_ADMIN_TOKEN` is the fine-grained token from the same section; the
`GITHUB_TOKEN` Actions creates for the run cannot register runners. Optional inputs:

| Input | Default | |
| - | - | - |
| `machine_type` | `default` | |
| `max_boxes` | 4 | |
| `pool` | `preset = "fork"` | More policy lines for the pool, such as `preset = "warm"`. |
| `name` | none | Set it when one run calls the workflow more than once. The label is then `boat-<run id>-<attempt>-<name>`. |
| `ref` | `main` | The sw-boat-runner commit to build. Pin one for anything you rely on. |

The label names the run and the attempt, so each run gets its own pool and two runs never
share a daemon. The `runners` job ends once every other job of the run is done, and then
deletes the pool's machines and runners with `sw-boat-runner drain`.

What this costs compared with a daemon that stays up:

- With the default `fork` preset every job is one start, as with `runs-on: boat`, on a
  fresh machine from your snapshot with none of the account's secrets. `pool: preset =
  "warm"` reuses machines among the jobs of one run. Nothing is kept between runs; a
  [daemon that stays up](../always-on/) does that.
- Jobs wait for the daemon to build and for its 90 s restart wait, about 3 minutes in
  all, and the `runners` job uses GitHub-hosted minutes for the whole run.
- It makes only starts the plan includes. A job past them waits.

This repository uses it on itself:
[self-hosted.yml](../../.github/workflows/self-hosted.yml) runs `cargo test` on a machine
the daemon forks. Both of its jobs are skipped until the repository variable
`BOAT_SNAPSHOT` (a snapshot with a Rust toolchain) and the two secrets are set.

## Pull requests

The runner is registered on the repository, so while the daemon runs it serves any queued
job that asks for `[self-hosted, boat-<run id>-<attempt>]`, not only the calling run's
jobs. The label is hard to guess ahead of time, but the run id is visible while the run
is going, and a pull request can add a job with that label to any workflow, a fork's
included once its run is approved. The machine is forked for that one job and deleted
after it, and the job gets none of the daemon's secrets, but it does run that pull
request's code on a boat machine. Call the workflow only from runs you trust, such as
pushes to your default branch (this repository's self-hosted.yml checks for `main`), and
approve fork pull request runs only after reading their workflow changes.
