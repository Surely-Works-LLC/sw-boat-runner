//! The shell command that starts a JIT runner on a machine, and where its config waits.

/// Where the jit config of `slot` waits for the start command; deleted once read.
pub(super) fn jit_path(slot: u32) -> String {
    format!("/home/user/.sbr-jit-{slot}")
}

/// Waits until `/home/user` is off FUSE. boat boots a machine with `/home/user` on a FUSE
/// mount that copies the snapshot in lazily, then swaps the real disk in once the copy
/// is done, and every file there gets a new device and inode. A job that runs across the
/// swap sees its files change identity, so the runner starts only after it. The wait
/// gives up after 300 tries 2 s apart and deletes the jit config at `$f`; `decide` then
/// unregisters the runner that never came online. Without `findmnt` it does not wait.
/// `HOME_WAIT_S` is that bound. A runner that starts the moment the swap happens still
/// fails (`Transport endpoint is not connected`), so the daemon registers only on a
/// machine boat lists as hydrated (`boat::Sandbox::machine_state`), and this wait is the
/// fallback for a boat that does not say.
const WAIT_FOR_HOME: &str = "n=0; \
     while findmnt -no FSTYPE -T /home/user 2>/dev/null | grep -q '^fuse'; do \
     n=$((n+1)); [ \"$n\" -lt 300 ] || { rm -f \"$f\"; exit 1; }; sleep 2; done";
/// The longest `WAIT_FOR_HOME` waits before a runner starts. A `register_timeout_s` at or
/// under it unregisters a runner that is still waiting for the copy, and a fork then gets
/// deleted and forked again.
pub(crate) const HOME_WAIT_S: u64 = 300 * 2;

/// Refuses the start while slot `slot` runs a job, that is while its runner's
/// `Runner.Worker` is alive: the start command's `pkill` would kill the job. `act` leaves
/// a machine alone when GitHub refuses to delete its runner for a running job, so this
/// guards against a daemon told that the slot was free when it was not. The refusal
/// deletes the jit config at `$f`; that runner never comes online, and `decide`
/// unregisters it after `register_timeout_s`.
fn refuse_busy_slot(slot: u32) -> String {
    format!(
        "if pgrep -f \"/home/user/sbr/{slot}/[b]in/Runner.Worker\" >/dev/null; then \
         echo \"sw-boat-runner: slot {slot} runs a job (Runner.Worker alive); not starting a \
         runner over it\" >&2; rm -f \"$f\"; exit 1; fi"
    )
}

