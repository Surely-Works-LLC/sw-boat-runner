//! The one seam between both clients and the network, so tests run without one.

use std::time::Duration;

pub struct NetworkError(pub String);

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

fn find_header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Sends one request. Both clients go through it, so tests swap in `fake::Fake`.
pub trait Transport {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError>;
}

pub(crate) struct Ureq(ureq::Agent);

impl Ureq {
    pub(crate) fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Both clients map every status themselves.
            .http_status_as_error(false)
            .build();
        Ureq(ureq::Agent::new_with_config(config))
    }
}

impl Transport for Ureq {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError> {
        let mut req = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let sent = match body {
            Some(b) => req.body(b.to_vec()).map(|r| self.0.run(r)),
            None => req.body(()).map(|r| self.0.run(r)),
        };
        // These errors name the URL at most, never headers or bodies.
        let mut resp = sent
            .map_err(|e| NetworkError(e.to_string()))?
            .map_err(|e| NetworkError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| NetworkError(e.to_string()))?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::{NetworkError, Response, Transport, find_header};

    #[derive(Debug, Clone)]
    pub(crate) struct Sent {
        pub method: String,
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: Option<Vec<u8>>,
    }

    impl Sent {
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            find_header(&self.headers, name)
        }

        pub(crate) fn body_text(&self) -> String {
            String::from_utf8_lossy(self.body.as_deref().unwrap_or_default()).into_owned()
        }
    }

    /// What the network gives back for one request.
    pub(crate) type Reply = Result<Response, NetworkError>;

    /// Answers every request with `reply` and records what was sent.
    pub(crate) struct Fake {
        reply: Box<dyn Fn(&Sent) -> Reply>,
        sent: RefCell<Vec<Sent>>,
    }

    impl Fake {
        pub(crate) fn new(reply: impl Fn(&Sent) -> Response + 'static) -> Self {
            Fake::failing(move |s| Ok(reply(s)))
        }

        /// Like `new`, but `reply` may fail as the network would.
        pub(crate) fn failing(reply: impl Fn(&Sent) -> Reply + 'static) -> Self {
            Fake {
                reply: Box::new(reply),
                sent: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn sent(&self) -> Vec<Sent> {
            self.sent.borrow().clone()
        }
    }

    impl Transport for Fake {
        fn send(
            &self,
            method: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: Option<&[u8]>,
        ) -> Result<Response, NetworkError> {
            let sent = Sent {
                method: method.to_string(),
                url: url.to_string(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.map(<[u8]>::to_vec),
            };
            let resp = (self.reply)(&sent);
            self.sent.borrow_mut().push(sent);
            resp
        }
    }

    pub(crate) fn respond(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }
}
