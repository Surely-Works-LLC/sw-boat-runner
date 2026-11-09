# mjth-monorepo, November 2026

Surely-Works-LLC/mjth-monorepo moved its `runs-on: boat` jobs to four persistent
self-hosted runners (label `mjth-boat-pool`, names `boat-pool-*`) on always-on
boat boxes. The change merged at 2026-11-05T21:52:45Z; the last `runs-on: boat`
job was queued at 21:53Z.

## Method

```sh
python3 -I eval/evals.py --repo Surely-Works-LLC/mjth-monorepo \
  --cutover 2026-11-05T21:52:45Z --boat-ci boat-ci-2026-11-09.json \
  --persistent-label mjth-boat-pool --after-end 2026-11-09T19:00:00Z \
  --workflow hg-next.yml --workflow rust-ci.yml --workflow next-release.yml \
  --workflow go-tooling.yml --workflow terraform-ci.yml --workflow container-runtime.yml
```

- Workflows: the ones whose jobs ran on boat (`boat ci` job names: Experimental
  Rust POC, next-release, Go tooling, terraform-ci, container-runtime).
  `hg-next.yml` was renamed `rust-ci.yml` on 2026-11-09 and `go-tooling.yml` was
  deleted on 2026-11-07, so they are passed by file name.
- Windows: before 2026-11-04T00:00Z to 2026-11-05T21:52:45Z; after
  2026-11-06T00:00Z to 2026-11-09T19:00Z.
- Sample: 108 runs per window, evenly spaced in time, from up to 4 pages of 100
  runs per workflow and window. All completed, non-skipped jobs of those runs.
- Queue time is GitHub's job `started_at - created_at`. On the 108 jobs that
  were fetched and also appear in the `boat ci` log, it equals boat's
  `waitSeconds` to the second, so it does not include waiting on `needs:`.
- "no runner" counts completed jobs that never got a runner; all of them here
  were cancelled while queued.
- Runner kind comes from job labels: `boat-large` is a boat per-job sandbox,
  `mjth-boat-pool` is a persistent runner, anything else (`mjth-release`,
  `mjth-ci-short`) is "other" and not part of the comparison.

## Results

| window | runner kind | jobs | no runner | queue p50 | queue p90 | run p50 | run p90 | boat starts/day |
|---|---|---|---|---|---|---|---|---|
| before (2026-11-04T00:00:00Z to 2026-11-05T21:52:45Z, 108 runs) | boat | 157 | 22 | 8s | 112s | 116s | 10.6m | 2228 |
| before (2026-11-04T00:00:00Z to 2026-11-05T21:52:45Z, 108 runs) | other | 48 | 37 | 97s | 8.7m | 3.0m | 4.3m | 2228 |
| after (2026-11-06T00:00:00Z to 2026-11-09T19:00:00Z, 108 runs) | other | 25 | 43 | 18s | 55s | 17s | 102s | 0 |
| after (2026-11-06T00:00:00Z to 2026-11-09T19:00:00Z, 108 runs) | persistent | 97 | 82 | 5.6m | 27.8m | 108s | 7.6m | 0 |
| boat ci log (497 jobs, 2026-11-05T19:34 to 2026-11-05T21:53) | boat | 497 | - | 8s | 10s | 99s | 5.5m | - |

| persistent runner | jobs | last job started |
|---|---|---|
| boat-pool-958arusm-1 | 39 | 2026-11-09T16:32:09Z |
| boat-pool-958arusm-2 | 11 | 2026-11-07T23:11:07Z |
| boat-pool-k64zbt5a-1 | 36 | 2026-11-09T16:41:43Z |
| boat-pool-k64zbt5a-2 | 11 | 2026-11-07T15:13:03Z |

| day (persistent jobs) | jobs | queue p50 | queue p90 |
|---|---|---|---|
| 2026-11-06 | 16 | 3.2m | 15.8m |
| 2026-11-07 | 17 | 5.8m | 29.1m |
| 2026-11-08 | 23 | 21.4m | 46.0m |
| 2026-11-09 | 41 | 4.8m | 12.0m |

## boat stats (`boat ci --json --limit 500`, 2026-11-09)

- 5,268 starts from 2026-11-02 to 2026-11-05, none from 2026-11-06 on.
  Peak 3,131 on 2026-11-05; 1,325 on 2026-11-04.
- 707 jobs refused on 2026-11-05 (first at 18:43Z): "plan's included time is
  used up and your wallet is empty".
- boat's own estimate: $34.54 of compute for the 5,268 jobs; median wait 8s,
  median run 213s.

## Billing (from the boat billing dashboard, 2026-11-09, last 30 days)

- Total $1,160.98: machine time $1,090.19, extra starts $70.79 (1,915 extra
  starts), snapshots and AI $0.
- 2026-11-05: extra starts $69.05, machine time $87.29.
- 2026-11-06: a small amount of extra starts. 2026-11-07 to 2026-11-09: $0.
- Machine time about $70/day on 2026-11-06 to 2026-11-08, about $45 on
  2026-11-09 (partial day).

## Reading

What improved:
- Boat starts went from about 2,200/day to zero, and the extra-starts charge went
  from $69.05 on 2026-11-05 to $0 from 2026-11-07. No job has been refused for
  an empty wallet since.
- Run time is similar at p50 (116s before, 108s after) and lower at p90 (10.6m
  to 7.6m). The workflow mix changed, so this is not evidence that the persistent
  runners are faster.

What got worse:
- Queue time. Boat sandboxes started a job in 8s at p50 and 112s at p90; the pool
  takes 5.6m and 27.8m. 82 of 179 pool jobs (46%) were cancelled before any
  runner picked them up, against 22 of 179 (12%) on boat. Long queues make it
  more likely that a newer push cancels a queued run.
- Two of the four pool runners, `boat-pool-958arusm-2` and
  `boat-pool-k64zbt5a-2`, are offline today; neither started a sampled job after
  2026-11-07. 2026-11-08, with only two runners taking work, has the worst
  queue (p50 21.4m, p90 46.0m). With all four up on 2026-11-06 and 2026-11-07
  the p50 was still 3 to 6 minutes, so four runners are too few for peak load.
- The boat p90 of 112s is not typical boat behaviour: in the fetched jobs, boat
  queues over a minute cluster on 2026-11-05 12:00-13:59Z and 18:00-18:59Z, the
  second around the empty-wallet refusals. The `boat ci` log for 19:34-21:53Z
  shows 8s/10s.

Caveats:
- Sampled: 108 runs per window, and the per-day rows hold 16 to 41 jobs each.
- Different traffic: Go tooling ran only in the before window, and the
  workflows themselves changed between windows (`hg-next.yml` became
  `rust-ci.yml`, release jobs moved).
- Machine time for always-on boxes is paid whether or not they run CI, and the
  dashboard does not split pool use from other use of those boxes. The saving
  shown here is the extra-starts line, not machine time.
- Boat starts/day uses whole calendar days from boat stats, so the before value
  counts all of 2026-11-05.