/// Starts slot `slot`'s runner from the snapshot's `/home/user/actions-runner`, in its own
/// directory because a runner keeps its state files next to `run.sh`. boat stores every
/// command for 90 days and offers no stdin or env on commands, so the jit config travels
/// as a file the command reads into `ACTIONS_RUNNER_INPUT_JITCONFIG` and then deletes. The
/// `[b]in` pattern keeps `pgrep` and `pkill` from matching this shell's own command line.
/// After `refuse_busy_slot`, the `pkill` meets only a runner with no job, such as one whose
/// registration was deleted while it started. boat's file write takes no mode, so once
/// home is mounted the command makes the file owner-only; a refused chmod does not fail
/// the start.
pub(super) fn start_command(slot: u32) -> String {
    let f = jit_path(slot);
    let refuse_busy = refuse_busy_slot(slot);
    format!(
        "set -eu; f={f}; {WAIT_FOR_HOME}; chmod 600 \"$f\" || true; d=/home/user/sbr/{slot}; \
         {refuse_busy}; pkill -f \"/home/user/sbr/{slot}/[b]in/Runner\" || true; \
         if [ ! -x \"$d/run.sh\" ]; then mkdir -p /home/user/sbr; cp -a /home/user/actions-runner \"$d\"; fi; \
         cd \"$d\"; rm -f .runner .credentials .credentials_rsaparams; \
         ACTIONS_RUNNER_INPUT_JITCONFIG=\"$(cat \"$f\")\"; rm -f \"$f\"; \
         export ACTIONS_RUNNER_INPUT_JITCONFIG; exec ./run.sh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Writes an executable `sh` script `name` with `body` into `dir`.
    fn fake_bin(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&path)
            .status()
            .unwrap();
    }

    /// Runs `WAIT_FOR_HOME` with a fake `findmnt` that reports FUSE for the first `fuse`
    /// calls, and returns whether the script went on, how often it looked, and whether
    /// the jit config file at `$f` is still there.
    fn wait_for_home(fuse: u32) -> (bool, u32, bool) {
        let dir = std::env::temp_dir().join(format!("sbr-wait-{}-{fuse}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, body: &str| fake_bin(&dir, name, body);
        let calls = dir.join("calls");
        let jit = dir.join("jit");
        std::fs::write(&jit, "JITSECRET").unwrap();
        script(
            "findmnt",
            &format!(
                "echo x >> '{c}'; if [ $(wc -l < '{c}') -le {fuse} ]; \
                 then echo fuse.lazyfs; else echo ext4; fi",
                c = calls.display()
            ),
        );
        script("sleep", "true");
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{WAIT_FOR_HOME}; echo started"))
            .env("PATH", path)
            .env("f", &jit)
            .output()
            .unwrap();
        let looked = std::fs::read_to_string(&calls).map_or(0, |c| c.lines().count() as u32);
        let kept = jit.exists();
        std::fs::remove_dir_all(&dir).unwrap();
        (
            String::from_utf8_lossy(&out.stdout).contains("started"),
            looked,
            kept,
        )
    }

    #[test]
    fn runner_waits_for_home_to_leave_fuse() {
        assert_eq!(wait_for_home(0), (true, 1, true));
        assert_eq!(wait_for_home(3), (true, 4, true));
        // A mount that never settles stops the start instead of running a job on it, and
        // leaves no jit config behind.
        assert_eq!(wait_for_home(u32::MAX), (false, 300, false));
        assert!(WAIT_FOR_HOME.contains("-lt 300 ]") && WAIT_FOR_HOME.contains("sleep 2;"));
        assert_eq!(HOME_WAIT_S, 300 * 2);
        assert!(start_command(1).starts_with(&format!(
            "set -eu; f=/home/user/.sbr-jit-1; {WAIT_FOR_HOME}; chmod 600 \"$f\" || true; "
        )));
    }

    /// Runs `script` with every command it may call faked: `findmnt` reports home on disk,
    /// `pgrep` finds a `Runner.Worker` in slot `busy` alone, and `chmod`, `rm` and `pkill`
    /// only record their arguments. Returns the exit code, stdout, stderr and the calls.
    fn run_faked(name: &str, busy: u32, script: &str) -> (Option<i32>, String, String, String) {
        let dir = std::env::temp_dir().join(format!("sbr-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let calls = dir.join("calls");
        let record = format!("echo \"$(basename \"$0\") $*\" >> '{}'", calls.display());
        fake_bin(&dir, "findmnt", "echo ext4");
        for bin in ["chmod", "rm", "pkill"] {
            fake_bin(&dir, bin, &record);
        }
        let worker = format!("/home/user/sbr/{busy}/[b]in/Runner.Worker");
        fake_bin(&dir, "pgrep", &format!("{record}; [ \"$2\" = '{worker}' ]"));
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .output()
            .unwrap();
        let called = std::fs::read_to_string(&calls).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            called,
        )
    }

    /// Slot 1's start command on a machine whose slot 1 runs a job: it fails, says why,
    /// deletes the jit config and kills nothing. Before, its `pkill` killed the job.
    #[test]
    fn start_refuses_a_slot_that_runs_a_job() {
        let (code, _, stderr, calls) = run_faked("busy", 1, &start_command(1));
        assert_eq!(code, Some(1), "{stderr}");
        assert!(
            stderr.contains("slot 1 runs a job (Runner.Worker alive)"),
            "{stderr}"
        );
        assert_eq!(
            calls,
            "chmod 600 /home/user/.sbr-jit-1\n\
             pgrep -f /home/user/sbr/1/[b]in/Runner.Worker\n\
             rm -f /home/user/.sbr-jit-1\n"
        );
    }

    /// A job in another slot does not hold slot 1's start back, and the guard comes before
    /// the `pkill`. The guard runs alone here: the rest of the command would write to
    /// `/home/user`.
    #[test]
    fn start_goes_on_while_only_another_slot_runs_a_job() {
        let guard = refuse_busy_slot(1);
        let script = format!("set -eu; f=/home/user/.sbr-jit-1; {guard}; echo started");
        let (code, stdout, stderr, calls) = run_faked("idle", 2, &script);
        assert_eq!((code, stdout.as_str()), (Some(0), "started\n"), "{stderr}");
        assert_eq!(calls, "pgrep -f /home/user/sbr/1/[b]in/Runner.Worker\n");
        let start = start_command(1);
        assert!(start.find(&guard).unwrap() < start.find("pkill").unwrap());
    }
}
