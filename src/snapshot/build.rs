//! Builds a runner snapshot from nothing: a temporary `noEnv` sandbox from boat's base
//! image, the latest actions/runner release extracted unconfigured at
//! `/home/user/actions-runner`, then the same checks and save as `snapshot`. The
//! temporary sandbox is deleted on every path, a Ctrl-C included.
//!
//! A fresh `noEnv` sandbox holds no creator logins, so it is checked and saved as it is,
//! without the fork `snapshot` makes of a sandbox that may hold some.

use serde::Deserialize;

use super::*;
use crate::boat::NewSandbox;

const RELEASES: &str = "https://api.github.com/repos/actions/runner/releases/latest";
const DOWNLOAD: &str = "https://github.com/actions/runner/releases/download/";
const CACHE: &str = "/home/user/ci-cache";

/// The runner release asset for one architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunnerAsset {
    pub version: String,
    pub url: String,
    /// The SHA-256 GitHub publishes for the asset, when it publishes one.
    pub sha256: Option<String>,
}

/// actions/runner's name for the architecture `uname -m` prints.
pub(crate) fn runner_arch(uname: &str) -> Option<&'static str> {
    match uname.trim() {
        "x86_64" | "amd64" => Some("x64"),
        "aarch64" | "arm64" => Some("arm64"),
        _ => None,
    }
}

/// The `actions-runner-linux-<arch>-<version>.tar.gz` asset of a `releases/latest`
/// answer, with its SHA-256 from the asset's `digest` or the release notes' `BEGIN SHA`
/// marker. Everything returned goes into a shell script, so it is checked strictly.
pub(crate) fn runner_asset(body: &[u8], arch: &str) -> Result<RunnerAsset, String> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        body: Option<String>,
        assets: Vec<Asset>,
    }
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
        #[serde(default)]
        digest: Option<String>,
    }
    let r: Release = serde_json::from_slice(body)
        .map_err(|e| format!("the actions/runner release was not understood: {e}"))?;
    let version = r
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&r.tag_name)
        .to_string();
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err(format!("odd actions/runner version {:?}", r.tag_name));
    }
    let name = format!("actions-runner-linux-{arch}-{version}.tar.gz");
    let asset = r
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| format!("the actions/runner release {} has no {name}", r.tag_name))?;
    let url = format!("{DOWNLOAD}{}/{name}", r.tag_name);
    if asset.browser_download_url != url {
        return Err(format!(
            "unexpected download URL {:?} for {name}",
            asset.browser_download_url
        ));
    }
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let from_digest = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    let from_notes = r.body.as_deref().and_then(|body| {
        let begin = format!("<!-- BEGIN SHA linux-{arch} -->");
        let end = format!("<!-- END SHA linux-{arch} -->");
        let rest = &body[body.find(&begin)? + begin.len()..];
        Some(rest[..rest.find(&end)?].trim().to_ascii_lowercase())
    });
    let sha256 = match (from_digest, from_notes) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "{name}: the asset's digest and the release notes give different SHA-256s"
            ));
        }
        (Some(s), _) | (None, Some(s)) => Some(s),
        (None, None) => None,
    };
    if let Some(s) = &sha256 {
        if !hex(s) {
            return Err(format!("{name}: odd SHA-256 {s:?}"));
        }
    }
    Ok(RunnerAsset {
        version,
        url,
        sha256,
    })
}

/// Downloads, checks and extracts the runner, installs its dependencies, and makes the
/// cache directory, with an `.env` that keeps tool caches out of `~/.cache`, which boat
/// does not save. Not configured: the daemon registers just-in-time runners.
pub(crate) fn install_script(asset: &RunnerAsset) -> String {
    let check = match &asset.sha256 {
        Some(sha) => format!("echo '{sha}  /tmp/sbr-runner.tar.gz' | sha256sum --strict -c -\n"),
        None => String::new(),
    };
    format!(
        "set -eu
mkdir -p {RUNNER} {CACHE}/xdg
curl --proto '=https' -fsSL -o /tmp/sbr-runner.tar.gz '{url}'
{check}tar -xzf /tmp/sbr-runner.tar.gz -C {RUNNER}
rm -f /tmp/sbr-runner.tar.gz
[ -e {RUNNER}/.env ] || printf 'XDG_CACHE_HOME={CACHE}/xdg\\n' > {RUNNER}/.env
cd {RUNNER}
if ! sudo -n env DEBIAN_FRONTEND=noninteractive ./bin/installdependencies.sh > /tmp/sbr-deps.log 2>&1; then
  tail -n 40 /tmp/sbr-deps.log >&2
  exit 1
fi
exit 0
",
        url = asset.url
    )
}

/// Builds and saves the snapshot `name` for `machine_type`; `Ok(true)` when it was saved.
pub(crate) fn build(
    t: &dyn Transport,
    key: &str,
    name: &str,
    machine_type: &str,
    io: &mut dyn Io,
) -> Result<bool, String> {
    let boat = Client::new(t, key.to_string());
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let spec = NewSandbox {
        machine_type: machine_type.to_string(),
        ttl_s: Some(FORK_TTL_S),
        no_env: true,
        labels: BTreeMap::from([(FORK_LABEL, name.to_string()), (RUN_LABEL, run_id.clone())]),
        name: format!("sbr-build-{name}"),
    };
    let idempotency_key = format!("sw-boat-runner-build-{run_id}");
    io.say(&format!(
        "creating a temporary noEnv {machine_type} sandbox from boat's base image"
    ));
    let id = match retry(io, "create sandbox", |_| {
        boat.create_sandbox(&spec, &idempotency_key)
    }) {
        Ok(id) => id,
        Err(e) => {
            delete_labelled(&boat, RUN_LABEL, &run_id, io);
            return Err(e);
        }
    };
    io.say(&format!("sandbox {id}"));
    let result = build_in(t, &boat, &id, name, machine_type, io);
    match cleanup_retry(io, "delete sandbox", |_| boat.delete(&id)) {
        Ok(()) => io.say(&format!("deleted the temporary sandbox {id}")),
        Err(e) => io.say(&format!(
            "could not delete the temporary sandbox {id}: {e}; delete it by hand \
             (boat delete {id})"
        )),
    }
    result
}

