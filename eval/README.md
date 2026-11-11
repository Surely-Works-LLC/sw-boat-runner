# Evals

`evals.py` compares GitHub Actions jobs before and after a repository moves from
boat's per-job machines (`runs-on: boat`) to persistent self-hosted runners. It is an
offline analysis, not part of the daemon: a standard-library Python script that reads
`boat ci --json` output and calls `gh api`.

Per window and runner kind it reports, over the completed, non-skipped jobs of the
sampled runs:

- `jobs`: jobs that got a runner. Queue and run times are over these jobs only.
- `no runner`: jobs that completed without ever getting a runner, for example cancelled
  while queued. They are not in `jobs`, so the sampled total is the sum of the two.
- queue time (`started_at - created_at`) and run time (`completed_at - started_at`) at
  p50 and p90, and the window's boat starts per day.

One more row gives wait and run time from the `boat ci` log itself. Two further tables
give jobs per persistent runner and persistent queue time per day.

The one result so far, [example-2026-11.md](results/example-2026-11.md), measured four
long-lived runners registered once on always-on machines, not this daemon. Their names
are not `sbr-...`, and each ran many jobs where a daemon runner runs one. Its queue times
(p50 5.6 min, p90 27.8 min) are those of too few fixed runners. The README's 17 s / 24 s
come from the [bench](../bench/), a simulation of the daemon. Neither measures the other;
what the result shows about keeping machines up is the drop in boat starts.

## Reproduce

Needs Python 3.9+, the `boat` CLI, and `gh` with read access to Actions.

```sh
boat ci --json --limit 500 > boat-ci.json
python3 -I eval/evals.py --repo OWNER/REPO --cutover 2026-11-05T21:52:45Z \
  --boat-ci boat-ci.json --persistent-label sw-boat-runner \
  --after-end 2026-11-09T19:00:00Z \
  --workflow ci.yml --workflow release.yml > result.md
```

- `--workflow` takes a workflow name or file name. Use file names for workflows
  that were deleted or never reached the default branch; the workflows listing
  omits them. Without it, the names in the boat jobs are used.
- Windows: before is 00:00Z the day before cutover to cutover; after is 00:00Z
  the day after cutover to now, truncated to the hour. `--before-start`,
  `--after-start` and `--after-end` override the defaults.
- `--boat-labels` sets the per-job labels (default `boat,boat-small,boat-large,boat-xlarge`;
  a `/options` suffix also matches). `--persistent-label` sets the persistent runners' label.

## API budget

`gh api` is called at most `--max-calls` times (default 250), counting cached
reads, plus one retry per failed call and one rate-limit check. That check runs before
the first uncached call and stops the script if the core rate limit has under 1,500
calls left.
Responses are cached in `eval/.cache/` (gitignored), so a rerun with the same
arguments makes no calls. Runs are listed per workflow with `created=` filters;
when a window has more pages than the budget allows, evenly spaced pages are
read. The jobs budget is split evenly between windows, over runs evenly spaced
in time. Results: [results/](results/).
