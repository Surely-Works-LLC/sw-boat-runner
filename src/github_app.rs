//! GitHub App authentication: an RS256 JWT signed with the app's private key, exchanged
//! for an installation token that the REST client sends in place of a personal token.
//!
//! The key, the JWT and the installation token are credentials: none of them has `Debug`
//! or `Display`, and no error message quotes them or a response body that holds them.

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
use crate::transport::Transport;

/// The JWT says it was issued this long ago, so a clock a little ahead of GitHub's still
/// passes.
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
        })
    }

    /// Reads the key from `path`; errors name the path, never the file's content.
    pub fn from_file(id: String, installation: Option<u64>, path: &Path) -> Result<App, String> {
        let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        App::new(id, installation, &pem).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// An RS256 JWT for the app, valid from a minute before `now` for nine minutes after.
    fn jwt(&self, now: UnixSecs) -> Result<String, Error> {
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
    /// `REFRESH_EARLY_S` of `now`.
    pub(crate) fn token(
        &mut self,
        t: &dyn Transport,
        repo: &str,
        now: UnixSecs,
    ) -> Result<String, Error> {
        if let Some(held) = &self.token {
            if now.saturating_add(REFRESH_EARLY_S) < held.expires {
                return Ok(held.token.clone());
            }
        }
        self.token = None;
        let jwt = self.jwt(now)?;
        let installation = match self.installation {
            Some(id) => id,
            None => {
                let id = installation_id(t, &jwt, repo)?;
                self.installation = Some(id);
                id
            }
        };
        let held = exchange(t, &jwt, installation, repo)?;
        let token = held.token.clone();
        self.token = Some(held);
        Ok(token)
    }

    /// Drops the held token, after GitHub answered it with 401.
    pub(crate) fn forget_token(&mut self) {
        self.token = None;
    }
}

/// `GET /repos/{owner}/{repo}/installation`: the app's installation on the repository.
fn installation_id(t: &dyn Transport, jwt: &str, repo: &str) -> Result<u64, Error> {
    #[derive(Deserialize)]
    struct Out {
        id: u64,
    }
    let url = format!("{API}/repos/{repo}/installation");
    let what = format!("find the app's installation on {repo} (is the app installed there?)");
    let body = call(t, "GET", &url, jwt, None, &what)?;
    let out: Out =
        serde_json::from_slice(&body).map_err(|_| Error::Parse("installation response".into()))?;
    Ok(out.id)
}

/// `POST /app/installations/{id}/access_tokens`, for the policy's repository only.
fn exchange(
    t: &dyn Transport,
    jwt: &str,
    installation: u64,
    repo: &str,
) -> Result<Installation, Error> {
    #[derive(Deserialize)]
    struct Out {
        token: String,
        expires_at: String,
    }
    let url = format!("{API}/app/installations/{installation}/access_tokens");
    let name = repo.split_once('/').map_or(repo, |(_, name)| name);
    let body = json!({ "repositories": [name] });
    let what = format!("get a token for installation {installation}");
    let bytes = call(t, "POST", &url, jwt, Some(&body), &what)?;
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
fn call(
    t: &dyn Transport,
    method: &str,
    url: &str,
    jwt: &str,
    body: Option<&Value>,
    what: &str,
) -> Result<Vec<u8>, Error> {
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
    if (200..300).contains(&resp.status) {
        return Ok(resp.body);
    }
    let message = serde_json::from_slice::<Value>(&resp.body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_string))
        .unwrap_or_default();
    let rate_limited = resp.status == 429
        || (resp.status == 403 && message.to_ascii_lowercase().contains("rate limit"));
    match resp.status {
        401 | 403 | 404 | 422 if !rate_limited => Err(Error::App(format!(
            "could not {what}: {} {message}",
            resp.status
        ))),
        status => Err(Error::Status { status, message }),
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the form GitHub sends, to Unix seconds.
fn parse_utc(s: &str) -> Option<UnixSecs> {
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
        assert_eq!(body, json!({"repositories": ["r"]}));

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
