# BoatRunner TLA+ spec

`BoatRunner.tla` models one sw-boat-runner pool: GitHub's job queue and JIT
runners, boat's machines, and the reconciler in `docs/design.md` as `src/decide.rs`
and `src/run.rs` implement it on branch `integrate`. TLC checks it in CI (job `tla`).

Draft: the known-violation traces are filled in from the first CI run.
