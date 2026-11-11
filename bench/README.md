# Bench

```sh
cargo run --release --example bench
```

[main.rs](main.rs) simulates one day of CI at eight loads and counts boat starts. It
writes [results.csv](results.csv) and [starts.svg](starts.svg), and takes about a minute.

To try your own pool at one load, pass the jobs a day and the pool's fields; a `\n`
separates lines. It prints the CSV header and one row, setup `custom`, and writes no
files. The pool's labels and `machine_type = "large"` are fixed, so leave them out:

```sh
cargo run --release --example bench -- 2000 'preset = "warm"\nslots_per_box = 2\nmax_boxes = 12'
```

The mock runs every job at the same speed however many slots share its machine, so a
higher `slots_per_box` looks better here than it will on a real machine.

- `runs-on: boat` is mocked: every job starts one machine, waits 8 s for it, and the
  machine is gone when the job ends.
- The daemon is real: `run::Loop` ticks every 15 s with `decide::decide`, and only the
  HTTP transport is replaced, by a mock of the boat and GitHub endpoints it calls.

Four policies run through the daemon: `fork` (may pay for starts), `hybrid` with
`min_up = 4`, `warm`, and `pool` with 16 machines. The first three allow 48 machines.
`fork` makes a start per job, as `runs-on: boat` does, so the chart leaves it out. The 16
`pool` machines make no start and cost $27.65 a day at any load; past about 2,000 jobs a
day they are too few and jobs queue. By 1,500 jobs a day `hybrid` has used all 840
included starts. With no paid starts allowed it then queues jobs: the p90 wait is 4.2 h at
2,000, and 1,779 jobs never get a runner at 5,000. A job that never gets a runner counts in
the queue columns as waiting until the simulation ends, four hours after the day.

## Assumptions

| | Value | Source |
| - | - | - |
| Plan | $100: 30 / 210 / 840 starts included per minute / hour / day | [boat's limits](https://docs.boat.dev/limits.md) |
| Extra start | $0.03 up to 1.5 times the included number, $0.05 up to 2 times, $0.07 up to 6 times (3 times per minute), then refused; the highest of the three windows applies | the same page |
| `large` machine | $0.072 an hour | [boat's pricing](https://docs.boat.dev/pricing.md) |
| `runs-on: boat` | one fresh machine per job, so one start per job | [boat's CI page](https://docs.boat.dev/ci.md) |
| Resume within 10 min of a stop | not a start | boat's limits |
| Wait for a `runs-on: boat` machine | 8 s | the median in [the eval](../eval/results/example-2026-11.md) |
| Runner comes online | 10 s after it is registered | assumed |

`runs-on: boat` is simulated with extra starts paid for; without them its jobs wait or fail
at a full limit. boat counts starts over rolling windows. The bench counts them per clock
minute, hour and day, which reproduces the daily bill of the worked example on boat's CI
page. Faults are not modelled: the mocked boat and GitHub answer every call, and boat
lists a machine as soon as it is created. The TLA+ model and the tests in `src/run/`
cover those.

Pushes arrive as a Poisson process, six times as often from 08:00 to 20:00 UTC as at
night, and each queues 1 to 4 jobs. Run times are lognormal with a median of 110 s and a
p90 of 10 min, close to the eval's. The seed is fixed, so a rerun gives the same file.
