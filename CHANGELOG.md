# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

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
- A TLA+ model of the daemon's safety and liveness rules, checked with TLC.

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

[0.1.0]: https://github.com/Surely-Works-LLC/sw-boat-runner/releases/tag/v0.1.0
