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

/// `GET url`, reading at most `cap` body bytes.
pub(crate) fn get(url: &str, headers: &[(&str, String)], cap: usize) -> Result<Reply, String> {
    use http_body_util::BodyExt;
    let mut req = hyper::http::Request::builder().method("GET").uri(url);
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
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
        })
    })
}
