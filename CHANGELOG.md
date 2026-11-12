# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

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

[Unreleased]: https://github.com/Surely-Works-LLC/sw-boat-runner/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Surely-Works-LLC/sw-boat-runner/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Surely-Works-LLC/sw-boat-runner/releases/tag/v0.1.0
