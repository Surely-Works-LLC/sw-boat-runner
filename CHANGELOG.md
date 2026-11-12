# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-11-12

Upgrading from 0.2.0:

- `setup` and `install.sh` use a personal token only with `--pat` (or `SBR_PAT=1`); a
  `GITHUB_TOKEN` in the shell is ignored. An install that already runs on a token keeps it.
- Forks now carry the repository. On a boat account that more than one repository uses,
  upgrade every daemon: a 0.2.0 daemon still takes any machine in a pool of its name.
- `install.sh` fails when a release lacks a binary for the host instead of building it.
- `drain` waits until boat has listed no machine for the listing lag (`--settle-s 0` for
  the old behaviour), and `run` finishes its tick on SIGTERM.

### Added

- Each fork carries `sw-boat-runner-daemon=<host>-<pid>-<start time>`. A daemon logs an
  error, once per other daemon, when a machine forked by another daemon for the same
  repository and pool appears while it runs.


- `setup --remote` stops when a boat machine already runs the daemon for the
  repository (labelled `sw-boat-runner-host`, or named `sw-boat-runner <repo>`), and
  `--replace` deletes that machine once `doctor` finds no FAIL on the new one.
- A rerun of `setup` with a stored or given GitHub App checks that the App is installed
  on the policy's repository before it writes anything, and uses the installation
  GitHub names for it.
- The unit `setup` writes sets `TimeoutStopSec=300`, so systemd waits for the daemon to
  finish its tick on SIGTERM.
- `tests/install_sh.rs` runs `install.sh` with fake `curl`, `uname`, `getconf`, `gh` and
  `cargo`; CI runs shellcheck on it.


- `tests/sim`: a stateful fake boat and GitHub over virtual time that the real
  `run::Loop` runs against. boat's machines boot, stop, expire on their TTL and lose their
  disk, behind a listing that lags; GitHub's runners come online only once their JIT
  config is started on an Up machine, and its jobs queue, run, finish or are cancelled.
  Faults: timeouts, lost answers, 429, 5xx, 401/403, crashes between two requests and
  restarts. `tests/sim_props.rs` checks the design's invariants over random policies,
  workloads and fault schedules (`SIM_CASES`, `SIM_SEED`);
  `tests/sim_known_bugs.rs` and `tests/sim_findings.rs` hold one ignored test per open
  bug. `run::Loop::drain` is public so the tests can drive it.

### Changed

- `check` and `run` warn about a `fork_ttl_s` that is not above `register_timeout_s` plus
  300 s; a fork's TTL then may end before its runner comes online.
- `run` warns once about a job fork whose `Busy` runner comes within 10 minutes of
  `fork_ttl_s`, after which boat archives the machine.

- `setup` and `setup --remote` ignore `GITHUB_TOKEN` unless `--pat` is given, and say
  so. They used to refuse to run while it was set. A rerun whose
  `/etc/sw-boat-runner/env` holds a personal token and no App keeps that token, so an
  upgrade of a 0.2.0 `--pat` install stays on it; `--new-app` switches to an App.
- `install.sh` passes `--pat` only with `SBR_PAT=1` or `--pat`, and otherwise unsets
  `GITHUB_TOKEN` and `GH_TOKEN` for `setup`. Before, the `GITHUB_TOKEN` every boat
  machine has (its creator's) made setup install that personal token instead of
  creating an App.
- `install.sh` stops when a release lacks the binary or `SHA256SUMS` (404) or has no
  binary for the platform, instead of installing rustup and building the tag. Only a
  commit, a branch, or glibc older than 2.35 builds with cargo. A `gh` too old for
  `gh attestation` gets a warning instead of a failure.
- `setup --remote` takes a pasted code or redirect address while it waits for the
  browser, for a browser on another computer, instead of only after 15 minutes.
- `run` logs what it does, one stderr line per action that succeeds: `fork <id> (pool
  <pool>): job <id> attempt <n>` or `kept machine, <why>`, `resume`, `stop ...:
  idle_stop`, `delete ...: finished` (or `orphaned`, `ttl`, `logins`, `drain`, ...),
  `register <runner> on <machine>` and `unregister <runner>: <why>`. It logs a `start:`
  line with the version, repository and pools, and a `summary:` line of each pool's
  machines up and runners busy and today's spend (or, until 00:00 UTC after a restart,
  that it is unknown and counted as the cap) at the first tick and then hourly. A
  tick with nothing to do logs nothing; no line carries a secret or jit config. Before,
  `journalctl -u sw-boat-runner -f` showed only errors. `run::Loop::set_log` sends the
  lines elsewhere.

