//! The warms that may outlive this process (docs/design.md#idle-warm), one
//! `machine\ttoken` line each, in the state directory. A warm is added before its launch
//! is sent and removed once the daemon knows it ended, so a restarted daemon cancels
//! every warm a predecessor may have left running before that machine takes a job.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::idle_file::state_path;

#[derive(Default)]
pub(super) struct WarmFile {
    /// `None` keeps the warms in memory only; then no warm is launched.
    path: Option<PathBuf>,
    warms: BTreeMap<String, String>,
}

impl WarmFile {
    /// The file for `repo` in `STATE_DIRECTORY`. An error when the file cannot be read,
    /// as the warms it may name would go uncancelled, and when a pool has `idle_warm`
    /// but there is no state directory.
    pub(super) fn from_env(repo: &str, needed: bool) -> Result<Self, String> {
        let name = format!(
            "idle-warm-{}.tsv",
            repo.to_ascii_lowercase().replace('/', "+")
        );
        match state_path(&name) {
            Some(path) => Self::at(path).map_err(|e| format!("{name}: {e}")),
            None if needed => Err("idle_warm needs STATE_DIRECTORY".into()),
            None => Ok(Self::default()),
        }
    }

    pub(super) fn at(path: PathBuf) -> io::Result<Self> {
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            r => r?,
        };
        let line = |l: &str| l.split_once('\t').map(|(m, t)| (m.into(), t.into()));
        let warms = text.lines().map(line).collect::<Option<_>>();
        let warms = warms.ok_or_else(|| io::Error::other("a line is not machine<TAB>token"))?;
        Ok(WarmFile {
            path: Some(path),
            warms,
        })
    }

    /// The token of each machine's warm.
    pub(super) fn warms(&self) -> &BTreeMap<String, String> {
        &self.warms
    }

    /// Adds `machine`'s warm and saves the file. A warm is launched only once this
    /// returns `Ok`.
    pub(super) fn add(&mut self, machine: &str, token: &str) -> io::Result<()> {
        let mut warms = self.warms.clone();
        warms.insert(machine.into(), token.into());
        let path = self.path.as_deref().ok_or(io::ErrorKind::NotFound)?;
        save(path, &warms)?;
        self.warms = warms;
        Ok(())
    }

    /// Keeps the warms of the machines `keep` says, saving the file if that drops any. A
    /// failed save keeps a stale line, which costs a restarted daemon one cancel.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let before = self.warms.len();
        self.warms.retain(|machine, _| keep(machine));
        if self.warms.len() == before {
            return;
        }
        if let Some(Err(e)) = self.path.as_deref().map(|p| save(p, &self.warms)) {
            eprintln!("warn: idle warm state: {e}");
        }
    }
}

/// Through a synced temporary file and a rename, so a crash leaves the old file or the
/// new one, and the new one is on disk when this returns.
fn save(path: &Path, warms: &BTreeMap<String, String>) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    for (machine, token) in warms {
        writeln!(file, "{machine}\t{token}")?;
    }
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warms_survive_a_restart_until_removed() {
        let dir = std::env::temp_dir().join(format!("sbr-warm-file-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("warms.tsv");
        let mut file = WarmFile::at(path.clone()).unwrap();
        file.add("bx_1", "t1").unwrap();
        file.add("bx_2", "t2").unwrap();
        file.retain(|m| m != "bx_1");
        let read = WarmFile::at(path.clone()).unwrap();
        assert_eq!(
            read.warms(),
            &BTreeMap::from([("bx_2".into(), "t2".into())])
        );
        // Unreadable warms must stop the daemon, not be forgotten.
        fs::write(&path, "bx_3\n").unwrap();
        assert!(WarmFile::at(path).is_err());
        // Without a state directory nothing is launched.
        assert!(WarmFile::default().add("bx_1", "t1").is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
