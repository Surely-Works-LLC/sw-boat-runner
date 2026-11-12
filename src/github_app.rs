//! GitHub App authentication: an RS256 JWT signed with the app's private key, exchanged
//! for an installation token that the REST client sends in place of a personal token.
//!
//! The key, the JWT and the installation token are credentials: none of them has `Debug`
//! or `Display`, and no error message quotes them or a response body that holds them.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::{API, Error};
use crate::model::UnixSecs;
use crate::transport::{Transport, redact};

/// The JWT says it was issued this long before GitHub's time, as GitHub advises, so a
/// clock a little ahead of the last `Date` seen still passes.
const JWT_BACKDATE_S: u64 = 60;
/// The JWT expires this long from now; GitHub refuses one that lives over 10 minutes.
const JWT_LIFE_S: u64 = 540;
/// An installation token, which lasts an hour, is replaced this long before it expires.
pub(crate) const REFRESH_EARLY_S: u64 = 300;

/// A GitHub App installed on the policy's repository.
pub struct App {
    /// The app's ID, or its client ID; the JWT's `iss`.
    id: String,
    /// Looked up from the repository on first use when not configured.
    installation: Option<u64>,
    key: RsaKeyPair,
    rng: SystemRandom,
    token: Option<Installation>,
    /// GitHub's clock minus this one, in seconds, from the last `Date` GitHub sent. The
    /// JWT's `iat` and `exp` and the token's expiry go by GitHub's time, so a clock that
    /// runs ahead or behind is not refused. A `Cell`, so `doctor`'s `&App` calls learn it.
    skew: Cell<i64>,
}

struct Installation {
    token: String,
    expires: UnixSecs,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("id", &self.id)
            .field("installation", &self.installation)
            .finish_non_exhaustive()
    }
}

