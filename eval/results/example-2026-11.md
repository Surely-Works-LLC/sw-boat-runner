# Example repository, November 2026

example-org/example-repo moved its `runs-on: boat` jobs to four persistent
self-hosted runners (label `pool-a`, names `pool-*`) on always-on
boat machines. The change merged at 2026-11-05T21:52:45Z; the last `runs-on: boat`
job was queued at 21:53Z.

Repository, label, runner and workflow names are placeholders; times and counts
are as measured.

## Method

```sh
python3 -I eval/evals.py --repo example-org/example-repo \
  --cutover 2026-11-05T21:52:45Z --boat-ci boat-ci-2026-11-09.json \
  --persistent-label pool-a --after-end 2026-11-09T19:00:00Z \
  --workflow ci-old.yml --workflow ci.yml --workflow release.yml \
  --workflow tooling.yml --workflow infra.yml --workflow containers.yml
```

- Workflows: the ones whose jobs ran on boat (`boat ci` job names: CI,
  release, tooling, infra, containers).
  `ci-old.yml` was renamed `ci.yml` on 2026-11-09 and `tooling.yml` was
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
- Runner kind comes from job labels: `boat-large` is a boat per-job machine,
  `pool-a` is a persistent runner, anything else (`other-a`,
  `other-b`) is "other" and not part of the comparison.

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
| pool-box1-1 | 39 | 2026-11-09T16:32:09Z |
| pool-box1-2 | 11 | 2026-11-07T23:11:07Z |
| pool-box2-1 | 36 | 2026-11-09T16:41:43Z |
| pool-box2-2 | 11 | 2026-11-07T15:13:03Z |

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
- boat starts went from about 2,200/day to zero, and the extra-starts charge went
  from $69.05 on 2026-11-05 to $0 from 2026-11-07. No job has been refused for
  an empty wallet since.
- Run time is similar at p50 (116s before, 108s after) and lower at p90 (10.6m
  to 7.6m). The workflow mix changed, so this is not evidence that the persistent
  runners are faster.

What got worse:
- Queue time. boat machines started a job in 8s at p50 and 112s at p90; the
  persistent runners take 5.6m and 27.8m. 82 of 179 persistent jobs (46%) were cancelled before any
  runner picked them up, against 22 of 179 (12%) on boat. Long queues make it
  more likely that a newer push cancels a queued run.
- Two of the four persistent runners, `pool-box1-2` and
  `pool-box2-2`, are offline today; neither started a sampled job after
  2026-11-07. 2026-11-08, with only two runners taking work, has the worst
  queue (p50 21.4m, p90 46.0m). With all four up on 2026-11-06 and 2026-11-07
  the p50 was still 3 to 6 minutes, so four runners are too few for peak load.
- The boat p90 of 112s is not typical boat behaviour: in the fetched jobs, boat
  queues over a minute cluster on 2026-11-05 12:00-13:59Z and 18:00-18:59Z, the
  second around the empty-wallet refusals. The `boat ci` log for 19:34-21:53Z
  shows 8s/10s.

Caveats:
- Sampled: 108 runs per window, and the per-day rows hold 16 to 41 jobs each.
- Different traffic: tooling ran only in the before window, and the
  workflows themselves changed between windows (`ci-old.yml` became
  `ci.yml`, release jobs moved).
- Machine time for always-on machines is paid whether or not they run CI, and the
  dashboard does not split CI use from other use of those machines. The saving
  shown here is the extra-starts line, not machine time.
- boat starts/day uses whole calendar days from boat stats, so the before value
  counts all of 2026-11-05.