fn build_in(
    t: &dyn Transport,
    boat: &Client,
    id: &str,
    name: &str,
    machine_type: &str,
    io: &mut dyn Io,
) -> Result<bool, String> {
    wait_until_up(boat, id, io)?;
    let uname = exec(boat, id, "uname -m", io)?;
    let arch = runner_arch(&uname)
        .ok_or_else(|| format!("no actions/runner build for {:?}", uname.trim()))?;
    let asset = latest_runner(t, arch)?;
    io.say(&format!(
        "installing actions/runner {} (linux-{arch}) at {RUNNER}, unconfigured",
        asset.version
    ));
    if asset.sha256.is_none() {
        io.say("WARN  GitHub published no SHA-256 for it; the download is not checked");
    }
    exec(boat, id, &install_script(&asset), io)?;
    let detail = retry(io, "read sandbox", |_| boat.detail(id))?;
    let opts = Options {
        source: id.to_string(),
        name: name.to_string(),
        machine_type: Some(machine_type.to_string()),
        fix: false,
        keep_fork: false,
        yes: false,
    };
    check_and_save(boat, id, &detail, &opts, io)
}

fn latest_runner(t: &dyn Transport, arch: &str) -> Result<RunnerAsset, String> {
    let headers = [
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
        ("User-Agent", "sw-boat-runner"),
    ];
    let resp = t
        .send("GET", RELEASES, &headers, None)
        .map_err(|e| format!("look up the latest actions/runner release: {}", e.0))?;
    if resp.status != 200 {
        return Err(format!(
            "look up the latest actions/runner release: status {}",
            resp.status
        ));
    }
    runner_asset(&resp.body, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40";

    fn release(digest: &str, notes_sha: &str) -> String {
        serde_json::json!({
            "tag_name": "v2.338.2",
            "body": format!("x <!-- BEGIN SHA linux-x64 -->{notes_sha}<!-- END SHA linux-x64 --> y"),
            "assets": [
                {"name": "actions-runner-linux-arm64-2.338.2.tar.gz",
                 "browser_download_url": "https://github.com/actions/runner/releases/download/v2.338.2/actions-runner-linux-arm64-2.338.2.tar.gz"},
                {"name": "actions-runner-linux-x64-2.338.2.tar.gz",
                 "digest": digest,
                 "browser_download_url": "https://github.com/actions/runner/releases/download/v2.338.2/actions-runner-linux-x64-2.338.2.tar.gz"}
            ]
        })
        .to_string()
    }

    #[test]
    fn picks_the_runner_for_the_machine_and_its_sha() {
        assert_eq!(runner_arch("x86_64\n"), Some("x64"));
        assert_eq!(runner_arch("aarch64"), Some("arm64"));
        assert_eq!(runner_arch("riscv64"), None);
        let a = runner_asset(release(&format!("sha256:{SHA}"), SHA).as_bytes(), "x64").unwrap();
        assert_eq!(a.version, "2.338.2");
        assert!(
            a.url
                .ends_with("/v2.338.2/actions-runner-linux-x64-2.338.2.tar.gz")
        );
        assert_eq!(a.sha256.as_deref(), Some(SHA));
        // The notes alone are enough; arm64 has neither here.
        let a = runner_asset(release("", SHA).as_bytes(), "x64").unwrap();
        assert_eq!(a.sha256.as_deref(), Some(SHA));
        let a = runner_asset(release("", SHA).as_bytes(), "arm64").unwrap();
        assert_eq!(a.sha256, None);
        // Two different hashes, or an odd one, stop the build.
        let other = "0".repeat(64);
        assert!(runner_asset(release(&format!("sha256:{other}"), SHA).as_bytes(), "x64").is_err());
        assert!(runner_asset(release("sha256:zz';rm", "").as_bytes(), "x64").is_err());
        let bad_tag = release("", SHA).replace("\"v2.338.2\"", "\"v2;x\"");
        assert!(runner_asset(bad_tag.as_bytes(), "x64").is_err());
    }

    #[test]
    fn the_install_script_checks_the_download_and_leaves_the_runner_unconfigured() {
        let asset = RunnerAsset {
            version: "2.338.2".into(),
            url: format!("{DOWNLOAD}v2.338.2/actions-runner-linux-x64-2.338.2.tar.gz"),
            sha256: Some(SHA.into()),
        };
        let s = install_script(&asset);
        assert!(s.contains(&format!("echo '{SHA}  /tmp/sbr-runner.tar.gz' | sha256sum")));
        assert!(s.contains("tar -xzf /tmp/sbr-runner.tar.gz -C /home/user/actions-runner"));
        assert!(
            s.contains("sudo -n env DEBIAN_FRONTEND=noninteractive ./bin/installdependencies.sh")
        );
        assert!(s.contains("mkdir -p /home/user/actions-runner /home/user/ci-cache/xdg"));
        assert!(s.contains("XDG_CACHE_HOME=/home/user/ci-cache/xdg"));
        assert!(!s.contains("config.sh"));
        let unchecked = install_script(&RunnerAsset {
            sha256: None,
            ..asset
        });
        assert!(!unchecked.contains("sha256sum"));
    }
}
