//! The idle clocks of kept machines, kept in a file so a restart does not reset them.
//!
//! Without it every kept machine a restarted daemon first sees counts as busy a moment
//! ago, so it stays up a full `idle_stop_s` more. The file holds one line per kept `Up`
//! machine: its id, when it came up and when it last had a `Busy` or `Registered` runner.
//! Only the stop timer reads these clocks, and a `Stop` still needs GitHub to list no
//! `Busy` or `Registered` runner on the machine, so a stale or wrong file can only stop
//! an idle machine early or late, never one that runs a job.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::model::UnixSecs;

/// The file's name in the state directory.
const FILE_NAME: &str = "idle-clocks.tsv";

/// When a kept machine came up and when it was last busy, as saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Clock {
    pub since: UnixSecs,
    pub last_busy: UnixSecs,
}

/// Where the clocks are saved, if anywhere, and what was written last.
#[derive(Default)]
pub(super) struct IdleFile {
    path: Option<PathBuf>,
    written: String,
    warned: bool,
}

impl IdleFile {
    /// systemd's `StateDirectory=` sets `STATE_DIRECTORY`, colon-separated when there are
    /// several; the first is used. Unset, nothing is saved and a restart resets the clocks.
    pub(super) fn from_env() -> Self {
        Self::at(state_path(FILE_NAME))
    }

    /// Saves to `path`; `None` saves nothing.
    pub(super) fn at(path: Option<PathBuf>) -> Self {
        IdleFile {
            path,
            ..IdleFile::default()
        }
    }

    /// The saved clocks, by machine id. A missing file is empty; a line that does not
    /// parse is skipped.
    pub(super) fn load(&mut self) -> HashMap<String, Clock> {
        let Some(path) = &self.path else {
            return HashMap::new();
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
            Err(e) => {
                eprintln!("warn: {}: {e}; idle clocks start over", path.display());
                return HashMap::new();
            }
        };
        self.written = text.clone();
        parse(&text)
    }

    /// Writes `text` when it differs from the last write, through a temporary file and a
    /// rename so a crash leaves the old file or the new one. A failure is logged once.
    pub(super) fn save(&mut self, text: &str) {
        let Some(path) = &self.path else {
            return;
        };
        if text == self.written {
            return;
        }
        let tmp = path.with_extension("tmp");
        let wrote = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
        match wrote {
            Ok(()) => {
                self.written = text.to_string();
                self.warned = false;
            }
            Err(e) if !self.warned => {
                self.warned = true;
                eprintln!("warn: {}: {e}; idle clocks are not saved", path.display());
            }
            Err(_) => {}
        }
    }
}

/// `name` in the state directory, if there is one.
pub(super) fn state_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("STATE_DIRECTORY")?;
    let first = dir.to_string_lossy().split(':').next()?.to_string();
    (!first.is_empty()).then(|| PathBuf::from(first).join(name))
}

/// One `id\tsince\tlast_busy` line per machine.
pub(super) fn format<'a>(clocks: impl Iterator<Item = (&'a str, Clock)>) -> String {
    let mut lines: Vec<String> = clocks
        .map(|(id, c)| format!("{id}\t{}\t{}\n", c.since, c.last_busy))
        .collect();
    lines.sort();
    lines.concat()
}

fn parse(text: &str) -> HashMap<String, Clock> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?;
            let since = fields.next()?.parse().ok()?;
            let last_busy = fields.next()?.parse().ok()?;
            let clock = Clock { since, last_busy };
            (fields.next().is_none() && crate::boat::is_sandbox_id(id))
                .then(|| (id.to_string(), clock))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sbr-idle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn clocks_survive_a_save_and_load() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let clock = Clock {
            since: 100,
            last_busy: 250,
        };
        let mut file = IdleFile::at(Some(path.clone()));
        assert!(file.load().is_empty());
        file.save(&format([("bx_23456789", clock)].into_iter()));
        let mut again = IdleFile::at(Some(path));
        assert_eq!(again.load().get("bx_23456789"), Some(&clock));
    }

    #[test]
    fn a_bad_line_is_skipped() {
        let text = "bx_23456789\t1\t2\nnot-an-id\t1\t2\nbx_abcdefgh\tx\t2\nbx_abcdefgh\t1\t2\t3\n";
        let clocks = parse(text);
        assert_eq!(clocks.len(), 1);
        assert!(clocks.contains_key("bx_23456789"));
    }

    #[test]
    fn without_a_path_nothing_is_saved() {
        let mut file = IdleFile::at(None);
        file.save("bx_23456789\t1\t2\n");
        assert!(file.load().is_empty());
    }
}
