//! Counts boat starts for one simulated day of CI, at several loads, two ways:
//! boat's managed `runs-on: boat` (mocked: one start per job) and this daemon's real
//! tick loop (`run::Loop` with `decide::decide`) against a mocked boat and GitHub API.
//! Writes `bench/results.csv` and `bench/starts.svg`. bench/README.md lists the
//! assumptions.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Value, json};
use sw_boat_runner::run::Loop;
use sw_boat_runner::transport::{NetworkError, Response, Transport};
use sw_boat_runner::{decide, github, policy};

const TICK_S: u64 = 15;
const DAY_S: u64 = 86_400;
/// The measured day starts here. The simulation starts an hour earlier, so the daemon's
/// restart rules have run out by then.
const DAY_START: u64 = 20_000 * DAY_S;
const SIM_START: u64 = DAY_START - 3_600;
/// Jobs created during the day get this long after it to finish.
const DRAIN_S: u64 = 4 * 3_600;

/// The $100 plan's included starts per minute, hour and day
/// (https://docs.boat.dev/limits.md), as (window seconds, included, most with extra starts).
/// boat's windows roll; these are counted per clock minute, hour and day, which gives the
/// daily bill of the worked example on https://docs.boat.dev/ci.md.
const START_WINDOWS: [(u64, u64, u64); 3] = [
    (60, 30, 3 * 30),
    (3_600, 210, 6 * 210),
    (DAY_S, 840, 6 * 840),
];
const INCLUDED_STARTS_PER_DAY: u64 = 840;
/// Included starts a day on the other plans, for the chart.
const OTHER_PLANS: [(&str, u64); 3] = [("$20", 200), ("$500", 1_680), ("$2000", 2_400)];
/// A `large` machine, from https://docs.boat.dev/pricing.md.
const MACHINE_CENTS_PER_HOUR: f64 = 7.2;
/// boat's median wait for a `runs-on: boat` job, from the same eval.
const BOOT_S: u64 = 8;
/// A registered runner takes this long to come online. An assumption.
const RUNNER_ONLINE_S: u64 = 10;
/// boat returns a machine stopped less than this long ago without counting a start.
const KEEP_S: u64 = 600;

const LOADS: [u64; 8] = [100, 250, 500, 1_000, 1_500, 2_000, 3_000, 5_000];
/// (name, pool fields). `fork` may pay for starts, as `runs-on: boat` does; the others
/// keep the default cap of 0 and so never make a paid start.
const SETUPS: [(&str, &str); 4] = [
    (
        "fork",
        "preset = \"fork\"\nmax_boxes = 48\npaid_start_cents_per_day = 1000000",
    ),
    ("hybrid", "preset = \"hybrid\"\nmin_up = 4\nmax_boxes = 48"),
    ("warm", "preset = \"warm\"\nmax_boxes = 48"),
    ("pool", "preset = \"pool\"\nmin_up = 16\nmax_boxes = 16"),
];

struct Rng(u64);

impl Rng {
    /// splitmix64
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1].
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        (-2.0 * self.unit().ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }
}

#[derive(Clone)]
struct Job {
    id: u64,
    push: u64,
    created: u64,
    run_s: u64,
    started: Option<u64>,
}