impl App {
    /// `pem` is the private key GitHub gives the app (PKCS#1), or the same key as PKCS#8.
    pub fn new(id: String, installation: Option<u64>, pem: &[u8]) -> Result<App, String> {
        let key = match PrivateKeyDer::from_pem_slice(pem) {
            Ok(PrivateKeyDer::Pkcs1(k)) => RsaKeyPair::from_der(k.secret_pkcs1_der()),
            Ok(PrivateKeyDer::Pkcs8(k)) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()),
            // The parser's error may quote a line of the file, so it is not passed on.
            _ => return Err("not a PEM RSA private key".into()),
        }
        .map_err(|e| format!("RSA private key rejected ({e})"))?;
        if id.trim().is_empty() {
            return Err("empty app id".into());
        }
        Ok(App {
            id: id.trim().to_string(),
            installation,
            key,
            rng: SystemRandom::new(),
            token: None,
            skew: Cell::new(0),
        })
    }

    /// Reads the key from `path`; errors name the path, never the file's content.
    pub fn from_file(id: String, installation: Option<u64>, path: &Path) -> Result<App, String> {
        let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        App::new(id, installation, &pem).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `now` on GitHub's clock, as far as the last `Date` it sent tells.
    fn github_now(&self, now: UnixSecs) -> UnixSecs {
        now.saturating_add_signed(self.skew.get())
    }

    /// Learns GitHub's clock from the `Date` of an answer received at `now`, which must be
    /// the time the answer arrived, not when its tick began: a slow request would read as
    /// skew.
    pub(crate) fn note_date(&self, now: UnixSecs, date: Option<&str>) {
        let Some(server) = date.and_then(parse_http_date) else {
            return;
        };
        let (Ok(server), Ok(local)) = (i64::try_from(server), i64::try_from(now)) else {
            return;
        };
        self.skew.set(server - local);
    }

    /// An RS256 JWT for the app, valid from a minute before `now` for nine minutes after,
    /// on GitHub's clock.
    fn jwt(&self, now: UnixSecs) -> Result<String, Error> {
        let now = self.github_now(now);
        let iss = match self.id.parse::<u64>() {
            Ok(n) => json!(n),
            Err(_) => json!(self.id),
        };
        let claims = json!({
            "iat": now.saturating_sub(JWT_BACKDATE_S),
            "exp": now.saturating_add(JWT_LIFE_S),
            "iss": iss,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &self.rng, signed.as_bytes(), &mut sig)
            .map_err(|_| Error::App("could not sign the app's JWT".into()))?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    /// The installation token, minted again when none is held or it expires within
    /// `REFRESH_EARLY_S` of `now` (on GitHub's clock). A JWT GitHub refuses for its time
    /// is signed again once on the clock of GitHub's answer.
    #[cfg(test)]
    pub(crate) fn token(
        &mut self,
        t: &dyn Transport,
        repo: &str,
        now: UnixSecs,
    ) -> Result<String, Error> {
        self.token_at(t, repo, &|| now)
    }

    /// `token` with `now` read at each step, so a JWT is signed, and each answer's `Date`
    /// compared, at the time it happens.
    pub(crate) fn token_at(
        &mut self,
        t: &dyn Transport,
        repo: &str,
        now: &dyn Fn() -> UnixSecs,
    ) -> Result<String, Error> {
        if let Some(held) = &self.token {
            if self.github_now(now()).saturating_add(REFRESH_EARLY_S) < held.expires {
                return Ok(held.token.clone());
            }
        }
        self.token = None;
        let skew = self.skew.get();
        let held = match self.mint(t, repo, now) {
            Err(Error::Clock(_)) if self.skew.get() != skew => self.mint(t, repo, now),
            r => r,
        }?;
        let token = held.token.clone();
        self.token = Some(held);
        Ok(token)
    }

    fn mint(
        &mut self,
        t: &dyn Transport,
        repo: &str,
        now: &dyn Fn() -> UnixSecs,
    ) -> Result<Installation, Error> {
        let jwt = self.jwt(now())?;
        let installation = match self.installation {
            Some(id) => id,
            None => {
                let id = self.installation_id(t, &jwt, repo, now)?;
                self.installation = Some(id);
                id
            }
        };
        self.exchange(t, &jwt, installation, repo, now)
    }

    /// One look for the app's installation on `repo`, for `setup`'s wait: `Ok(None)` while
    /// GitHub answers 404 (not installed there yet), `Error::Status` for any other refusal.
    pub fn installation_on(
        &self,
        t: &dyn Transport,
        repo: &str,
        now: UnixSecs,
    ) -> Result<Option<u64>, Error> {
        #[derive(Deserialize)]
        struct Out {
            id: u64,
        }
        let jwt = self.jwt(now)?;
        let url = format!("{API}/repos/{repo}/installation");
        let (status, body) = self.send(t, "GET", &url, &jwt, None, &|| now)?;
        match status {
            200..=299 => serde_json::from_slice::<Out>(&body)
                .map(|out| Some(out.id))
                .map_err(|_| Error::Parse("installation response".into())),
            404 => Ok(None),
            status => Err(Error::Status {
                status,
                message: message(&body, &jwt),
            }),
        }
    }

    /// The installation set in `GITHUB_APP_INSTALLATION_ID` or found on first use.
    pub(crate) fn installation(&self) -> Option<u64> {
        self.installation
    }

    /// `GET /app/installations/{id}`: what the installation was granted, such as
    /// `administration: write`, for `doctor`.
    pub(crate) fn installation_permissions(
        &self,
        t: &dyn Transport,
        installation: u64,
        now: UnixSecs,
    ) -> Result<BTreeMap<String, String>, Error> {
        #[derive(Deserialize)]
        struct Out {
            #[serde(default)]
            permissions: BTreeMap<String, String>,
        }
        let jwt = self.jwt(now)?;
        let url = format!("{API}/app/installations/{installation}");
        let what = format!("read installation {installation}");
        let body = self.call(t, "GET", &url, &jwt, None, &what, &|| now)?;
        serde_json::from_slice::<Out>(&body)
            .map(|out| out.permissions)
            .map_err(|_| Error::Parse("installation response".into()))
    }

    /// The installation token held now, if any.
    pub(crate) fn held_token(&self) -> Option<&str> {
        self.token.as_ref().map(|held| held.token.as_str())
    }

    /// Drops the held token, after GitHub answered it with 401.
    pub(crate) fn forget_token(&mut self) {
        self.token = None;
    }

    /// `GET /repos/{owner}/{repo}/installation`: the app's installation on the repository.
    fn installation_id(
        &self,
        t: &dyn Transport,
        jwt: &str,
        repo: &str,
        now: &dyn Fn() -> UnixSecs,
    ) -> Result<u64, Error> {
        #[derive(Deserialize)]
        struct Out {
            id: u64,
        }
        let url = format!("{API}/repos/{repo}/installation");
        let what = format!("find the app's installation on {repo} (is the app installed there?)");
        let body = self.call(t, "GET", &url, jwt, None, &what, now)?;
        let out: Out = serde_json::from_slice(&body)
            .map_err(|_| Error::Parse("installation response".into()))?;
        Ok(out.id)
    }

    /// `POST /app/installations/{id}/access_tokens`, for the policy's repository only and
    /// only the permissions the daemon uses, whatever else the app was granted.
    fn exchange(
        &self,
        t: &dyn Transport,
        jwt: &str,
        installation: u64,
        repo: &str,
        now: &dyn Fn() -> UnixSecs,
    ) -> Result<Installation, Error> {
        #[derive(Deserialize)]
        struct Out {
            token: String,
            expires_at: String,
        }
        let url = format!("{API}/app/installations/{installation}/access_tokens");
        let name = repo.split_once('/').map_or(repo, |(_, name)| name);
        let body = json!({
            "repositories": [name],
            "permissions": { "administration": "write", "actions": "read" },
        });
        let what = format!("get a token for installation {installation}");
        let bytes = self.call(t, "POST", &url, jwt, Some(&body), &what, now)?;
        // serde errors can quote the input; this input holds the token.
        let out: Out = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Parse("installation token response".into()))?;
        let expires = parse_utc(&out.expires_at)
            .ok_or_else(|| Error::Parse(format!("expires_at {:?}", out.expires_at)))?;
        Ok(Installation {
            token: out.token,
            expires,
        })
    }

    /// One app-authenticated call. These count against the app's own limit, not the
    /// installation token's, so they stay out of the client's request budget.
    ///
    /// A 401 for the JWT's time claims is `Error::Clock`, which a later try on GitHub's
    /// clock fixes; any other 401, 403, 404 or 422 is `Error::App`, which it does not.
    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        t: &dyn Transport,
        method: &str,
        url: &str,
        jwt: &str,
        body: Option<&Value>,
        what: &str,
        now: &dyn Fn() -> UnixSecs,
    ) -> Result<Vec<u8>, Error> {
        let (status, body) = self.send(t, method, url, jwt, body, now)?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let message = message(&body, jwt);
        let rate_limited =
            status == 429 || (status == 403 && message.to_ascii_lowercase().contains("rate limit"));
        match status {
            401 if refuses_time(&message) => Err(Error::Clock(format!(
                "GitHub refused the app's JWT for its time ({message}); this clock is {} s \
                 off GitHub's, so the next JWT goes by GitHub's; turn on time sync \
                 (`sudo timedatectl set-ntp true`)",
                self.skew.get()
            ))),
            401 | 403 | 404 | 422 if !rate_limited => {
                Err(Error::App(format!("could not {what}: {status} {message}")))
            }
            status => Err(Error::Status { status, message }),
        }
    }

    /// Sends one request signed with the app's JWT: the status and the body. Learns
    /// GitHub's clock from the answer's `Date`.
    fn send(
        &self,
        t: &dyn Transport,
        method: &str,
        url: &str,
        jwt: &str,
        body: Option<&Value>,
        now: &dyn Fn() -> UnixSecs,
    ) -> Result<(u16, Vec<u8>), Error> {
        let auth = format!("Bearer {jwt}");
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "sw-boat-runner"),
        ];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let bytes = body.map(|b| b.to_string().into_bytes());
        let resp = t
            .send(method, url, &headers, bytes.as_deref())
            .map_err(|e| Error::Network(e.0))?;
        self.note_date(now(), resp.header("date"));
        Ok((resp.status, resp.body))
    }
}

