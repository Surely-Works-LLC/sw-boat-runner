# The daemon inside the workflow

No host to run: the reusable [`runners` workflow](../../.github/workflows/runners.yml)
builds the daemon and runs it for the length of the calling workflow run. Add one job and
change `runs-on`:

```yaml
on:
  push:
    branches: [main]
permissions:
  contents: read
jobs:
  runners:
    if: github.event_name == 'push' && github.ref == 'refs/heads/main'
    permissions:
      contents: read
      actions: read
    uses: Surely-Works-LLC/sw-boat-runner/.github/workflows/runners.yml@main   # pin a SHA
    with:
      snapshot: snap-1
      ref: main                                                               # the same SHA
      app-id: ${{ vars.BOAT_APP_ID }}
    secrets:
      BOAT_API_KEY: ${{ secrets.BOAT_API_KEY }}
      app-private-key: ${{ secrets.BOAT_APP_PRIVATE_KEY }}

  test:
    if: github.event_name == 'push' && github.ref == 'refs/heads/main'
    runs-on: [self-hosted, "boat-${{ github.run_id }}-${{ github.run_attempt }}"]
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
        with:
          persist-credentials: false
      - run: make test
```

The trigger and the `if:` on both jobs keep the pool to pushes to `main`. Keep the `if:`
if you add triggers: in a `pull_request_target` run `github.ref` is still the base branch,
so only the event check stops it. [Pull requests](#pull-requests) says why.

The `runners` job needs `contents: read` and `actions: read` from the run's token: it
checks out the daemon and lists the run's jobs to know when they are done. `@main` and
`ref` (default `main`) are placeholders: pin both to one commit SHA of this repository,
or to a release tag that has the inputs you use (`app-id` is newer than `v0.1.0`).
GitHub tells a called workflow nothing reliable about its own ref, so `uses:` pins the
workflow and `ref` pins the daemon it builds: change both together.

`snapshot` is a boat snapshot as the [README](../../README.md#connect-a-repository)
describes. The daemon registers runners as a GitHub App installed on the repository with
Administration read and write and Actions read: pass its ID as `app-id` (a variable is
fine) and its private key as the `app-private-key` secret. The daemon mints and renews
the installation token, scoped to the calling repository, itself; the key is written to
an owner-only file in the job's temporary directory after the build and deleted at the
end. Instead of an app, pass a fine-grained token with Administration read and write and
Actions read as the `RUNNER_ADMIN_TOKEN` secret; the `GITHUB_TOKEN` Actions creates for the run cannot
register runners. Passing both, or an app ID without its key, fails the job at its first
step. Optional inputs:

| Input | Default | |
| - | - | - |
| `machine_type` | `default` | |
| `max_boxes` | 4 | |
| `pool` | `preset = "fork"` | More policy lines for the pool, such as `preset = "warm"`. |
| `name` | none | Set it when one run calls the workflow more than once. The label is then `boat-<run id>-<attempt>-<name>`. |
| `ref` | `main` | The sw-boat-runner tag or commit to build. Keep it the same as the `@` ref of `uses:`. |
| `app-id` | none | The GitHub App's ID or client ID; its key goes in the `app-private-key` secret. Empty means `RUNNER_ADMIN_TOKEN`. |

The label names the run and the attempt, so each run gets its own pool and two runs never
share a daemon. The `runners` job ends once every other job of the run is done, and then
deletes the pool's machines and runners with `sw-boat-runner drain`.

What this costs compared with a daemon that stays up:

- With the default `fork` preset every job is one start, as with `runs-on: boat`, on a
  fresh machine from your snapshot with none of the account's secrets, as long as the
  snapshot was saved from a `--no-env` sandbox
  ([README](../../README.md#connect-a-repository), step 2). `pool: preset =
  "warm"` reuses machines among the jobs of one run. Nothing is kept between runs; a
  [daemon that stays up](../always-on/) does that.
- Jobs wait for the daemon to build and for its 90 s restart wait, about 3 minutes in
  all, and the `runners` job uses GitHub-hosted minutes for the whole run.
- It makes only starts the plan includes. A job past them waits.

[self-hosted.yml](../../.github/workflows/self-hosted.yml) in this repository is a
working caller that runs `cargo test` on a machine the daemon forks. Its jobs are skipped
unless the repository variable `BOAT_SNAPSHOT` (a snapshot with a Rust toolchain) is set,
along with the variable `BOAT_APP_ID` and the secrets `BOAT_API_KEY` and
`BOAT_APP_PRIVATE_KEY`, and they are not set here, so it does not run in this repository. A
fork that sets them runs it.

## Pull requests

The runner is registered on the repository, so while the daemon runs it serves any queued
job that asks for `[self-hosted, boat-<run id>-<attempt>]`, not only the calling run's
jobs. The label is hard to guess ahead of time, but the run id is visible while the run
is going, and a pull request can add a job with that label to any workflow, a fork's
included once its run is approved. The machine is forked for that one job and deleted
after it, and the job gets none of the daemon's secrets, but it does run that pull
request's code on a boat machine. Call the workflow only from runs you trust, such as
pushes to your default branch, as the example above does, and approve fork pull request
runs only after reading their workflow changes.
