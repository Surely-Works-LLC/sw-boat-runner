# Bench

```sh
cargo run --release --example bench
```

[main.rs](main.rs) simulates one day of CI at eight loads and counts boat starts. It
writes [results.csv](results.csv) and [starts.svg](starts.svg), and takes about a minute.

- `runs-on: boat` is mocked: every job starts one machine, waits 8 s for it, and the
  machine is gone when the job ends.
- The daemon is real: `run::Loop` ticks every 15 s with `decide::decide`, and only the
  HTTP transport is replaced, by a mock of the boat and GitHub endpoints it calls.

Four policies run through the daemon: `fork` (may pay for starts), `hybrid` with
`min_up = 4`, `warm`, and `pool` with 16 machines. The first three allow 48 machines.
`fork` makes a start per job, as `runs-on: boat` does, so the chart leaves it out. The 16
`pool` machines make no start and cost $27.65 a day at any load; past about 2,000 jobs a
day they are too few and jobs queue. By 2,000 jobs a day `hybrid` has used all 1,250
included starts. With no paid starts allowed it then queues jobs: the p90 wait is 5.8 h at
3,000, and 985 jobs never get a runner at 5,000. A job that never gets a runner counts in
the queue columns as waiting until the simulation ends, four hours after the day.

## Assumptions

boat's documentation was not reachable when this was written, so the numbers about boat
come from this repository:

| | Value | Source |
| - | - | - |
| Included starts | 1,250 a day | estimated from the bill in [the eval](../eval/results/example-2026-11.md): 3,131 starts and $69.05 of extra starts on one day |
| Extra start | $0.037 | the same bill: $70.79 for 1,915 extra starts |
| `large` machine | $0.072 an hour | [docs/design.md](../docs/design.md#start-budget) |
| Wait for a `runs-on: boat` machine | 8 s | the eval's median |
| Resume within 10 min of a stop | not a start | docs/design.md |
| Runner comes online | 10 s after it is registered | assumed |

The per-minute and per-hour start limits are not modelled, and neither are faults: the
mocked boat and GitHub answer every call, and boat lists a machine as soon as it is
created. The TLA+ model and the tests in `src/run.rs` cover those.

Pushes arrive as a Poisson process, six times as often from 08:00 to 20:00 UTC as at
night, and each queues 1 to 4 jobs. Run times are lognormal with a median of 110 s and a
p90 of 10 min, close to the eval's. The seed is fixed, so a rerun gives the same file.