/// The measured day of jobs and the hour before it. Pushes arrive as a Poisson process, six
/// times as often from 08:00 to 20:00 UTC as at night, and each queues 1 to 4 jobs.
/// Run times are lognormal with a median of 110 s and a p90 of 10 min, as in the eval.
fn workload(jobs_per_day: u64, seed: u64) -> Vec<Job> {
    let mut rng = Rng(seed);
    let mut jobs = Vec::new();
    let mut push = 0;
    for t in (SIM_START..DAY_START + DAY_S).step_by(TICK_S as usize) {
        let hour = t % DAY_S / 3_600;
        let weight = if (8..20).contains(&hour) { 6.0 } else { 1.0 };
        // The weights sum to 84 over a day, and a push holds 2.5 jobs on average.
        let pushes_per_tick = jobs_per_day as f64 / 2.5 * weight / 84.0 / (3_600 / TICK_S) as f64;
        // Knuth's Poisson sampler; the mean is far below 1.
        let (limit, mut k, mut p) = ((-pushes_per_tick).exp(), 0, rng.unit());
        while p > limit {
            k += 1;
            p *= rng.unit();
        }
        for _ in 0..k {
            push += 1;
            for _ in 0..rng.next() % 4 + 1 {
                let run_s = (110.0 * (1.32 * rng.normal()).exp()).clamp(5.0, 3_600.0) as u64;
                jobs.push(Job {
                    id: jobs.len() as u64 + 1,
                    push,
                    created: t - rng.next() % TICK_S,
                    run_s,
                    started: None,
                });
            }
        }
    }
    jobs.sort_by_key(|j| j.created);
    jobs
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Booting { until: u64 },
    Up,
    Archiving { until: u64 },
    Archived { at: u64 },
}

struct Sandbox {
    id: String,
    labels: Value,
    state: State,
}

struct GhRunner {
    id: u64,
    name: String,
    machine: String,
    online_at: u64,
    /// When its job ends, once it has one.
    busy_until: Option<u64>,
}

/// boat and GitHub as the daemon sees them through `Transport`.
#[derive(Default)]
struct World {
    now: u64,
    sandboxes: Vec<Sandbox>,
    runners: Vec<GhRunner>,
    /// Every job, sorted by `created`; `arrived` of them are visible so far.
    jobs: Vec<Job>,
    arrived: usize,
    next_id: u64,
    /// Starts so far in the current clock minute, hour and day, in `START_WINDOWS` order.
    window_starts: [u64; 3],
    /// Counted over the measured day only.
    starts: u64,
    paid_starts: u64,
    start_cents: u64,
    machine_s: u64,
}

impl World {
    fn in_day(&self) -> bool {
        (DAY_START..DAY_START + DAY_S).contains(&self.now)
    }

    fn advance(&mut self, now: u64) {
        if self.in_day() {
            let running = self
                .sandboxes
                .iter()
                .filter(|s| !matches!(s.state, State::Archived { .. }))
                .count();
            self.machine_s += running as u64 * (now - self.now);
        }
        for (count, (window, ..)) in self.window_starts.iter_mut().zip(START_WINDOWS) {
            if now / window != self.now / window {
                *count = 0;
            }
        }
        self.now = now;
        for s in &mut self.sandboxes {
            match s.state {
                State::Booting { until } if until <= now => s.state = State::Up,
                State::Archiving { until } if until <= now => {
                    // The keep window counts from the stop request.
                    s.state = State::Archived { at: until - TICK_S }
                }
                _ => {}
            }
        }
        while self
            .jobs
            .get(self.arrived)
            .is_some_and(|j| j.created <= now)
        {
            self.arrived += 1;
        }
        // GitHub hands each queued job, in that order, to the idle online runner that came
        // online first. A runner that finishes its job is gone: JIT runners take one job.
        self.runners
            .retain(|r| r.busy_until.is_none_or(|t| t > now));
        for job in &mut self.jobs[..self.arrived] {
            if job.started.is_some() {
                continue;
            }
            let idle = self
                .runners
                .iter_mut()
                .filter(|r| r.busy_until.is_none() && r.online_at <= now)
                .min_by_key(|r| r.online_at);
            let Some(runner) = idle else { break };
            let start = job.created.max(runner.online_at);
            job.started = Some(start);
            runner.busy_until = Some(start + job.run_s);
        }
        self.runners
            .retain(|r| r.busy_until.is_none_or(|t| t > now));
    }

