# Evals

`evals.py` compares GitHub Actions jobs before and after a repository moves from
boat per-job sandboxes (`runs-on: boat`) to persistent self-hosted runners.

Per window and runner kind it reports jobs sampled, jobs that never got a
runner, queue time (`started_at - created_at`) and run time (`completed_at -
started_at`) at p50 and p90, and boat starts per day; then jobs per persistent
runner and persistent queue time per day.

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
  the day after cutover to `--after-end` (default: now, truncated to the hour).
- `--boat-labels` sets the per-job labels (default `boat,boat-small,boat-large,boat-xlarge`;
  a `/options` suffix also matches). `--persistent-label` sets the pool label.

## API budget

`gh api` is called at most `--max-calls` times (default 250), counting cached
reads, and the script stops if the core rate limit has under 1,500 calls left.
Responses are cached in `eval/.cache/` (gitignored), so a rerun with the same
arguments makes no calls. Runs are listed per workflow with `created=` filters;
when a window has more pages than the budget allows, evenly spaced pages are
read. The jobs budget is split evenly between windows, over runs evenly spaced
in time. Results: [results/](results/).
