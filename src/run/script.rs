//! The shell commands that start a JIT runner on a machine and run or cancel an idle
//! warm, and where a runner's config waits.

use crate::model::IdleWarm;

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

/// Ends every process that carries `SBR_WARM` in its environment, except the process id
/// in `$1`: TERM, up to 2 s for them to exit, then KILL every 0.1 s for up to 1 s more.
/// Exits 1 if one is left. Every process a warm starts inherits the variable wherever it
/// moves in process groups and sessions (`setsid`, a double fork), so this finds what
/// a kill of the process group would miss. It must run without the variable itself.
const END_WARM: &str = "s=$1; m() { grep -lzs '^SBR_WARM=' /proc/[0-9]*/environ | cut -d/ -f3 | grep -vx \"$s\"; }; \
     p=$(m); [ -z \"$p\" ] || kill -TERM $p 2>/dev/null; i=0; \
     while p=$(m); [ -n \"$p\" ]; do [ $i -lt 30 ] || exit 1; \
     [ $i -lt 20 ] || kill -KILL $p 2>/dev/null; i=$((i+1)); sleep 0.1; done";

/// Runs a warm, unless `cancel_command` ran for its token first: `cd` to its working
/// directory and run it under `timeout` (TERM, then KILL 2 s later) with its output in
/// `/tmp/sbr-idle-warm.log`, end whatever it left running, and print `finish <status>
/// <seconds>` or `timeout <seconds>`. Arguments: `END_WARM`, the token, cwd, timeout_s,
/// argv. It checks the token with the marker already set, so a cancel either finds this
/// shell or keeps it from starting anything.
const SUPERVISE: &str = "k=$1 t=$2 d=$3 s=$4; shift 4; [ ! -e /tmp/sbr-warm-$t ] || exit 0; \
     umask 077; b=$(date +%s); { cd \"$d\" && timeout -k 2 \"$s\" \"$@\"; } </dev/null >/tmp/sbr-idle-warm.log 2>&1; \
     r=$?; e=$(($(date +%s) - b)); env -u SBR_WARM sh -c \"$k\" sh $$ || exit 1; \
     if [ $e -lt $s ]; then echo \"finish $r $e\"; else echo \"timeout $e\"; fi";

fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Starts `warm` for pool `pool` under `token`, a UUID, with only `HOME`, `PATH`,
/// `SBR_POOL` and the `SBR_WARM` marker in its environment. Kept in boat's command
/// history like every command, so the policy's arguments must hold no secret.
pub(super) fn warm_command(token: &str, pool: &str, warm: &IdleWarm) -> String {
    let argv: Vec<String> = warm.argv.iter().map(|a| quote(a)).collect();
    format!(
        "exec /usr/bin/env -i SBR_WARM={token} HOME=/home/user PATH=/usr/local/bin:/usr/bin:/bin \
         SBR_POOL={} /bin/sh -c {} sh {} {token} {} {} {}",
        quote(pool),
        quote(SUPERVISE),
        quote(END_WARM),
        quote(&warm.cwd),
        warm.timeout_s,
        argv.join(" ")
    )
}

/// Marks `token` cancelled, so a launch boat runs late does nothing, then ends every
/// warm process on the machine. Prints `clear` once none is left.
pub(super) fn cancel_command(token: &str) -> String {
    format!(
        ": > /tmp/sbr-warm-{token} && sh -c {} sh 0 && echo clear",
        quote(END_WARM)
    )
}

/// What `SUPERVISE` printed, as its log line's event and detail; `None` for anything else.
pub(super) fn warm_result(stdout: &str) -> Option<(&'static str, String)> {
    let mut words = stdout.split_whitespace();
    let event = words.next()?;
    let numbers: Vec<u64> = words.map(str::parse).collect::<Result<_, _>>().ok()?;
    match (event, numbers.as_slice()) {
        ("finish", &[status, secs]) => Some(("finish", format!("status {status}, {secs}s"))),
        ("timeout", &[secs]) => Some(("timeout", format!("{secs}s"))),
        _ => None,
    }
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

    /// The warm and cancel commands run here as boat runs them, with `sh -c`. One test,
    /// as a cancel ends every warm process on the machine.
    #[cfg(target_os = "linux")]
    #[test]
    fn warms_end_whole_and_on_time() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let marked = || {
            let out = Command::new("sh")
                .arg("-c")
                .arg("grep -lzs '^SBR_WARM=' /proc/[0-9]*/environ | wc -l")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .unwrap()
        };
        let dir = std::env::temp_dir();
        let warm = |timeout_s: u64, script: &str| IdleWarm {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: dir.display().to_string(),
            interval_s: 600,
            timeout_s,
        };
        let launch = |token: &str, w: &IdleWarm| {
            Command::new("sh")
                .arg("-c")
                .arg(warm_command(token, "ci", w))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        };
        let cancel = |token: &str| {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cancel_command(token))
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let token = |n: u32| format!("test-{}-{n}", std::process::id());
        // Ignores TERM, and one child leaves the session and the process group.
        let stubborn = "trap '' TERM; setsid sh -c 'sleep 60' & sleep 60 & wait";

        let t = token(1);
        let mut child = launch(&t, &warm(600, stubborn));
        let began = Instant::now();
        while marked() < 4 {
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "the warm did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let began = Instant::now();
        assert_eq!(cancel(&t), "clear\n");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(marked(), 0);
        child.wait().unwrap();

        // The time limit ends the same tree, and what the command left behind.
        let began = Instant::now();
        let out = launch(&token(2), &warm(1, stubborn))
            .wait_with_output()
            .unwrap();
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.starts_with("timeout "), "{out:?}");
        assert!(
            began.elapsed() < Duration::from_secs(8),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(marked(), 0);
        assert_eq!(warm_result(&out).unwrap().0, "timeout");

        // A command that exits early has its background processes ended too.
        let out = launch(&token(3), &warm(600, "sleep 60 & exit 3"))
            .wait_with_output()
            .unwrap();
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(warm_result(&out), Some(("finish", "status 3, 0s".into())));
        assert_eq!(marked(), 0);

        // A launch boat carries out after its cancel starts nothing.
        let t = token(4);
        assert_eq!(cancel(&t), "clear\n");
        let out = launch(&t, &warm(600, "sleep 60"))
            .wait_with_output()
            .unwrap();
        assert!(out.stdout.is_empty() && out.status.success());
        assert_eq!(marked(), 0);
        for n in 1..=4 {
            let _ = std::fs::remove_file(format!("/tmp/sbr-warm-{}", token(n)));
        }
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