    /// What boat charges for a start made now, in cents: nothing inside every window's
    /// included starts, else the highest bracket among the windows (3 cents up to 1.5
    /// times the included number, 5 up to 2 times, 7 up to the most). `None` when a
    /// window is at its most and boat would refuse.
    fn next_start_cents(&self) -> Option<u64> {
        let mut cents = 0;
        for (count, (_, included, most)) in self.window_starts.iter().zip(START_WINDOWS) {
            let n = count + 1;
            cents = cents.max(match n {
                _ if n <= included => 0,
                _ if 2 * n <= 3 * included => 3,
                _ if n <= 2 * included => 5,
                _ if n <= most => 7,
                _ => return None,
            });
        }
        Some(cents)
    }

    /// Makes a start if boat allows one.
    fn count_start(&mut self) -> bool {
        let Some(cents) = self.next_start_cents() else {
            return false;
        };
        self.window_starts.iter_mut().for_each(|count| *count += 1);
        if self.in_day() {
            self.starts += 1;
            self.paid_starts += u64::from(cents > 0);
            self.start_cents += cents;
        }
        true
    }

    /// boat's answer to a start it will not make.
    fn refusal() -> (u16, Value) {
        (
            429,
            json!({ "code": "rate_limited", "retryAfterSeconds": 60 }),
        )
    }

    /// A fresh id in boat's format, which `github::parse_runner_name` checks.
    fn new_id(&mut self) -> String {
        const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
        self.next_id += 1;
        let mut n = self.next_id;
        let mut id = String::from("bx_");
        for _ in 0..8 {
            id.push(ALPHABET[n as usize % ALPHABET.len()] as char);
            n /= ALPHABET.len() as u64;
        }
        id
    }

    fn sandbox(&mut self, id: &str) -> Option<&mut Sandbox> {
        self.sandboxes.iter_mut().find(|s| s.id == id)
    }

    fn boat(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        let now = self.now;
        let parts: Vec<&str> = path.split(['/', '?']).collect();
        match (method, parts.as_slice()) {
            ("GET", ["limits"]) => {
                let price = self.next_start_cents().map(|c| c as f64 / 100.0);
                let [minute, hour, day] = std::array::from_fn(|i| {
                    let left = START_WINDOWS[i].1.saturating_sub(self.window_starts[i]);
                    json!({ "remaining": left })
                });
                (
                    200,
                    json!({ "canStart": true, "nextStartDollars": price,
                            "starts": { "minute": minute, "hour": hour, "day": day } }),
                )
            }
            ("GET", ["sandboxes", _]) => {
                let list: Vec<Value> = self
                    .sandboxes
                    .iter()
                    .map(|s| {
                        let state = match s.state {
                            State::Booting { .. } => "provisioning",
                            State::Up => "running",
                            State::Archiving { .. } => "archiving",
                            State::Archived { .. } => "archived",
                        };
                        json!({ "id": s.id, "state": state, "labels": s.labels })
                    })
                    .collect();
                (
                    200,
                    json!({ "sandboxes": list, "pageInfo": { "nextCursor": null, "hasMore": false } }),
                )
            }
            ("POST", ["sandboxes"]) => {
                if !self.count_start() {
                    return Self::refusal();
                }
                let id = self.new_id();
                self.sandboxes.push(Sandbox {
                    id: id.clone(),
                    labels: body["labels"].clone(),
                    state: State::Booting {
                        until: now + BOOT_S,
                    },
                });
                (200, json!({ "sandbox": { "id": id, "state": "init" } }))
            }
            ("POST", ["sandboxes", id, "resume"]) => {
                let Some(State::Archived { at }) = self.sandbox(id).map(|s| s.state) else {
                    return (409, json!({ "code": "invalid_state" }));
                };
                if now - at >= KEEP_S && !self.count_start() {
                    return Self::refusal();
                }
                let until = now + BOOT_S;
                self.sandbox(id).expect("found above").state = State::Booting { until };
                (200, json!({}))
            }
            ("POST", ["sandboxes", id, "stop"]) => {
                assert!(
                    !self.runners.iter().any(|r| r.machine == *id),
                    "stopped a machine that still has a runner"
                );
                let until = now + TICK_S;
                match self.sandbox(id) {
                    Some(s) => s.state = State::Archiving { until },
                    None => return (404, json!({})),
                }
                (202, json!({}))
            }
            ("DELETE", ["sandboxes", id]) => {
                assert!(
                    !self.runners.iter().any(|r| r.machine == *id),
                    "deleted a machine that still has a runner"
                );
                self.sandboxes.retain(|s| s.id != *id);
                (200, json!({}))
            }
            // The jit config file and the command that starts the runner.
            ("PUT", ["sandboxes", _, "files"]) | ("POST", ["sandboxes", _, "commands"]) => {
                (200, json!({}))
            }
            _ => panic!("boat mock: {method} {path}"),
        }
    }