/// The `message` of an error response, with the JWT redacted.
fn message(body: &[u8], jwt: &str) -> String {
    let mut message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_string))
        .unwrap_or_default();
    redact(&mut message, jwt);
    message
}

/// Whether a 401's `message` refuses the JWT's `iat` or `exp`, as GitHub does for a clock
/// that is off its own: "'Issued at' claim ('iat') must be an Integer representing the
/// time that the assertion was issued", "'Expiration time' claim ('exp') is too far in the
/// future", and the like.
fn refuses_time(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    ["'iat'", "'exp'", "issued at", "expiration time", "expired"]
        .iter()
        .any(|claim| m.contains(claim))
}

/// `Wed, 11 Nov 2026 10:00:00 GMT`, the form HTTP's `Date` takes, to Unix seconds.
pub(crate) fn parse_http_date(s: &str) -> Option<UnixSecs> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    let month = MONTHS.iter().position(|m| m == month)? + 1;
    let day: u32 = day.parse().ok()?;
    parse_utc(&format!("{year}-{month:02}-{day:02}T{time}Z"))
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the form GitHub sends, to Unix seconds.
pub(crate) fn parse_utc(s: &str) -> Option<UnixSecs> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<u64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from 1970-01-01 to y-m-d in the proleptic Gregorian calendar.
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::transport::fake::{Fake, respond};
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    /// The test-only key in tests/fixtures, as PKCS#1 PEM.
    pub(crate) fn test_pem() -> String {
        pem("RSA PRIVATE KEY", "github-app-test-key.pkcs1.b64")
    }

    fn pem(label: &str, file: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(file);
        let body = std::fs::read_to_string(path).unwrap();
        format!("-----BEGIN {label}-----\n{body}-----END {label}-----\n")
    }

    pub(crate) fn test_app(installation: Option<u64>) -> App {
        App::new("12345".into(), installation, test_pem().as_bytes()).unwrap()
    }

    fn b64(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn reads_pkcs1_and_pkcs8_keys() {
        test_app(None);
        let p8 = pem("PRIVATE KEY", "github-app-test-key.pkcs8.b64");
        App::new("Iv1.abc".into(), Some(1), p8.as_bytes()).unwrap();
        let err =
            App::new("1".into(), None, b"-----BEGIN RSA PRIVATE KEY-----\nAAAA\n").unwrap_err();
        assert_eq!(err, "not a PEM RSA private key");
        assert!(App::new(" ".into(), None, test_pem().as_bytes()).is_err());
        let missing = App::from_file("1".into(), None, Path::new("/nonexistent/key.pem"));
        assert!(missing.unwrap_err().starts_with("/nonexistent/key.pem: "));
        let debug = format!("{:?}", test_app(Some(7)));
        assert_eq!(debug, r#"App { id: "12345", installation: Some(7), .. }"#);
    }

    #[test]
    fn jwt_is_rs256_signed_and_short_lived() {
        let app = test_app(None);
        let jwt = app.jwt(1_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(b64(parts[0]), json!({"alg": "RS256", "typ": "JWT"}));
        assert_eq!(
            b64(parts[1]),
            json!({"iat": 999_940, "exp": 1_000_540, "iss": 12345})
        );
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, app.key.public());
        let signed = format!("{}.{}", parts[0], parts[1]);
        public.verify(signed.as_bytes(), &sig).unwrap();
        public.verify(b"something else", &sig).unwrap_err();

        let p8 = pem("PRIVATE KEY", "github-app-test-key.pkcs8.b64");
        let client_id = App::new("Iv1.abc".into(), None, p8.as_bytes()).unwrap();
        let jwt = client_id.jwt(1_000_000).unwrap();
        assert_eq!(b64(jwt.split('.').nth(1).unwrap())["iss"], "Iv1.abc");
    }

    const INSTALLATION: &str = "https://api.github.com/repos/o/r/installation";
    const TOKENS: &str = "https://api.github.com/app/installations/77/access_tokens";

    fn app_github() -> Fake {
        Fake::new(|s| match (s.method.as_str(), s.url.as_str()) {
            ("GET", INSTALLATION) => respond(200, &[], r#"{"id":77}"#),
            ("POST", TOKENS) => respond(
                201,
                &[],
                r#"{"token":"ghs_one","expires_at":"2001-09-09T01:46:40Z"}"#,
            ),
            _ => respond(404, &[], r#"{"message":"Not Found"}"#),
        })
    }

    #[test]
    fn looks_up_the_installation_and_caches_the_token() {
        let fake = app_github();
        let mut app = test_app(None);
        // 2001-09-09T01:46:40Z is 1_000_000_000.
        let now = 1_000_000_000 - 3_600;
        assert_eq!(app.token(&fake, "o/r", now).unwrap(), "ghs_one");
        let sent = fake.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].url, INSTALLATION);
        assert_eq!(sent[1].url, TOKENS);
        for s in &sent {
            let jwt = s
                .header("authorization")
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap();
            assert_eq!(b64(jwt.split('.').nth(1).unwrap())["iss"], 12345);
        }
        let body: Value = serde_json::from_slice(sent[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "repositories": ["r"],
                "permissions": {"administration": "write", "actions": "read"},
            })
        );

        // Held until five minutes before it expires; the installation is not looked up again.
        let refresh_at = 1_000_000_000 - REFRESH_EARLY_S;
        app.token(&fake, "o/r", refresh_at - 1).unwrap();
        assert_eq!(fake.sent().len(), 2);
        app.token(&fake, "o/r", refresh_at).unwrap();
        let sent = fake.sent();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[2].url, TOKENS);

        app.forget_token();
        app.token(&fake, "o/r", now).unwrap();
        assert_eq!(fake.sent().len(), 4);
    }

    #[test]
    fn a_configured_installation_skips_the_lookup() {
        let fake = app_github();
        let mut app = test_app(Some(77));
        app.token(&fake, "o/r", 0).unwrap();
        let urls: Vec<String> = fake.sent().into_iter().map(|s| s.url).collect();
        assert_eq!(urls, [TOKENS]);
    }

    #[test]
    fn auth_failures_are_fatal_and_quote_no_credential() {
        let fake = app_github();
        let err = test_app(None).token(&fake, "o/other", 0).unwrap_err();
        assert!(err.is_fatal());
        assert_eq!(
            err.to_string(),
            "GitHub App: could not find the app's installation on o/other (is the app \
             installed there?): 404 Not Found"
        );

        let fake = Fake::new(|_| {
            respond(
                401,
                &[],
                r#"{"message":"A JSON web token could not be decoded"}"#,
            )
        });
        let err = test_app(Some(77)).token(&fake, "o/r", 0).unwrap_err();
        assert!(err.is_fatal());
        let jwt = fake.sent()[0].header("authorization").unwrap().to_string();
        assert!(!err.to_string().contains(&jwt[7..]));

        let fake =
            Fake::new(|_| respond(201, &[], r#"{"token":"ghs_secret","expires_at":"soon"}"#));
        let err = test_app(Some(77)).token(&fake, "o/r", 0).unwrap_err();
        assert!(!err.to_string().contains("ghs_secret"), "{err}");

        let fake = Fake::new(|_| respond(502, &[], ""));
        assert!(
            !test_app(Some(77))
                .token(&fake, "o/r", 0)
                .unwrap_err()
                .is_fatal()
        );
    }

    /// The JWT goes by GitHub's clock once a `Date` showed it, so a clock running ahead
    /// (or behind) is not refused; a 401 for the time claims is transient, others are not.
    #[test]
    fn the_jwt_follows_githubs_clock() {
        let app = test_app(None);
        // GitHub reads 1_000_000 while this clock reads 1_000_300.
        app.note_date(1_000_300, Some("Mon, 12 Jan 1970 13:46:40 GMT"));
        let jwt = app.jwt(1_000_300).unwrap();
        assert_eq!(
            b64(jwt.split('.').nth(1).unwrap()),
            json!({"iat": 999_940, "exp": 1_000_540, "iss": 12345})
        );
        app.note_date(1_000_300, Some("not a date"));
        assert_eq!(app.skew.get(), -300);

        assert!(refuses_time("'Issued at' claim ('iat') must be an Integer"));
        assert!(refuses_time(
            "'Expiration time' claim ('exp') is too far in the future"
        ));
        assert!(!refuses_time("A JSON web token could not be decoded"));
        assert!(!refuses_time("Integration not found"));
        assert_eq!(
            parse_http_date("Wed, 11 Nov 2026 10:00:00 GMT"),
            parse_utc("2026-11-11T10:00:00Z")
        );
        assert_eq!(parse_http_date("Wed, 11 Nov 2026 10:00:00 PST"), None);
    }

    #[test]
    fn parses_githubs_timestamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        assert_eq!(parse_utc("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_utc("2026-11-10T13:05:09Z"), Some(1_794_315_909));
        assert_eq!(parse_utc("2026-11-10T13:05:09+00:00"), None);
        assert_eq!(parse_utc("2026-13-10T13:05:09Z"), None);
    }
}