### Fixed

- `doctor` and `run` no longer advise deleting unlabelled machines (made before 0.3.0)
  in a pool the policy does not have. Those are almost always another daemon's on the
  same boat account, so they get one info line per pool ("another daemon on this boat
  account probably owns them; left alone") with no `boat delete` or `drain` command.
  Delete and drain advice stays for machines labelled with this repository in a pool
  the policy no longer has.

- Queued and in-progress runs, and each run's jobs, are read past the first page of 100
  (up to 10 pages, stopping once `total_count` is covered). A matrix of more than 100
  jobs no longer hangs with its later jobs never seen.
- A tick skipped for the GitHub request budget no longer ages the ETag cache, so the
  next observe sends `If-None-Match` and gets free 304s instead of paying for every read.
- GitHub errors are split into permanent and transient. Only a bad or revoked
  credential, a missing permission, an App that is not installed or is refused, and a
  repository that may not have self-hosted runners stop the daemon. Rate limits, 5xx,
  network errors and a JWT refused for clock skew are retried on the next tick instead
  of ending in a systemd restart loop.
- A GitHub App's JWT goes by GitHub's clock, learned from the `Date` of its answers, so
  a clock running ahead or behind no longer has the JWT refused; a refusal for `iat` or
  `exp` resyncs and tries once more.
- A 403 on registering a runner that blames runner policy now says so, and how to allow
  self-hosted runners for the repository.
- Daemons of two repositories with the same pool name on one boat account no longer stop,
  delete and register runners on each other's machines. Forks now carry
  `sw-boat-runner-repo=<owner>/<name>` next to `sw-boat-runner=<pool>`, and a daemon keeps
  only its repository's machines. This holds once every daemon on the boat account runs
  0.3.0: an older daemon filters on the pool label alone and still takes every machine
  with its pool name, so where two repositories share a pool name, upgrade both. Machines
  made before 0.3.0 have no repo label and are adopted, so an upgrade orphans none. boat
  cannot relabel a machine, and a resumed kept machine is never forked again, so they may
  stay; `run` and `doctor` note them in one info line, harmless while one repository uses
  the pool name. They warn, and suggest draining after the upgrade, only when the account
  shows another repository's machines with that pool name or unlabelled machines in a
  pool the policy does not have.
- A pool removed from or renamed in the policy no longer leaks its machines silently:
  `doctor` lists this repository's machines in pools the policy does not have and prints
  the `drain` or `boat delete` command, and `run` warns about them once at start.
- `drain` no longer reports success while boat has yet to list a fork the stopped daemon
  made just before it stopped. It repeats every `tick_s` until boat has listed no machine
  for the request timeout plus a tick (`--settle-s` to change), and fails with machines left
  after `--timeout-s` (default 900).
- `run` handles SIGINT and SIGTERM: it finishes the current tick and exits 0; a second
  signal exits at once. A tick that outlasts the supervisor's stop timeout is still
  killed. `run --drain-on-stop` drains the pools in the same process first.
  The `runners` workflow uses it and waits for the daemon, and runs `drain` only when the
  daemon did not finish.
- An error no tick can fix no longer ends the tick before its cleanup: the tick's other
  unregisters, deletes and stops still run, and only its registers and starts are
  skipped. `run` then tries again after 15 s, doubling to 10 min, instead of exiting at
  once, so a restart loop no longer resets today's spend, closes starts for 90 s and
  deletes every up job fork each time. While it waits it still runs cleanup-only ticks
  (unregisters, deletes, stops) every `tick_s`, and a stop signal ends the wait at once.
  After 30 min of such errors it runs the cleanup `decide` asks for, stops the kept
  machines no runner holds (even when GitHub refuses the runner delete, or the observe),
  and exits.
- A boat 403 to one pool's fork or resume (a `machine_type` the plan does not allow)
  backs off that pool's starts, 60 s doubling to 10 min, and logs an error; other pools go
  on. Before, it stopped the daemon for every pool.
- A `Register` whose file write or start command fails no longer holds its slot and
  covers a job for `register_timeout_s`: the next tick deletes the GitHub runner and
  frees the slot.
- A kept machine holding its creator's logins no longer blocks its pool: it is deleted,
  or stopped in a pool with `resume`, whose resume scrubs the logins, and the pool's kept
  forks back off as job forks do.
- A start that boat charged above the quote `decide` saw (someone else on the account
  took the last included start first) now counts against `paid_start_cents_per_day`: a
  quote read after an accepted start that is above its booked price books the
  difference. Before, the daemon could pay past its cap.
- A resume without an answer is held as in flight, so the next tick does not resume and
  pay for it again.
- The listing-lag, lost-create and `Retry-After` waits count from the request's answer,
  not from the start of the tick, which can be minutes earlier after slow requests.
- A job's fork whose job left the queue (cancelled or re-run) is deleted 60 s after the
  job was last seen queued, instead of after `register_timeout_s` or `fork_ttl_s`. One
  with an `Idle` or `Registered` runner waits until the pool has had no queued job for
  60 s, and while jobs are queued as many forks as there are jobs, that no `Busy` runner
  holds, stay. Its runner is deleted first, so one that just took a job keeps it.


- `setup --remote` waits for `boat ssh` when writing its input fails, and an install
  failure says the machine is kept, may hold the keys in `~/.sbr-setup`, and gives
  `boat delete <id> --yes`.

## [0.2.0] - 2026-11-11

### Added

- `sw-boat-runner setup --remote`, run on a laptop or workstation: it asks the usual
  questions and `BOAT_API_KEY` without echo, creates the GitHub App with the browser
  sent back to a one-shot listener on `127.0.0.1` (`docs/new-app.html` takes a
  `redirect` only of the form `http://127.0.0.1:<port>/callback`; after 15 minutes, or
  with `--paste`, you paste the code), creates a `small` boat machine with no auto-stop,
  and installs the same release there with `install.sh` over `boat ssh`. The keys go
  only through `boat ssh`'s stdin. `--remote --dry-run` prints the plan.
- `setup` offers to build a missing snapshot (or one you call `new`): a temporary `noEnv`
  sandbox gets the latest actions/runner release, checked against its published SHA-256,
  unconfigured, and is checked and saved with the `snapshot` command's code. The sandbox
  is deleted on every path, Ctrl-C included.
- `sw-boat-runner setup`: writes a policy, creates a GitHub App from a manifest (or takes
  a personal token with `--pat`), stores the keys root-only and enables the systemd
  service. `--dry-run` prints the steps. Running it again reuses the App it stored;
  `--new-app` makes another. `examples/always-on/install.sh` now gets the binary and
  runs it. The App comes from a short link to `docs/new-app.html` on GitHub Pages
  (`--offline-link` prints a `data:` URL instead), and the prompts offer the repository
  from `gh` or `origin`, labels `self-hosted, boat` and the saved boat snapshots.
- `sw-boat-runner doctor [policy.toml]`: checks a setup end to end (policy, clock skew
  against GitHub, boat key, each pool's snapshot and its size and logins, GitHub App
  permissions or token, rate limit, the systemd service, each pool's runners) and prints
  `ok`, `warn` or `FAIL` with a fix for each; any FAIL exits non-zero. As root without
  credentials in the environment it reads `/etc/sw-boat-runner/env`. `setup` runs it at
  the end; a FAIL there leaves the setup in place.
- A policy needs only `repo`, `labels` and `snapshot`. `name` comes from the labels,
  `machine_type` defaults to `default` and `max_boxes` to 2. A policy with one pool can
  put its keys at the top level, without `[[pool]]`.
- A `release` workflow: a pushed `v*` tag publishes Linux x86_64 and arm64 (glibc 2.35 or newer)
  and macOS arm64 binaries with `SHA256SUMS` and a build provenance attestation
  (`gh attestation verify`). CI builds the same targets on every pull request.
- `examples/always-on/install.sh` gets the latest release's binary, checked against
  `SHA256SUMS` and, when `gh` is logged in, its attestation, before it runs `setup`. It
  builds with cargo only for a commit or branch named in `SBR_REF`, or for a release
  without a binary for the machine, and then builds that release's tag. Any other
  failure stops it. `SBR_REF` pins a tag or commit.
- The `runners` workflow takes a GitHub App: the `app-id` input and the `app-private-key`
  secret. `RUNNER_ADMIN_TOKEN` is now the opt-in alternative; passing both, or neither,
  fails the job at its first step.

### Changed

- The default preset is `warm`, not `hybrid`. A pool with no `preset` no longer forks a
  machine per job past its kept machines; set `preset = "hybrid"` to keep that.
- The `runners` workflow lists the run's jobs with the run's own token, so the calling
  job must grant `actions: read`. Its `ref` input is required, with no default; set it to
  the same tag or SHA as `uses:`.
- The daemon asks for GitHub App installation tokens with only Administration write and
  Actions read, besides the one repository.

## [0.1.0] - 2026-11-11

First release. sw-boat-runner keeps self-hosted GitHub Actions runners on
[boat](https://boat.dev) machines, so jobs reuse machines instead of paying a start each.

### Added

- The `pool`, `warm`, `fork` and `hybrid` presets over the `slot`, `resume` and `fork`
  placement steps, with `max_boxes`, `min_up`, idle stops and a daily paid-start cap.
- Just-in-time runners only, with the jit config passed as an owner-only file and never in
  a command, URL or log.
- `check`, `plan`, `run` and `drain` commands, plus `--help` and `--version`. `check`
  warns about a `register_timeout_s` that the runner's wait for boat's snapshot copy can
  outlast.
- A reusable `runners` workflow that runs the daemon for one workflow run, and an install
  script for a daemon on a small boat machine.
- A GitHub request budget: on a busy repository the daemon paces its reads to stay
  inside the token's hourly rate limit.
- A TLA+ model of the daemon's safety and liveness rules, checked with TLC.
- GitHub App authentication: `GITHUB_APP_ID`, `GITHUB_APP_PRIVATE_KEY_FILE` and an
  optional `GITHUB_APP_INSTALLATION_ID` instead of `GITHUB_TOKEN`. The daemon mints and
  renews the installation token itself. Setting both modes is an error.

### Fixed

- A resume or create without a definite answer counts its price as spent and closes starts
  for the tick, so a lost answer cannot pass the paid-start cap.
- A price under half a cent no longer rounds to an included start.
- A resumed machine that boat still lists as stopped counts as starting, so it is not
  resumed twice and holds its `max_boxes` box.
- A kept machine left in a pool whose placement no longer has `slot` is deleted instead of
  holding a box until boat's 30-day TTL.
- An error from boat or GitHub that echoes the request no longer carries the API key,
  token or jit config into the log.
- A daemon started just before 00:00 UTC no longer counts every pool's cap as spent for
  the whole next day.
- `register_timeout_s` defaults to 900 s, above the runner's 600 s wait for the snapshot
  copy, and 0 is rejected. `repo` rejects `.` and `..`.
- The GitHub request budget paces itself on the requests actually left until the reset,
  up to the whole limit for an App installation token and a quarter for a personal token,
  and saves up more for a busy tick. A job queued in one of 30 in-progress runs is found
  within 60 s with an App instead of 150 s. "Budget spent" is logged at most once a minute.
- `examples/always-on/install.sh` builds `SBR_REF` (default `main`) instead of a tag that
  did not exist.

### Security

- A runner is registered only on a machine boat reports free of its creator's personal
  logins (`holdsCreatorLogins: false`). A `noEnv` create keeps logins saved in a snapshot
  of a sandbox made without `--no-env`, so a CI job could read them. Such a job fork is
  deleted and the pool's forks back off; a kept machine waits for its next start.
- Resumes send `noEnv`, so boat scrubs owner secrets from the restored disk.

[Unreleased]: https://github.com/Surely-Works-LLC/sw-boat-runner/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/Surely-Works-LLC/sw-boat-runner/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Surely-Works-LLC/sw-boat-runner/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Surely-Works-LLC/sw-boat-runner/releases/tag/v0.1.0