    fn github(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        let now = self.now;
        let queued = self.jobs[..self.arrived]
            .iter()
            .filter(|j| j.started.is_none());
        match (
            method,
            path.split(['/', '?']).collect::<Vec<_>>().as_slice(),
        ) {
            ("GET", ["runners", query]) => {
                let page = if query.ends_with("page=1") { 0 } else { 1 };
                let runners: Vec<Value> = self
                    .runners
                    .iter()
                    .skip(page * 100)
                    .take(100)
                    .map(|r| {
                        let status = if r.online_at <= now {
                            "online"
                        } else {
                            "offline"
                        };
                        json!({ "id": r.id, "name": r.name, "status": status,
                                "busy": r.busy_until.is_some() })
                    })
                    .collect();
                (200, json!({ "runners": runners }))
            }
            // One run per push, listed as queued while any of its jobs is, and none as in
            // progress, so the daemon sees each run once. It reads the first 100.
            ("GET", ["runs", query]) => {
                let mut pushes: Vec<u64> = if query.contains("status=queued") {
                    queued.map(|j| j.push).collect()
                } else {
                    Vec::new()
                };
                // GitHub lists the newest runs first.
                pushes.sort_unstable_by(|a, b| b.cmp(a));
                pushes.dedup();
                pushes.truncate(100);
                let runs: Vec<Value> = pushes.iter().map(|p| json!({ "id": p })).collect();
                (200, json!({ "workflow_runs": runs }))
            }
            ("GET", ["runs", push, "jobs", _]) => {
                let push: u64 = push.parse().expect("run id");
                let jobs: Vec<Value> = queued
                    .filter(|j| j.push == push)
                    .map(|j| {
                        json!({ "id": j.id, "run_attempt": 1, "status": "queued",
                                "labels": ["self-hosted", "bench"] })
                    })
                    .collect();
                (200, json!({ "jobs": jobs }))
            }
            ("POST", ["runners", "generate-jitconfig"]) => {
                let name = body["name"].as_str().expect("runner name").to_string();
                let machine = github::parse_runner_name(&name).expect("sbr name").machine;
                self.next_id += 1;
                self.runners.push(GhRunner {
                    id: self.next_id,
                    name,
                    machine,
                    online_at: now + RUNNER_ONLINE_S,
                    busy_until: None,
                });
                (201, json!({ "encoded_jit_config": "jit" }))
            }
            ("DELETE", ["runners", id]) => {
                let id: u64 = id.parse().expect("runner id");
                // GitHub refuses to delete a runner that is running a job.
                if self
                    .runners
                    .iter()
                    .any(|r| r.id == id && r.busy_until.is_some())
                {
                    return (422, json!({ "message": "runner is busy" }));
                }
                self.runners.retain(|r| r.id != id);
                (204, json!({}))
            }
            _ => panic!("github mock: {method} {path}"),
        }
    }
}

struct Mock(RefCell<World>);

