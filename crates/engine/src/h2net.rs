//! HTTP/2 transport, used when `MAZDA_HTTP2=1`.
//!
//! **Why this exists.** The engine speaks HTTP/1.1 through `ureq` everywhere
//! else. Over HTTP/1.1 every lane is its own TCP connection and its own TLS
//! handshake, and requests on one connection are serialized. A cold
//! `github.com` therefore pays ~1.2 s of handshake per lane and queues ~11
//! asset requests behind each other; measured on this host, a fresh TLS
//! connection answers in 1.58 s where a reused one answers in 0.39 s.
//!
//! One HTTP/2 connection multiplexes all of them: one handshake per origin,
//! every stream in flight at once, HPACK on the headers.
//!
//! **Shape.** The engine's fetch path is synchronous and stays that way. A
//! two-worker tokio runtime is created on first use and `block_on` bridges
//! each call, so callers see a plain blocking function. Nothing else in the
//! engine is async, and the h1 path is untouched: any error here is the
//! caller's signal to fall back to `ureq`.

use std::sync::OnceLock;

pub(crate) struct Reply {
    pub status: u16,
    /// Lowercased names, values as sent.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Where the body actually came from: the last hop of a redirect chain.
    /// Subresource URLs resolve against this, not against the URL that was
    /// asked for, and cookies belong to its host.
    pub final_url: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, name: &str) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

type Connector = hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;
type Client = hyper_util::client::legacy::Client<Connector, http_body_util::Empty<bytes::Bytes>>;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("mazda-h2")
            .enable_all()
            .build()
            .expect("tokio runtime for the HTTP/2 client")
    })
}

fn client() -> &'static Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(crate::net::tls_config_h2())
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            // a page load reuses the pool across its phases; a long idle
            // connection is cheaper to keep than to re-handshake
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build(connector)
    })
}

/// `GET url`, reading at most `cap` body bytes, following redirects.
///
/// `ureq` follows them for the HTTP/1.1 path; hyper does not, so without this
/// `https://google.com` answered with its own 301 page ("301 Moved") instead
/// of `www.google.com`. Chrome's limit is 20 hops; ten is past any real chain
/// and still bounds a redirect loop.
const MAX_HOPS: usize = 10;

pub(crate) fn get(url: &str, headers: &[(&str, String)], cap: usize) -> Result<Reply, String> {
    let mut current = url.to_string();
    // the caller's headers describe the URL it asked for: its `Cookie` and
    // `If-None-Match` are not the next hop's
    let mut hop_headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let mut hops = 0;
    loop {
        let mut reply = get_once(&current, &hop_headers, cap)?;
        reply.final_url = current.clone();
        let Some(next) = next_hop(reply.status, reply.header("location"), &current) else {
            return Ok(reply);
        };
        // a cookie set on a hop belongs to that hop: its response is dropped
        // here, so nobody else can store it (see cookie.rs)
        crate::cookie::store_response(&current, &set_cookies(&reply.headers));
        hops += 1;
        if hops > MAX_HOPS {
            return Err(format!("too many redirects for {url}"));
        }
        current = next;
        hop_headers = hop_headers_for(&hop_headers, &current);
    }
}

/// The URL to follow for a `3xx` with this `Location`, or `None` to answer with
/// the response as it stands (not a redirect, no `Location`, or an unparseable
/// one).
fn next_hop(status: u16, location: Option<&str>, current: &str) -> Option<String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = location?.trim();
    if location.is_empty() {
        return None;
    }
    let mut next = url::Url::parse(current).ok()?.join(location).ok()?;
    // the fragment never reaches the server
    next.set_fragment(None);
    Some(next.to_string())
}

/// The per-URL headers for a hop: the cookie jar matches on host and path, and
/// a conditional tag belongs to the URL it was stored for.
fn hop_headers_for(base: &[(String, String)], url: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = base
        .iter()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("cookie") && !k.eq_ignore_ascii_case("if-none-match")
        })
        .cloned()
        .collect();
    if let Some(c) = crate::cookie::request_header(url) {
        out.push(("Cookie".to_string(), c));
    }
    out
}

fn set_cookies(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.clone())
        .collect()
}

fn get_once(url: &str, headers: &[(String, String)], cap: usize) -> Result<Reply, String> {
    use http_body_util::BodyExt;
    let mut req = hyper::http::Request::builder().method("GET").uri(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let req = req
        .body(http_body_util::Empty::new())
        .map_err(|e| e.to_string())?;
    runtime().block_on(async move {
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            client().request(req),
        )
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut body = resp.into_body();
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| e.to_string())?;
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > cap {
                    break;
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(Reply {
            status,
            headers,
            body: out,
            final_url: url.to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_hop_follows_the_redirect_and_resolves_it() {
        // the exact case a user hits by typing google.com: the apex answers a
        // 301 to www, and hyper does not follow redirects by itself
        assert_eq!(
            next_hop(301, Some("https://www.google.com/"), "https://google.com/").as_deref(),
            Some("https://www.google.com/")
        );
        // a relative Location resolves against the URL that answered it
        assert_eq!(
            next_hop(302, Some("/login"), "https://github.com/settings").as_deref(),
            Some("https://github.com/login")
        );
        // scheme-relative and temporary/permanent preserve-method codes
        assert_eq!(
            next_hop(308, Some("//example.com/a"), "https://example.org/b").as_deref(),
            Some("https://example.com/a")
        );
        // the fragment never reaches the server
        assert_eq!(
            next_hop(307, Some("/next#frag"), "https://example.com/x").as_deref(),
            Some("https://example.com/next")
        );
    }

    #[test]
    fn next_hop_stops_on_anything_that_is_not_a_usable_redirect() {
        assert_eq!(next_hop(200, Some("/elsewhere"), "https://example.com/"), None);
        assert_eq!(next_hop(304, Some("/elsewhere"), "https://example.com/"), None);
        assert_eq!(next_hop(301, None, "https://example.com/"), None);
        assert_eq!(next_hop(301, Some("   "), "https://example.com/"), None);
        // a Location that cannot be resolved leaves the response as it stands
        assert_eq!(next_hop(301, Some("http://[::1"), "https://example.com/"), None);
    }

    #[test]
    fn a_hop_drops_the_previous_urls_cookie_and_etag() {
        // a conditional tag belongs to the URL it was stored for, and the
        // cookie header is rebuilt from the hop's own host
        let base = vec![
            ("Cookie".to_string(), "sid=old".to_string()),
            ("if-none-match".to_string(), "\"abc\"".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        assert_eq!(
            hop_headers_for(&base, "https://redirect.invalid/"),
            vec![("Accept".to_string(), "*/*".to_string())]
        );
    }
}

