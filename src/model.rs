//! Types shared by the policy file, `decide`, and the boat and GitHub clients.
//! Names follow docs/design.md and spec/BoatRunner.tla.

use serde::{Deserialize, Serialize};

pub type UnixSecs = u64;
pub type Cents = u32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// `owner/name` of the repository whose jobs the pools serve.
    pub repo: String,
    #[serde(default = "default_tick_s")]
    pub tick_s: u64,
    #[serde(rename = "pool")]
    pub pools: Vec<Pool>,
}

fn default_tick_s() -> u64 {
    15
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Slot,
    Resume,
    Fork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMode {
    /// Registered once, takes job after job.
    Persistent,
    /// Just-in-time config, one job, then the runner exits.
    Jit,
}

/// A pool after its preset has been applied; see `policy::load`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    /// Runner labels; a job goes to this pool when its `runs-on` labels are a subset.
    pub labels: Vec<String>,
    /// boat machine type: small, default, large or xlarge.
    pub machine_type: String,
    /// Saved snapshot that `Fork` starts from.
    pub snapshot: String,
    pub placement: Vec<Placement>,
    pub runner: RunnerMode,
    pub min_up: u32,
    pub max_boxes: u32,
    pub slots_per_box: u32,
    pub idle_stop_s: u64,
    pub min_up_s: u64,
    pub register_timeout_s: u64,
    pub fork_ttl_s: u64,
    pub paid_start_cents_per_day: Cents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobKey {
    pub id: u64,
    pub attempt: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Starting,
    Up,
    Stopping,
    Stopped,
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub pool: String,
    pub state: MachineState,
    /// When the machine entered `state`.
    pub since: UnixSecs,
    /// Last time a runner on it was `Busy` or `Registered`; in memory only.
    pub last_busy: UnixSecs,
    /// The job an ephemeral (forked) machine was made for.
    pub job: Option<JobKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Registered,
    Idle,
    Busy,
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runner {
    /// `sbr-<pool>-<machine>-<slot>-<registered_at>`.
    pub name: String,
    pub pool: String,
    pub machine: String,
    pub slot: u32,
    pub registered_at: UnixSecs,
    pub state: RunnerState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedJob {
    pub key: JobKey,
    pub labels: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub can_start: bool,
    pub next_start_cents: Cents,
    /// Cents paid for starts since 00:00 UTC, by this process's count.
    pub spent_today_cents: Cents,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub now: UnixSecs,
    pub machines: Vec<Machine>,
    pub runners: Vec<Runner>,
    pub queued: Vec<QueuedJob>,
    pub limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Register {
        pool: String,
        machine: String,
        slot: u32,
    },
    Resume {
        machine: String,
    },
    Fork {
        pool: String,
        job: JobKey,
    },
    Stop {
        machine: String,
    },
    Delete {
        machine: String,
    },
    Unregister {
        runner: String,
    },
    RestartRunner {
        machine: String,
        slot: u32,
    },
}