impl Transport for Mock {
    fn send(
        &self,
        method: &str,
        url: &str,
        _: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError> {
        let body: Value = body.map_or(Value::Null, |b| serde_json::from_slice(b).expect("json"));
        let mut world = self.0.borrow_mut();
        let mut headers = Vec::new();
        let (status, answer) = if let Some(path) = url.strip_prefix("https://boat.dev/api/v1/") {
            world.boat(method, path, &body)
        } else if let Some(path) = url.strip_prefix("https://api.github.com/repos/o/r/actions/") {
            // The mock sends no ETags, so every read would count against a real limit. A
            // limit this high keeps the daemon's GitHub request budget out of the numbers.
            headers.push(("x-ratelimit-limit".to_string(), "1000000000".to_string()));
            world.github(method, path, &body)
        } else {
            panic!("mock: {method} {url}")
        };
        Ok(Response {
            status,
            headers,
            body: answer.to_string().into_bytes(),
        })
    }
}

struct Row {
    setup: &'static str,
    target: u64,
    jobs: usize,
    unserved: usize,
    starts: u64,
    paid_starts: u64,
    start_cents: u64,
    machine_hours: f64,
    queue_p50: u64,
    queue_p90: u64,
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    match sorted.len() {
        0 => 0,
        n => sorted[(n * p).div_ceil(100).max(1) - 1],
    }
}

fn row(setup: &'static str, target: u64, jobs: &[Job], world: &World) -> Row {
    let day: Vec<&Job> = jobs.iter().filter(|j| j.created >= DAY_START).collect();
    // A job that never got a runner waited until the simulation ended.
    let end = DAY_START + DAY_S + DRAIN_S;
    let mut queue: Vec<u64> = day
        .iter()
        .map(|j| j.started.unwrap_or(end) - j.created)
        .collect();
    queue.sort_unstable();
    Row {
        setup,
        target,
        jobs: day.len(),
        unserved: day.iter().filter(|j| j.started.is_none()).count(),
        starts: world.starts,
        paid_starts: world.paid_starts,
        start_cents: world.start_cents,
        machine_hours: world.machine_s as f64 / 3_600.0,
        queue_p50: percentile(&queue, 50),
        queue_p90: percentile(&queue, 90),
    }
}

/// `runs-on: boat`: every job starts its own machine, which is gone when the job ends.
fn managed(target: u64, jobs: &[Job]) -> Row {
    let mut jobs = jobs.to_vec();
    let mut world = World::default();
    for job in &mut jobs {
        world.advance(job.created);
        // With extra starts on, a job past the most boat allows fails and is not retried.
        if !world.count_start() {
            continue;
        }
        job.started = Some(job.created + BOOT_S);
        if world.in_day() {
            // Cut off at midnight, as the daemon's machine time is.
            let end = (job.created + BOOT_S + job.run_s).min(DAY_START + DAY_S);
            world.machine_s += end - job.created;
        }
    }
    row("runs-on: boat", target, &jobs, &world)
}

/// The daemon's own loop, one tick every 15 s, against the mocked boat and GitHub.
fn daemon(setup: &'static str, pool: &str, target: u64, jobs: &[Job]) -> Row {
    let text = format!(
        "repo = \"o/r\"\n[[pool]]\nname = \"ci\"\nlabels = [\"self-hosted\", \"bench\"]\n\
         machine_type = \"large\"\nsnapshot = \"snap\"\n{pool}\n"
    );
    let cfg = policy::parse(&text).expect("bench policy");
    let start = SIM_START;
    let mock = Mock(RefCell::new(World {
        now: start,
        jobs: jobs.to_vec(),
        ..World::default()
    }));
    let mut tick_loop = Loop::new(&cfg, &mock, "key".into(), "token".into(), start);
    for now in (start..DAY_START + DAY_S + DRAIN_S).step_by(TICK_S as usize) {
        mock.0.borrow_mut().advance(now);
        tick_loop.tick(now, decide::decide).expect("tick");
    }
    let world = mock.0.into_inner();
    row(setup, target, &world.jobs, &world)
}

fn dollars(r: &Row) -> (f64, f64) {
    (
        r.start_cents as f64 / 100.0,
        r.machine_hours * MACHINE_CENTS_PER_HOUR / 100.0,
    )
}

fn csv(rows: &[Row]) -> String {
    let mut out = String::from(
        "jobs_per_day,setup,jobs,unserved,starts,paid_starts,start_dollars,\
         machine_hours,machine_dollars,queue_p50_s,queue_p90_s\n",
    );
    for r in rows {
        let (start, machine) = dollars(r);
        writeln!(
            out,
            "{},{},{},{},{},{},{start:.2},{:.1},{machine:.2},{},{}",
            r.target,
            r.setup,
            r.jobs,
            r.unserved,
            r.starts,
            r.paid_starts,
            r.machine_hours,
            r.queue_p50,
            r.queue_p90
        )
        .expect("write to a String");
    }
    out
}

/// Starts per day against jobs per day, one line per setup, on a linear scale.
fn svg(rows: &[Row]) -> String {
    // Categorical slots 1 to 3 of the palette, light and dark.
    const SERIES: [(&str, &str, &str); 3] = [
        ("runs-on: boat", "#2a78d6", "#3987e5"),
        ("warm", "#eb6834", "#d95926"),
        ("hybrid", "#1baf7a", "#199e70"),
    ];
    const W: f64 = 760.0;
    const H: f64 = 430.0;
    let (left, right, top, bottom) = (64.0, 150.0, 64.0, 48.0);
    let max = 5_000.0;
    let x = |v: f64| ((left + v / max * (W - left - right)) * 10.0).round() / 10.0;
    let y = |v: f64| ((H - bottom - v / max * (H - top - bottom)) * 10.0).round() / 10.0;
    let x_end = x(max);

    let (mut light, mut dark) = (String::new(), String::new());
    for (i, (_, l, d)) in SERIES.iter().enumerate() {
        write!(light, ".s{i}{{stroke:{l}}} .f{i}{{fill:{l}}} ").unwrap();
        write!(dark, ".s{i}{{stroke:{d}}} .f{i}{{fill:{d}}} ").unwrap();
    }
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-labelledby="t d" font-family="system-ui, -apple-system, 'Segoe UI', sans-serif">
<title id="t">boat starts per day against CI jobs per day</title>
<desc id="d">Simulated. runs-on: boat makes one start per job; the warm preset stays near 100 starts a day, and hybrid stops at the $100 plan's included starts. bench/results.csv has the numbers.</desc>
<style>
.bg{{fill:#fcfcfb}} .ink{{fill:#0b0b0b}} .dim{{fill:#52514e}} .grid{{stroke:#e4e3df}} .ref{{stroke:#8a8983}}
{light}
@media (prefers-color-scheme: dark){{
.bg{{fill:#1a1a19}} .ink{{fill:#f0efec}} .dim{{fill:#c3c2b7}} .grid{{stroke:#34332f}} .ref{{stroke:#8f8e86}}
{dark}
}}
</style>
<rect class="bg" width="{W}" height="{H}" rx="8"/>
<text class="ink" x="{left}" y="28" font-size="15" font-weight="600">boat starts per day</text>
<text class="dim" x="{left}" y="46" font-size="12">one simulated day per point; the daemon's real loop against a mocked boat and GitHub</text>
"##
    );
    for v in (0..=5).map(|i| f64::from(i) * 1_000.0) {
        writeln!(
            s,
            r#"<line class="grid" x1="{left}" x2="{x_end}" y1="{y0}" y2="{y0}"/>
<text class="dim" x="{tx}" y="{ty}" font-size="11" text-anchor="end">{v}</text>
<text class="dim" x="{xv}" y="{xy}" font-size="11" text-anchor="middle">{v}</text>"#,
            y0 = y(v),
            tx = left - 8.0,
            ty = y(v) + 4.0,
            xv = x(v),
            xy = H - bottom + 18.0,
        )
        .unwrap();
    }
    writeln!(
        s,
        r#"<text class="dim" x="{mid}" y="{by}" font-size="12" text-anchor="middle">CI jobs per day</text>"#,
        mid = x(max / 2.0),
        by = H - 10.0,
    )
    .unwrap();
    // Each plan's included starts a day; the simulated plan is spelled out.
    let simulated = ("$100", INCLUDED_STARTS_PER_DAY);
    for (plan, included) in OTHER_PLANS.into_iter().chain([simulated]) {
        let note = if plan == simulated.0 {
            " (simulated)"
        } else {
            ""
        };
        writeln!(
            s,
            r#"<line class="ref" x1="{left}" x2="{x_end}" y1="{y0}" y2="{y0}" stroke-dasharray="4 4"/>
<text class="dim" x="{x_end}" y="{ly}" font-size="11" text-anchor="end">{plan} plan includes {included} a day{note}</text>"#,
            y0 = y(included as f64),
            ly = y(included as f64) - 6.0,
        )
        .unwrap();
    }
    for (i, (name, _, _)) in SERIES.iter().enumerate() {
        let series: Vec<&Row> = rows.iter().filter(|r| r.setup == *name).collect();
        let points: Vec<(f64, f64)> = series
            .iter()
            .map(|r| (x(r.jobs as f64), y(r.starts as f64)))
            .collect();
        let path: Vec<String> = points
            .iter()
            .map(|(px, py)| format!("{px:.1},{py:.1}"))
            .collect();
        writeln!(
            s,
            r#"<polyline class="s{i}" fill="none" stroke-width="2" stroke-linejoin="round" points="{}"/>"#,
            path.join(" ")
        )
        .unwrap();
        for (px, py) in &points {
            writeln!(
                s,
                r#"<circle class="f{i}" cx="{px:.1}" cy="{py:.1}" r="3"/>"#
            )
            .unwrap();
        }
        let ((px, py), last) = (
            points.last().expect("series"),
            series.last().expect("series"),
        );
        writeln!(
            s,
            r#"<text class="ink" x="{tx:.1}" y="{ty:.1}" font-size="12">{name}: {starts}</text>"#,
            tx = px + 10.0,
            ty = py + 4.0,
            starts = last.starts,
        )
        .unwrap();
    }
    s.push_str("</svg>\n");
    s
}

/// `bench <jobs per day> <pool fields>` runs one setup and prints its CSV row, writing no
/// files. A literal `\n` in the fields separates lines.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [target, pool] = args.as_slice() {
        let target = target.parse().expect("jobs per day must be a number");
        let jobs = workload(target, 7);
        let row = daemon("custom", &pool.replace("\\n", "\n"), target, &jobs);
        print!("{}", csv(std::slice::from_ref(&row)));
        return;
    }
    let mut rows = Vec::new();
    for target in LOADS {
        let jobs = workload(target, 7);
        rows.push(managed(target, &jobs));
        for (setup, pool) in SETUPS {
            rows.push(daemon(setup, pool, target, &jobs));
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bench");
    let table = csv(&rows);
    std::fs::write(dir.join("results.csv"), &table).expect("write results.csv");
    std::fs::write(dir.join("starts.svg"), svg(&rows)).expect("write starts.svg");
    print!("{table}");
    // A change that breaks the mock or the loop shows up here as jobs that never ran.
    let by_setup: BTreeMap<&str, usize> = rows.iter().fold(BTreeMap::new(), |mut m, r| {
        *m.entry(r.setup).or_default() += r.unserved;
        m
    });
    eprintln!("jobs that never got a runner, by setup: {by_setup:?}");
}
